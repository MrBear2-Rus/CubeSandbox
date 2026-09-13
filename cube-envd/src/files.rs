use crate::fsutil;
use axum::{
    body::{to_bytes, Body},
    extract::{FromRequest, Multipart, Query, Request},
    http::{header, HeaderMap, StatusCode},
    response::Response,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::{fs, io, path::Path, time::UNIX_EPOCH};

/// Maximum request body accepted by `POST /files`, enforced for raw and
/// multipart uploads (the router installs the same limit as a body cap).
pub(crate) const MAX_FILE_BODY_SIZE: usize = 64 * 1024 * 1024;

#[derive(serde::Serialize)]
struct UploadEntry {
    name: String,
    path: String,
    #[serde(rename = "type")]
    file_type: &'static str,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct FileQuery {
    path: String,
    username: Option<String>,
}

enum FileReadOutcome {
    Read(FileRead),
    TooLarge,
}

struct FileRead {
    data: Vec<u8>,
    last_modified: String,
    modified_seconds: u64,
}

pub async fn get(headers: HeaderMap, Query(query): Query<FileQuery>) -> Response {
    let outcome = match fsutil::run_blocking(move || read_file(query)).await {
        Ok(outcome) => outcome,
        Err(error) => return fsutil::fs_error_response(error),
    };
    match outcome {
        FileReadOutcome::TooLarge => request_error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("file exceeds the {} byte limit", MAX_FILE_BODY_SIZE),
        ),
        FileReadOutcome::Read(read) => build_read_response(&headers, read),
    }
}

fn read_file(query: FileQuery) -> io::Result<FileReadOutcome> {
    let path = fs::canonicalize(&query.path)?;
    let username = query.username.as_deref().unwrap_or("root");
    check_read_permission(&path, username)?;
    let metadata = fs::metadata(&path)?;
    if metadata.len() > MAX_FILE_BODY_SIZE as u64 {
        return Ok(FileReadOutcome::TooLarge);
    }
    let modified_seconds = modified_seconds(&metadata)?;
    let last_modified = format_http_date(modified_seconds);
    let data = fs::read(&path)?;
    Ok(FileReadOutcome::Read(FileRead {
        data,
        last_modified,
        modified_seconds,
    }))
}

fn build_read_response(headers: &HeaderMap, read: FileRead) -> Response {
    if let Some(value) = headers.get(header::IF_MODIFIED_SINCE) {
        if let Ok(value) = value.to_str() {
            if parse_http_date(value).is_some_and(|requested| requested >= read.modified_seconds) {
                return response_with_file_headers(
                    StatusCode::NOT_MODIFIED,
                    &read.last_modified,
                    None,
                    Body::empty(),
                );
            }
        }
    }
    let range = match parse_range(headers.get(header::RANGE), read.data.len()) {
        Ok(range) => range,
        Err(()) => {
            return response_with_file_headers(
                StatusCode::RANGE_NOT_SATISFIABLE,
                &read.last_modified,
                Some(format!("bytes */{}", read.data.len())),
                Body::empty(),
            )
        }
    };
    match range {
        Some((start, end)) => response_with_file_headers(
            StatusCode::PARTIAL_CONTENT,
            &read.last_modified,
            Some(format!("bytes {start}-{end}/{}", read.data.len())),
            Body::from(read.data[start..=end].to_vec()),
        ),
        None => response_with_file_headers(
            StatusCode::OK,
            &read.last_modified,
            None,
            Body::from(read.data),
        ),
    }
}

fn response_with_file_headers(
    status: StatusCode,
    last_modified: &str,
    content_range: Option<String>,
    body: Body,
) -> Response {
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::LAST_MODIFIED, last_modified);
    if let Some(content_range) = content_range {
        builder = builder.header(header::CONTENT_RANGE, content_range);
    }
    builder.body(body).expect("valid file response")
}

fn modified_seconds(metadata: &fs::Metadata) -> io::Result<u64> {
    metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| io::Error::other(error.to_string()))
}

fn format_http_date(seconds: u64) -> String {
    DateTime::<Utc>::from_timestamp(seconds as i64, 0)
        .expect("file modification time is representable as an HTTP date")
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string()
}

fn parse_http_date(value: &str) -> Option<u64> {
    DateTime::parse_from_rfc2822(value)
        .ok()
        .and_then(|date| u64::try_from(date.timestamp()).ok())
}

fn parse_range(
    value: Option<&axum::http::HeaderValue>,
    length: usize,
) -> Result<Option<(usize, usize)>, ()> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| ())?;
    let range = value.strip_prefix("bytes=").ok_or(())?;
    if range.contains(',') || length == 0 {
        return Err(());
    }
    let (start, end) = range.split_once('-').ok_or(())?;
    if start.is_empty() {
        let suffix = end.parse::<usize>().map_err(|_| ())?;
        if suffix == 0 {
            return Err(());
        }
        let start = length.saturating_sub(suffix);
        return Ok(Some((start, length - 1)));
    }
    let start = start.parse::<usize>().map_err(|_| ())?;
    if start >= length {
        return Err(());
    }
    let end = if end.is_empty() {
        length - 1
    } else {
        end.parse::<usize>().map_err(|_| ())?.min(length - 1)
    };
    if start > end {
        return Err(());
    }
    Ok(Some((start, end)))
}

pub async fn post(request: Request) -> Response {
    let query = match request.uri().query() {
        Some(raw_query) => match serde_urlencoded::from_str::<FileQuery>(raw_query) {
            Ok(query) => query,
            Err(error) => {
                return request_error_response(StatusCode::BAD_REQUEST, error.to_string());
            }
        },
        None => {
            return request_error_response(
                StatusCode::BAD_REQUEST,
                "path query parameter is required".to_owned(),
            );
        }
    };
    if query.path.is_empty() {
        return request_error_response(
            StatusCode::BAD_REQUEST,
            "path query parameter is required".to_owned(),
        );
    }
    let username = query.username.as_deref().unwrap_or("root").to_owned();
    if let Err(error) = fsutil::run_blocking({
        let username = username.clone();
        move || fsutil::validate_username(&username)
    })
    .await
    {
        return fsutil::fs_error_response(error);
    }

    let is_multipart = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.starts_with("multipart/form-data"))
        .unwrap_or(false);

    let data = if is_multipart {
        let mut multipart = match Multipart::from_request(request, &()).await {
            Ok(multipart) => multipart,
            Err(error) => {
                return request_error_response(StatusCode::BAD_REQUEST, error.to_string());
            }
        };
        let mut file_data = None;
        loop {
            match multipart.next_field().await {
                Ok(Some(field)) => {
                    if field.name() == Some("file") {
                        match field.bytes().await {
                            Ok(data) => {
                                file_data = Some(data);
                                break;
                            }
                            Err(error) => {
                                return request_error_response(
                                    StatusCode::BAD_REQUEST,
                                    error.to_string(),
                                );
                            }
                        }
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    return request_error_response(StatusCode::BAD_REQUEST, error.to_string());
                }
            }
        }
        match file_data {
            Some(data) => data,
            None => {
                return request_error_response(
                    StatusCode::BAD_REQUEST,
                    "multipart field 'file' is required".to_owned(),
                );
            }
        }
    } else {
        match to_bytes(request.into_body(), MAX_FILE_BODY_SIZE).await {
            Ok(data) => data,
            Err(error) => {
                return request_error_response(StatusCode::PAYLOAD_TOO_LARGE, error.to_string());
            }
        }
    };

    let path = query.path;
    let result = fsutil::run_blocking(move || {
        let path = fsutil::writable_path(&path)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_file(&path, &data)?;
        fsutil::apply_owner(&path, &username)?;
        Ok(UploadEntry {
            name: path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_owned(),
            path: path.to_string_lossy().into_owned(),
            file_type: "file",
        })
    })
    .await;

    match result {
        Ok(entry) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Body::from(
                serde_json::to_string(&[entry]).expect("upload entry is serializable"),
            ))
            .expect("valid upload response"),
        Err(error) => fsutil::fs_error_response(error),
    }
}

fn write_file(path: &Path, data: &[u8]) -> io::Result<()> {
    fs::write(path, data)?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o644))?;
    Ok(())
}

fn check_read_permission(path: &Path, username: &str) -> io::Result<()> {
    fsutil::validate_username(username)?;
    if username.is_empty() || username == "root" {
        return Ok(());
    }
    use std::os::unix::fs::MetadataExt;
    let user = nix::unistd::User::from_name(username)
        .map_err(|error| io::Error::other(error.to_string()))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "user not found"))?;
    let metadata = fs::metadata(path)?;
    let mode = metadata.mode();
    let readable = if metadata.uid() == user.uid.as_raw() {
        mode & 0o400 != 0
    } else if metadata.gid() == user.gid.as_raw() {
        mode & 0o040 != 0
    } else {
        mode & 0o004 != 0
    };
    if !readable {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("user {username} cannot read {}", path.display()),
        ));
    }
    Ok(())
}

fn request_error_response(status: StatusCode, message: String) -> Response {
    fsutil::error_response_with_code(status, "invalid_argument", message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::to_bytes, http::Request};
    use tempfile::tempdir;

    #[tokio::test]
    async fn raw_file_write_and_read_round_trip() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("nested").join("file.txt");
        let request = Request::builder()
            .uri(format!("/files?path={}", path.display()))
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(Body::from("content"))
            .unwrap();
        assert_eq!(post(request).await.status(), StatusCode::OK);

        let response = get(
            HeaderMap::new(),
            Query(FileQuery {
                path: path.to_string_lossy().into_owned(),
                username: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), MAX_FILE_BODY_SIZE)
                .await
                .unwrap(),
            "content"
        );
    }

    #[tokio::test]
    async fn missing_file_returns_not_found() {
        let response = get(
            HeaderMap::new(),
            Query(FileQuery {
                path: "/tmp/cube-envd-file-that-does-not-exist".to_owned(),
                username: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(response.into_body(), MAX_FILE_BODY_SIZE)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["code"],
            "not_found"
        );
    }

    #[tokio::test]
    async fn multipart_file_write_is_supported() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("multipart.txt");
        let boundary = "cube-envd-test-boundary";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"multipart.txt\"\r\nContent-Type: application/octet-stream\r\n\r\ncontent-mp\r\n--{boundary}--\r\n"
        );
        let request = Request::builder()
            .uri(format!("/files?path={}", path.display()))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap();

        assert_eq!(post(request).await.status(), StatusCode::OK);
        assert_eq!(fs::read_to_string(path).unwrap(), "content-mp");
    }

    #[tokio::test]
    async fn write_path_resolves_parent_dot_dot_components() {
        let directory = tempdir().unwrap();
        let direct_path = directory.path().join("escape-check.txt");
        let dot_dot_path = directory
            .path()
            .join("nested")
            .join("..")
            .join("escape-check.txt");
        let request = Request::builder()
            .uri(format!("/files?path={}", dot_dot_path.display()))
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(Body::from("resolved"))
            .unwrap();

        assert_eq!(post(request).await.status(), StatusCode::OK);
        assert_eq!(fs::read_to_string(direct_path).unwrap(), "resolved");
        assert!(!directory.path().join("nested").is_dir());
    }

    #[tokio::test]
    async fn range_response_returns_requested_bytes_and_metadata() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("range.txt");
        fs::write(&path, b"0123456789").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, "bytes=0-3".parse().unwrap());

        let response = get(
            headers,
            Query(FileQuery {
                path: path.to_string_lossy().into_owned(),
                username: None,
            }),
        )
        .await;

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 0-3/10");
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
        assert_eq!(
            to_bytes(response.into_body(), MAX_FILE_BODY_SIZE)
                .await
                .unwrap(),
            "0123"
        );
    }

    #[tokio::test]
    async fn invalid_range_returns_416_with_total_length() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("range-invalid.txt");
        fs::write(&path, b"0123456789").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, "bytes=999999-".parse().unwrap());

        let response = get(
            headers,
            Query(FileQuery {
                path: path.to_string_lossy().into_owned(),
                username: None,
            }),
        )
        .await;

        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */10");
    }

    #[tokio::test]
    async fn matching_if_modified_since_returns_304() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("conditional.txt");
        fs::write(&path, b"unchanged").unwrap();
        let first = get(
            HeaderMap::new(),
            Query(FileQuery {
                path: path.to_string_lossy().into_owned(),
                username: None,
            }),
        )
        .await;
        let last_modified = first.headers()[header::LAST_MODIFIED].clone();
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_MODIFIED_SINCE, last_modified);

        let response = get(
            headers,
            Query(FileQuery {
                path: path.to_string_lossy().into_owned(),
                username: None,
            }),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert!(to_bytes(response.into_body(), MAX_FILE_BODY_SIZE)
            .await
            .unwrap()
            .is_empty());
    }
}
