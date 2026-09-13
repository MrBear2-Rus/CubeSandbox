use crate::connect::{decode_frame, encode_end_stream, encode_stream_message};
use crate::defaults::Defaults;
use crate::termination::{self, CgroupMemoryMonitor, TerminationInfo};
use crate::AppState;
use axum::{
    body::Body,
    http::{header, HeaderMap, StatusCode},
    response::Response,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use std::{convert::Infallible, io, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    sync::mpsc,
    time::{self, Instant},
};
use tokio_stream::{wrappers::ReceiverStream, StreamExt};

const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Deserialize)]
struct ProcessStartRequest {
    process: ProcessConfig,
    #[serde(default, rename = "stdin")]
    _stdin: bool,
}

#[derive(Debug, Deserialize)]
struct ProcessConfig {
    cmd: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    envs: std::collections::HashMap<String, String>,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Debug, Serialize)]
struct ProcessResponse {
    event: ProcessEvent,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProcessEvent {
    #[serde(skip_serializing_if = "Option::is_none")]
    start: Option<StartEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<DataEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end: Option<EndEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    keepalive: Option<EmptyEvent>,
}

#[derive(Debug, Serialize)]
struct StartEvent {
    pid: u32,
}

#[derive(Debug, Default, Serialize)]
struct DataEvent {
    #[serde(skip_serializing_if = "Option::is_none")]
    stdout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stderr: Option<String>,
}

#[derive(Debug, Serialize)]
struct EndEvent {
    #[serde(rename = "exitCode", skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    exited: bool,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    termination: Option<TerminationInfo>,
}

#[derive(Debug, Serialize)]
struct EmptyEvent {}

enum ChildOutput {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    ReaderDone,
}

pub async fn start(state: Arc<AppState>, headers: HeaderMap, body: bytes::Bytes) -> Response {
    let request = match decode_request(&body) {
        Ok(request) => request,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error.to_string()),
    };

    let defaults = state
        .defaults
        .read()
        .expect("defaults lock poisoned")
        .clone();
    let user = match basic_auth_user(&headers, &defaults) {
        Ok(user) => user,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error),
    };
    let timeout = match connect_timeout(&headers) {
        Ok(timeout) => timeout,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error),
    };

    let oom_monitor = CgroupMemoryMonitor::start();
    let child = match spawn_process(request, &user, &defaults) {
        Ok(child) => child,
        Err(error) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };

    let pid = child.id().unwrap_or_default();
    let (sender, receiver) = mpsc::channel::<Result<bytes::Bytes, Infallible>>(32);
    send_frame(
        &sender,
        ProcessResponse {
            event: ProcessEvent {
                start: Some(StartEvent { pid }),
                data: None,
                end: None,
                keepalive: None,
            },
        },
    )
    .await;

    tokio::spawn(run_process(child, sender, timeout, oom_monitor));

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/connect+json")
        .body(Body::from_stream(
            ReceiverStream::new(receiver).map(|item| item),
        ))
        .expect("valid process stream response")
}

fn decode_request(
    body: &[u8],
) -> Result<ProcessStartRequest, Box<dyn std::error::Error + Send + Sync>> {
    let (flags, payload) = decode_frame(body)?;
    if flags != 0 {
        return Err("Process.Start request must be a regular Connect frame".into());
    }
    Ok(serde_json::from_slice(&payload)?)
}

fn basic_auth_user(headers: &HeaderMap, defaults: &Defaults) -> Result<String, String> {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return Ok(defaults.user_or_root());
    };
    let value = value
        .to_str()
        .map_err(|_| "Authorization header is not valid UTF-8".to_owned())?;
    let encoded = value
        .strip_prefix("Basic ")
        .ok_or_else(|| "Authorization must use Basic authentication".to_owned())?;
    let decoded = STANDARD
        .decode(encoded)
        .map_err(|_| "Authorization credentials are not valid base64".to_owned())?;
    let credentials = String::from_utf8(decoded)
        .map_err(|_| "Authorization credentials are not valid UTF-8".to_owned())?;
    let username = credentials
        .split_once(':')
        .map(|(username, _)| username)
        .unwrap_or(credentials.as_str());
    if username.is_empty() {
        Ok(defaults.user_or_root())
    } else {
        Ok(username.to_owned())
    }
}

pub(crate) fn connect_timeout(headers: &HeaderMap) -> Result<Option<Duration>, String> {
    let Some(value) = headers.get("Connect-Timeout-Ms") else {
        return Ok(None);
    };
    let milliseconds = value
        .to_str()
        .map_err(|_| "Connect-Timeout-Ms is not valid UTF-8".to_owned())?
        .parse::<u64>()
        .map_err(|_| "Connect-Timeout-Ms must be an unsigned integer".to_owned())?;
    Ok(Some(Duration::from_millis(milliseconds)))
}

fn spawn_process(
    request: ProcessStartRequest,
    user: &str,
    defaults: &Defaults,
) -> io::Result<Child> {
    let mut command = Command::new(&request.process.cmd);
    command
        .args(&request.process.args)
        .envs(defaults.merged_env_vars(&request.process.envs))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = request.process.cwd.or_else(|| defaults.workdir()) {
        command.current_dir(cwd);
    }

    if user != "root" {
        use nix::unistd::{Gid, Uid, User};
        use std::ffi::CString;

        let account = User::from_name(user)
            .map_err(|error| io::Error::other(format!("lookup user {user}: {error}")))?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("user {user} not found"))
            })?;
        let username = CString::new(user)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "username contains NUL"))?;
        let uid = Uid::from_raw(account.uid.as_raw());
        let gid = Gid::from_raw(account.gid.as_raw());
        unsafe {
            command.pre_exec(move || {
                nix::unistd::initgroups(&username, gid)
                    .map_err(|error| io::Error::other(format!("initgroups failed: {error}")))?;
                nix::unistd::setgid(gid)
                    .map_err(|error| io::Error::other(format!("setgid failed: {error}")))?;
                nix::unistd::setuid(uid)
                    .map_err(|error| io::Error::other(format!("setuid failed: {error}")))?;
                Ok(())
            });
        }
    }

    command.spawn()
}

fn end_event_fields(
    status: std::process::ExitStatus,
    oom_killed: bool,
) -> (i32, bool, String, Option<String>, TerminationInfo) {
    use std::os::unix::process::ExitStatusExt;

    if let Some(signal) = status.signal() {
        let text = format!("signal: {}", termination::legacy_signal_name(signal));
        (
            -1,
            false,
            text.clone(),
            Some(text),
            termination::from_exit_status(status, oom_killed),
        )
    } else {
        let fields = exit_status_fields(status.code().unwrap_or(-1));
        (
            fields.0,
            fields.1,
            fields.2,
            fields.3,
            termination::from_exit_status(status, false),
        )
    }
}

fn exit_status_fields(exit_code: i32) -> (i32, bool, String, Option<String>) {
    let text = format!("exit status {exit_code}");
    (
        exit_code,
        true,
        text.clone(),
        (exit_code != 0).then_some(text),
    )
}

async fn run_process(
    mut child: Child,
    sender: mpsc::Sender<Result<bytes::Bytes, Infallible>>,
    timeout: Option<Duration>,
    oom_monitor: CgroupMemoryMonitor,
) {
    let (output_sender, mut output_receiver) = mpsc::channel(16);
    if let Some(stdout) = child.stdout.take() {
        spawn_reader(stdout, true, output_sender.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_reader(stderr, false, output_sender.clone());
    }
    drop(output_sender);
    let mut readers_done = 0;
    let deadline = timeout.map(|duration| Instant::now() + duration);
    let mut process_done = false;
    let mut keepalive = time::interval(KEEPALIVE_INTERVAL);
    keepalive.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    keepalive.tick().await;
    let mut timeout_sleep = Box::pin(time::sleep(timeout.unwrap_or(Duration::from_secs(86400))));
    let mut timed_out = false;
    let mut status = None;

    loop {
        tokio::select! {
            result = child.wait(), if !process_done => {
                status = Some(result);
                process_done = true;
            }
            Some(item) = output_receiver.recv(), if readers_done < 2 => {
                match item {
                    ChildOutput::ReaderDone => readers_done += 1,
                    output => send_output(&sender, output).await,
                }
            }
            _ = &mut timeout_sleep, if deadline.is_some() && !process_done && !timed_out => {
                let _ = child.start_kill();
                timed_out = true;
            }
            _ = keepalive.tick(), if !process_done => {
                send_frame(&sender, ProcessResponse {
                    event: ProcessEvent { start: None, data: None, end: None, keepalive: Some(EmptyEvent {}) },
                }).await;
            }
        }

        if process_done && readers_done == 2 {
            break;
        }
    }

    let (exit_code, exited, status_text, error, mut termination) = match status {
        Some(Ok(exit_status)) => end_event_fields(exit_status, oom_monitor.was_oom_killed()),
        Some(Err(error)) => (
            -1,
            false,
            error.to_string(),
            Some(error.to_string()),
            termination::TerminationInfo::unknown(),
        ),
        None => {
            let message = "process did not return an exit status".to_owned();
            (
                -1,
                false,
                message.clone(),
                Some(message),
                termination::TerminationInfo::unknown(),
            )
        }
    };
    // The timeout message is cube-envd's own contract (unit-tested); upstream
    // envd surfaces the underlying kill as "signal: killed" instead.
    let error = if timed_out {
        termination = termination::TerminationInfo::timeout();
        Some("process timed out".to_owned())
    } else {
        error
    };
    send_frame(
        &sender,
        ProcessResponse {
            event: ProcessEvent {
                start: None,
                data: None,
                end: Some(EndEvent {
                    exit_code: (!timed_out).then_some(exit_code),
                    exited: !timed_out && exited,
                    status: status_text,
                    error,
                    termination: Some(termination),
                }),
                keepalive: None,
            },
        },
    )
    .await;
    let _ = sender
        .send(Ok(bytes::Bytes::from(encode_end_stream(None))))
        .await;
}

fn spawn_reader<R>(mut reader: R, stdout: bool, sender: mpsc::Sender<ChildOutput>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buffer = vec![0u8; 8192];
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) => break,
                Ok(size) => {
                    let data = buffer[..size].to_vec();
                    let item = if stdout {
                        ChildOutput::Stdout(data)
                    } else {
                        ChildOutput::Stderr(data)
                    };
                    if sender.send(item).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = sender.send(ChildOutput::ReaderDone).await;
    });
}

async fn send_output(sender: &mpsc::Sender<Result<bytes::Bytes, Infallible>>, output: ChildOutput) {
    let mut data = DataEvent::default();
    match output {
        ChildOutput::Stdout(bytes) => data.stdout = Some(STANDARD.encode(bytes)),
        ChildOutput::Stderr(bytes) => data.stderr = Some(STANDARD.encode(bytes)),
        ChildOutput::ReaderDone => return,
    }
    send_frame(
        sender,
        ProcessResponse {
            event: ProcessEvent {
                start: None,
                data: Some(data),
                end: None,
                keepalive: None,
            },
        },
    )
    .await;
}

async fn send_frame<T: Serialize>(
    sender: &mpsc::Sender<Result<bytes::Bytes, Infallible>>,
    value: T,
) {
    if let Ok(payload) = serde_json::to_vec(&value) {
        let _ = sender
            .send(Ok(bytes::Bytes::from(encode_stream_message(&payload))))
            .await;
    }
}

fn error_response(status: StatusCode, message: String) -> Response {
    let payload = serde_json::json!({
        "code": "process_error",
        "message": message,
    });
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(payload.to_string()))
        .expect("valid process error response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn test_state() -> Arc<AppState> {
        Arc::new(AppState::new(49983))
    }

    async fn collect_response(response: Response) -> Vec<serde_json::Value> {
        let mut body = response.into_body().into_data_stream();
        let mut frames = Vec::new();
        while let Some(Ok(chunk)) = body.next().await {
            let (flags, payload) = decode_frame(&chunk).unwrap();
            if flags == 0 {
                frames.push(serde_json::from_slice(&payload).unwrap());
            }
        }
        frames
    }

    #[tokio::test]
    async fn command_stream_contains_start_data_and_end() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Basic cm9vdDo="),
        );
        let body = bytes::Bytes::from(encode_stream_message(
            br#"{"process":{"cmd":"/bin/bash","args":["-c","echo -n hello; echo -n world >&2; exit 42"]},"stdin":false}"#,
        ));
        let frames = collect_response(start(test_state(), headers, body).await).await;
        assert!(
            frames.first().unwrap()["event"]["start"]["pid"]
                .as_u64()
                .unwrap()
                > 0
        );
        let combined = frames[1..frames.len() - 1].iter().fold(
            (String::new(), String::new()),
            |mut output, frame| {
                if let Some(value) = frame["event"]["data"]["stdout"].as_str() {
                    output
                        .0
                        .push_str(&String::from_utf8(STANDARD.decode(value).unwrap()).unwrap());
                }
                if let Some(value) = frame["event"]["data"]["stderr"].as_str() {
                    output
                        .1
                        .push_str(&String::from_utf8(STANDARD.decode(value).unwrap()).unwrap());
                }
                output
            },
        );
        assert_eq!(combined, ("hello".to_owned(), "world".to_owned()));
        assert_eq!(frames.last().unwrap()["event"]["end"]["exitCode"], 42);
        assert_eq!(
            frames.last().unwrap()["event"]["end"]["status"],
            "exit status 42"
        );
        assert_eq!(
            frames.last().unwrap()["event"]["end"]["error"],
            "exit status 42"
        );
        assert_eq!(
            frames.last().unwrap()["event"]["end"]["termination"]["reason"],
            "exited"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn signaled_process_reports_go_style_signal_status() {
        let headers = HeaderMap::new();
        let body = bytes::Bytes::from(encode_stream_message(
            br#"{"process":{"cmd":"/bin/sh","args":["-c","kill -9 $$"]}}"#,
        ));
        let frames = collect_response(start(test_state(), headers, body).await).await;
        let end = &frames.last().unwrap()["event"]["end"];
        assert_eq!(end["exitCode"], -1);
        assert_eq!(end["exited"], false);
        assert_eq!(end["status"], "signal: killed");
        assert_eq!(end["error"], "signal: killed");
        assert_eq!(end["termination"]["reason"], "signal");
        assert_eq!(end["termination"]["signal"], 9);
        assert_eq!(end["termination"]["signalName"], "SIGKILL");
    }

    #[tokio::test]
    async fn timeout_kills_process_and_reports_error() {
        let mut headers = HeaderMap::new();
        headers.insert("Connect-Timeout-Ms", HeaderValue::from_static("10"));
        let body = bytes::Bytes::from(encode_stream_message(
            br#"{"process":{"cmd":"/bin/bash","args":["-c","sleep 2"]}}"#,
        ));
        let frames = collect_response(start(test_state(), headers, body).await).await;
        assert_eq!(
            frames.last().unwrap()["event"]["end"]["error"],
            "process timed out"
        );
        assert!(frames.last().unwrap()["event"]["end"]["exitCode"].is_null());
        assert_eq!(
            frames.last().unwrap()["event"]["end"]["status"],
            "signal: killed"
        );
        assert_eq!(
            frames.last().unwrap()["event"]["end"]["termination"]["reason"],
            "timeout"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn user_auth_runs_process_as_user_account_when_available() {
        if nix::unistd::User::from_name("user").unwrap().is_none() {
            return;
        }

        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Basic dXNlcjo="),
        );
        let body = bytes::Bytes::from(encode_stream_message(
            br#"{"process":{"cmd":"/bin/bash","args":["-c","id -u"]}}"#,
        ));
        let frames = collect_response(start(test_state(), headers, body).await).await;
        let stdout = frames
            .iter()
            .filter_map(|frame| frame["event"]["data"]["stdout"].as_str())
            .map(|value| String::from_utf8(STANDARD.decode(value).unwrap()).unwrap())
            .collect::<String>();
        assert_eq!(stdout.trim(), "1000");
    }
}
