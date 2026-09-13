use crate::{
    connect::{decode_frame, encode_end_stream, encode_stream_message, ConnectError},
    filesystem, fsutil,
};
use axum::{
    body::{Body, Bytes},
    http::{header, HeaderMap, StatusCode},
    response::Response,
};
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};
use serde::{Deserialize, Serialize};
use std::{
    io,
    os::fd::AsRawFd,
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::Stream;

static ACTIVE_WATCHERS: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Deserialize)]
struct WatchDirRequest {
    path: String,
}

#[derive(Debug, Serialize)]
struct WatchResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    start: Option<EmptyEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    filesystem: Option<WatchEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    keepalive: Option<EmptyEvent>,
}

#[derive(Debug, Serialize)]
struct WatchEvent {
    name: String,
    #[serde(rename = "type")]
    event_type: String,
}

#[derive(Debug, Serialize)]
struct EmptyEvent {}

pub async fn watch_dir(headers: HeaderMap, body: Bytes) -> Response {
    let request = match decode_request(&body) {
        Ok(request) => request,
        Err(error) => return filesystem_error(StatusCode::BAD_REQUEST, error),
    };
    let username = match filesystem::request_user(&headers) {
        Ok(username) => username,
        Err(error) => return fsutil::fs_error_response(error),
    };
    let path = match filesystem::existing_path(&request.path) {
        Ok(path) => path,
        Err(error) => return fsutil::fs_error_response(error),
    };
    if let Err(error) = filesystem::validate_directory(&path, &username) {
        return fsutil::fs_error_response(error);
    }

    let inotify = match Inotify::init() {
        Ok(inotify) => inotify,
        Err(error) => {
            return filesystem_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
        }
    };
    if let Err(error) = set_nonblocking(&inotify) {
        return filesystem_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
    }
    let watch_descriptor = match inotify.watches().add(
        &path,
        WatchMask::CREATE
            | WatchMask::DELETE
            | WatchMask::MODIFY
            | WatchMask::ATTRIB
            | WatchMask::MOVED_FROM
            | WatchMask::MOVED_TO,
    ) {
        Ok(descriptor) => descriptor,
        Err(error) => {
            return filesystem_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
        }
    };

    let (sender, receiver) = mpsc::channel::<Result<Bytes, io::Error>>(32);
    let _ = send_frame(
        &sender,
        WatchResponse {
            start: Some(EmptyEvent {}),
            filesystem: None,
            keepalive: None,
        },
    )
    .await;
    let (cancel_sender, cancel_receiver) = oneshot::channel();
    ACTIVE_WATCHERS.fetch_add(1, Ordering::SeqCst);
    tokio::spawn(run_watcher(
        inotify,
        watch_descriptor,
        sender,
        cancel_receiver,
    ));

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/connect+json")
        .body(Body::from_stream(WatchResponseStream {
            receiver,
            cancel: Some(cancel_sender),
        }))
        .expect("valid WatchDir stream response")
}

async fn run_watcher(
    mut inotify: Inotify,
    watch_descriptor: WatchDescriptor,
    sender: mpsc::Sender<Result<Bytes, io::Error>>,
    mut cancel: oneshot::Receiver<()>,
) {
    let mut buffer = vec![0u8; 16 * 1024];
    let mut last_event = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _ = &mut cancel => break,
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }

        let changes = match inotify.read_events(&mut buffer) {
            Ok(events) => events
                .filter_map(|event| {
                    let name = event.name?.to_string_lossy().into_owned();
                    let event_type = event_type(event.mask)?;
                    Some((name, event_type))
                })
                .collect::<Vec<_>>(),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Vec::new(),
            Err(error) => {
                send_end_error(&sender, error.to_string()).await;
                break;
            }
        };

        for (name, event_type) in changes {
            last_event = tokio::time::Instant::now();
            if send_frame(
                &sender,
                WatchResponse {
                    start: None,
                    filesystem: Some(WatchEvent { name, event_type }),
                    keepalive: None,
                },
            )
            .await
            .is_err()
            {
                cleanup_watcher(&mut inotify, watch_descriptor);
                ACTIVE_WATCHERS.fetch_sub(1, Ordering::SeqCst);
                return;
            }
        }

        if tokio::time::Instant::now().duration_since(last_event) >= Duration::from_secs(30) {
            if send_frame(
                &sender,
                WatchResponse {
                    start: None,
                    filesystem: None,
                    keepalive: Some(EmptyEvent {}),
                },
            )
            .await
            .is_err()
            {
                break;
            }
            last_event = tokio::time::Instant::now();
        }
    }
    cleanup_watcher(&mut inotify, watch_descriptor);
    ACTIVE_WATCHERS.fetch_sub(1, Ordering::SeqCst);
}

fn cleanup_watcher(inotify: &mut Inotify, watch_descriptor: WatchDescriptor) {
    let _ = inotify.watches().remove(watch_descriptor);
}

fn set_nonblocking(inotify: &Inotify) -> io::Result<()> {
    use nix::fcntl::{fcntl, FcntlArg, OFlag};

    let flags = fcntl(inotify.as_raw_fd(), FcntlArg::F_GETFL)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
    fcntl(inotify.as_raw_fd(), FcntlArg::F_SETFL(flags))
        .map(|_| ())
        .map_err(|error| io::Error::other(error.to_string()))
}

fn event_type(mask: EventMask) -> Option<String> {
    if mask.contains(EventMask::CREATE) {
        Some("EVENT_TYPE_CREATE".to_owned())
    } else if mask.contains(EventMask::DELETE) {
        Some("EVENT_TYPE_REMOVE".to_owned())
    } else if mask.contains(EventMask::MODIFY) {
        Some("EVENT_TYPE_WRITE".to_owned())
    } else if mask.intersects(EventMask::MOVED_FROM | EventMask::MOVED_TO) {
        Some("EVENT_TYPE_RENAME".to_owned())
    } else if mask.contains(EventMask::ATTRIB) {
        Some("EVENT_TYPE_CHMOD".to_owned())
    } else {
        None
    }
}

fn decode_request(body: &[u8]) -> Result<WatchDirRequest, String> {
    let (flags, payload) = decode_frame(body).map_err(|error| error.to_string())?;
    if flags != 0 {
        return Err("WatchDir request must be a regular Connect frame".to_owned());
    }
    serde_json::from_slice(&payload).map_err(|error| error.to_string())
}

async fn send_frame<T: Serialize>(
    sender: &mpsc::Sender<Result<Bytes, io::Error>>,
    value: T,
) -> Result<(), mpsc::error::SendError<Result<Bytes, io::Error>>> {
    let payload = serde_json::to_vec(&value).expect("WatchDir response serializes");
    sender
        .send(Ok(Bytes::from(encode_stream_message(&payload))))
        .await
}

async fn send_end_error(sender: &mpsc::Sender<Result<Bytes, io::Error>>, message: String) {
    let error = ConnectError::new("internal", message);
    let _ = sender
        .send(Ok(Bytes::from(encode_end_stream(Some(error)))))
        .await;
}

struct WatchResponseStream {
    receiver: mpsc::Receiver<Result<Bytes, io::Error>>,
    cancel: Option<oneshot::Sender<()>>,
}

impl Stream for WatchResponseStream {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(context)
    }
}

impl Drop for WatchResponseStream {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

fn filesystem_error(status: StatusCode, message: String) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({"code": "invalid_argument", "message": message}).to_string(),
        ))
        .expect("valid WatchDir error response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    use tokio::time::{sleep, timeout};
    use tokio_stream::StreamExt;

    async fn next_json(
        stream: &mut (impl Stream<Item = Result<Bytes, axum::Error>> + Unpin),
    ) -> serde_json::Value {
        let chunk = timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("WatchDir event timed out")
            .expect("WatchDir stream ended")
            .expect("WatchDir stream failed");
        let (flags, payload) = decode_frame(&chunk).expect("valid Connect frame");
        assert_eq!(flags, 0);
        serde_json::from_slice(&payload).expect("valid WatchDir JSON")
    }

    fn request(path: &std::path::Path) -> Bytes {
        let payload = serde_json::json!({"path": path.to_string_lossy()});
        Bytes::from(encode_stream_message(payload.to_string().as_bytes()))
    }

    #[test]
    fn event_types_match_upstream_filesystem_protocol() {
        assert_eq!(
            event_type(EventMask::CREATE).as_deref(),
            Some("EVENT_TYPE_CREATE")
        );
        assert_eq!(
            event_type(EventMask::DELETE).as_deref(),
            Some("EVENT_TYPE_REMOVE")
        );
        assert_eq!(
            event_type(EventMask::MODIFY).as_deref(),
            Some("EVENT_TYPE_WRITE")
        );
        assert_eq!(
            event_type(EventMask::MOVED_FROM).as_deref(),
            Some("EVENT_TYPE_RENAME")
        );
        assert_eq!(
            event_type(EventMask::ATTRIB).as_deref(),
            Some("EVENT_TYPE_CHMOD")
        );
    }

    #[cfg_attr(not(target_os = "linux"), ignore)]
    #[tokio::test]
    async fn watch_dir_reports_create_remove_and_releases_watcher() {
        let directory = tempdir().unwrap();
        let response = watch_dir(HeaderMap::new(), request(directory.path())).await;
        assert_eq!(response.status(), StatusCode::OK);
        let mut stream = response.into_body().into_data_stream();
        let start = next_json(&mut stream).await;
        assert!(start["start"].is_object());

        let active_before = ACTIVE_WATCHERS.load(Ordering::SeqCst);
        let file = directory.path().join("a.txt");
        fs::write(&file, b"hello").unwrap();
        loop {
            let event = next_json(&mut stream).await;
            if event["filesystem"]["name"] == "a.txt" {
                break;
            }
        }
        fs::remove_file(&file).unwrap();
        loop {
            let event = next_json(&mut stream).await;
            if event["filesystem"]["name"] == "a.txt"
                && event["filesystem"]["type"] == "EVENT_TYPE_REMOVE"
            {
                break;
            }
        }

        drop(stream);
        timeout(Duration::from_secs(2), async {
            loop {
                if ACTIVE_WATCHERS.load(Ordering::SeqCst) <= active_before {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("WatchDir watcher was not released");
    }
}
