use axum::{
    body::Body,
    extract::Json,
    http::{header, HeaderMap, StatusCode},
    response::Response,
};
use base64::Engine;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, Metadata},
    io,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
    time::{Duration, UNIX_EPOCH},
};

#[derive(Debug, Deserialize)]
pub(crate) struct PathRequest {
    path: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MoveRequest {
    source: String,
    destination: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct FileEntry {
    name: String,
    #[serde(rename = "type")]
    file_type: String,
    path: String,
    size: String,
    mode: u32,
    permissions: String,
    owner: String,
    group: String,
    #[serde(rename = "modifiedTime")]
    modified_time: String,
}

#[derive(Debug, Serialize)]
struct EntriesResponse {
    entries: Vec<FileEntry>,
}

#[derive(Debug, Serialize)]
struct EntryResponse {
    entry: FileEntry,
}

pub async fn list_dir(headers: HeaderMap, Json(request): Json<PathRequest>) -> Response {
    let username = match request_user(&headers) {
        Ok(username) => username,
        Err(error) => return fs_error_response(error),
    };
    let path = match existing_path(&request.path) {
        Ok(path) => path,
        Err(error) => return fs_error_response(error),
    };
    if let Err(error) = validate_directory(&path, &username) {
        return fs_error_response(error);
    }
    let mut entries = Vec::new();
    let directory = match fs::read_dir(&path) {
        Ok(directory) => directory,
        Err(error) => return fs_error_response(error),
    };
    for item in directory {
        let item = match item {
            Ok(item) => item,
            Err(error) => return fs_error_response(error),
        };
        let child_path = match fs::canonicalize(item.path()) {
            Ok(path) => path,
            Err(error) => return fs_error_response(error),
        };
        let metadata = match item.metadata() {
            Ok(metadata) => metadata,
            Err(error) => return fs_error_response(error),
        };
        entries.push(entry_from_meta(&child_path, &metadata));
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    json_response(StatusCode::OK, EntriesResponse { entries })
}

pub async fn stat(headers: HeaderMap, Json(request): Json<PathRequest>) -> Response {
    let username = match request_user(&headers) {
        Ok(username) => username,
        Err(error) => return fs_error_response(error),
    };
    let path = match existing_path(&request.path) {
        Ok(path) => path,
        Err(error) => return fs_error_response(error),
    };
    if let Err(error) = validate_directory_or_readable(&path, &username) {
        return fs_error_response(error);
    }
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) => return fs_error_response(error),
    };
    json_response(
        StatusCode::OK,
        EntryResponse {
            entry: entry_from_meta(&path, &metadata),
        },
    )
}

pub async fn remove(headers: HeaderMap, Json(request): Json<PathRequest>) -> Response {
    let username = match request_user(&headers) {
        Ok(username) => username,
        Err(error) => return fs_error_response(error),
    };
    let path = match existing_path(&request.path) {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return empty_response(),
        Err(error) => return fs_error_response(error),
    };
    if let Err(error) = ensure_can_modify(path.parent().unwrap_or(Path::new("/")), &username) {
        return fs_error_response(error);
    }
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return empty_response(),
        Err(error) => return fs_error_response(error),
    };
    let result = if metadata.is_dir() {
        fs::remove_dir_all(&path)
    } else {
        fs::remove_file(&path)
    };
    match result {
        Ok(()) => empty_response(),
        Err(error) => fs_error_response(error),
    }
}

pub async fn move_entry(headers: HeaderMap, Json(request): Json<MoveRequest>) -> Response {
    let username = match request_user(&headers) {
        Ok(username) => username,
        Err(error) => return fs_error_response(error),
    };
    let source = match existing_path(&request.source) {
        Ok(path) => path,
        Err(error) => return fs_error_response(error),
    };
    let destination = match writable_path(&request.destination) {
        Ok(path) => path,
        Err(error) => return fs_error_response(error),
    };
    if let Err(error) = ensure_can_modify(source.parent().unwrap_or(Path::new("/")), &username)
        .and_then(|_| ensure_can_modify(destination.parent().unwrap_or(Path::new("/")), &username))
    {
        return fs_error_response(error);
    }
    if let Some(parent) = destination.parent() {
        if let Err(error) = fs::create_dir_all(parent) {
            return fs_error_response(error);
        }
    }
    if let Err(error) = fs::rename(&source, &destination) {
        return fs_error_response(error);
    }
    let metadata = match fs::metadata(&destination) {
        Ok(metadata) => metadata,
        Err(error) => return fs_error_response(error),
    };
    json_response(
        StatusCode::OK,
        EntryResponse {
            entry: entry_from_meta(&destination, &metadata),
        },
    )
}

pub async fn make_dir(headers: HeaderMap, Json(request): Json<PathRequest>) -> Response {
    let username = match request_user(&headers) {
        Ok(username) => username,
        Err(error) => return fs_error_response(error),
    };
    let path = match writable_path(&request.path) {
        Ok(path) => path,
        Err(error) => return fs_error_response(error),
    };
    if let Err(error) = ensure_can_modify(path.parent().unwrap_or(Path::new("/")), &username) {
        return fs_error_response(error);
    }
    if let Err(error) = fs::create_dir_all(&path) {
        return fs_error_response(error);
    }
    if let Err(error) = apply_owner(&path, &username) {
        return fs_error_response(error);
    }
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) => return fs_error_response(error),
    };
    json_response(
        StatusCode::OK,
        EntryResponse {
            entry: entry_from_meta(&path, &metadata),
        },
    )
}

fn entry_from_meta(abs_path: &Path, metadata: &Metadata) -> FileEntry {
    let mode = metadata.mode() & 0o7777;
    let file_type = if metadata.is_dir() {
        "FILE_TYPE_DIRECTORY"
    } else {
        "FILE_TYPE_FILE"
    };
    let modified_time = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(rfc3339_from_duration)
        .unwrap_or_else(|| {
            DateTime::<Utc>::from(UNIX_EPOCH).to_rfc3339_opts(SecondsFormat::Millis, true)
        });
    FileEntry {
        name: abs_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("/")
            .to_owned(),
        file_type: file_type.to_owned(),
        path: abs_path.to_string_lossy().into_owned(),
        size: metadata.len().to_string(),
        mode,
        permissions: permission_string(metadata, mode),
        owner: nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(metadata.uid()))
            .ok()
            .flatten()
            .map(|user| user.name)
            .unwrap_or_else(|| metadata.uid().to_string()),
        group: nix::unistd::Group::from_gid(nix::unistd::Gid::from_raw(metadata.gid()))
            .ok()
            .flatten()
            .map(|group| group.name)
            .unwrap_or_else(|| metadata.gid().to_string()),
        modified_time,
    }
}

fn permission_string(metadata: &Metadata, mode: u32) -> String {
    let mut permissions = String::with_capacity(10);
    permissions.push(if metadata.is_dir() { 'd' } else { '-' });
    for (read, write, execute, special_bit, special) in [
        (0o400, 0o200, 0o100, 0o4000, 's'),
        (0o040, 0o020, 0o010, 0o2000, 's'),
        (0o004, 0o002, 0o001, 0o1000, 't'),
    ] {
        permissions.push(if mode & read != 0 { 'r' } else { '-' });
        permissions.push(if mode & write != 0 { 'w' } else { '-' });
        permissions.push(match (mode & execute != 0, mode & special_bit != 0) {
            (true, true) => special,
            (true, false) => 'x',
            (false, true) => special.to_ascii_uppercase(),
            (false, false) => '-',
        });
    }
    permissions
}

pub(crate) fn existing_path(raw_path: &str) -> io::Result<PathBuf> {
    if raw_path.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path is required",
        ));
    }
    fs::canonicalize(raw_path)
}

fn writable_path(raw_path: &str) -> io::Result<PathBuf> {
    let requested = lexical_normalize(Path::new(raw_path));
    if requested.exists() {
        return fs::canonicalize(requested);
    }
    let mut missing = Vec::new();
    let mut current = requested.as_path();
    while !current.exists() {
        let name = current.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "path has no existing parent")
        })?;
        missing.push(name.to_owned());
        current = current.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "path has no existing parent")
        })?;
    }
    let mut resolved = fs::canonicalize(current)?;
    for component in missing.iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            Component::Normal(component) => normalized.push(component),
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
        }
    }
    normalized
}

pub(crate) fn request_user(headers: &HeaderMap) -> io::Result<String> {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return Ok("root".to_owned());
    };
    let value = value
        .to_str()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid Authorization header"))?;
    let encoded = value.strip_prefix("Basic ").ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "Authorization must use Basic")
    })?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid Basic credentials"))?;
    let credentials = String::from_utf8(decoded)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid Basic credentials"))?;
    let username = credentials
        .split_once(':')
        .map(|(name, _)| name)
        .unwrap_or(&credentials);
    let username = if username.is_empty() {
        "root"
    } else {
        username
    };
    validate_username(username)?;
    Ok(username.to_owned())
}

fn validate_username(username: &str) -> io::Result<()> {
    if username == "root" || username.is_empty() {
        return Ok(());
    }
    if nix::unistd::User::from_name(username)
        .map_err(|error| io::Error::other(error.to_string()))?
        .is_none()
    {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("user {username} not found"),
        ));
    }
    Ok(())
}

pub(crate) fn validate_directory(path: &Path, username: &str) -> io::Result<()> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path is not a directory",
        ));
    }
    validate_username(username)
}

fn validate_directory_or_readable(path: &Path, username: &str) -> io::Result<()> {
    validate_username(username)?;
    let metadata = fs::metadata(path)?;
    if metadata.is_dir() && !can_access_directory(&metadata, username)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "directory access denied",
        ));
    }
    Ok(())
}

fn ensure_can_modify(path: &Path, username: &str) -> io::Result<()> {
    validate_username(username)?;
    if username == "root" {
        return Ok(());
    }
    let metadata = fs::metadata(path)?;
    if !can_access_directory(&metadata, username)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "directory modification denied",
        ));
    }
    Ok(())
}

fn can_access_directory(metadata: &Metadata, username: &str) -> io::Result<bool> {
    let user = nix::unistd::User::from_name(username)
        .map_err(|error| io::Error::other(error.to_string()))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "user not found"))?;
    let mode = metadata.mode();
    let owner = metadata.uid() == user.uid.as_raw();
    let group = metadata.gid() == user.gid.as_raw();
    let bits = if owner {
        0o700
    } else if group {
        0o070
    } else {
        0o007
    };
    Ok(mode & bits == bits)
}

fn apply_owner(path: &Path, username: &str) -> io::Result<()> {
    if username == "root" {
        return Ok(());
    }
    let user = nix::unistd::User::from_name(username)
        .map_err(|error| io::Error::other(error.to_string()))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "user not found"))?;
    nix::unistd::chown(path, Some(user.uid), Some(user.gid))
        .map_err(|error| io::Error::other(error.to_string()))
}

fn rfc3339_from_duration(duration: Duration) -> String {
    let timestamp = UNIX_EPOCH + duration;
    DateTime::<Utc>::from(timestamp).to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn json_response<T: Serialize>(status: StatusCode, value: T) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&value).expect("filesystem response serializes"),
        ))
        .expect("valid filesystem response")
}

fn empty_response() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .expect("valid empty filesystem response")
}

pub(crate) fn fs_error_response(error: io::Error) -> Response {
    let (status, code) = match error.kind() {
        io::ErrorKind::NotFound => (StatusCode::NOT_FOUND, "not_found"),
        io::ErrorKind::PermissionDenied => (StatusCode::FORBIDDEN, "permission_denied"),
        io::ErrorKind::InvalidInput => (StatusCode::BAD_REQUEST, "invalid_argument"),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    };
    json_response(
        status,
        serde_json::json!({"code": code, "message": error.to_string()}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use tempfile::tempdir;

    fn root_headers() -> HeaderMap {
        HeaderMap::new()
    }

    #[tokio::test]
    async fn filesystem_rpcs_cover_lifecycle_and_metadata() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("nested");
        let file = path.join("file.txt");
        let headers = root_headers();

        let created = make_dir(
            headers.clone(),
            Json(PathRequest {
                path: path.to_string_lossy().into_owned(),
            }),
        )
        .await;
        assert_eq!(created.status(), StatusCode::OK);
        let created_body = to_bytes(created.into_body(), 64 * 1024).await.unwrap();
        let created_json: serde_json::Value = serde_json::from_slice(&created_body).unwrap();
        assert_eq!(created_json["entry"]["type"], "FILE_TYPE_DIRECTORY");

        fs::write(&file, b"hello").unwrap();
        let listed = list_dir(
            headers.clone(),
            Json(PathRequest {
                path: path.to_string_lossy().into_owned(),
            }),
        )
        .await;
        let listed_body = to_bytes(listed.into_body(), 64 * 1024).await.unwrap();
        let listed_json: serde_json::Value = serde_json::from_slice(&listed_body).unwrap();
        assert_eq!(listed_json["entries"][0]["size"], "5");
        assert_eq!(
            listed_json["entries"][0]["permissions"]
                .as_str()
                .unwrap()
                .len(),
            10
        );

        let moved = move_entry(
            headers.clone(),
            Json(MoveRequest {
                source: file.to_string_lossy().into_owned(),
                destination: path.join("moved.txt").to_string_lossy().into_owned(),
            }),
        )
        .await;
        assert_eq!(moved.status(), StatusCode::OK);
        assert!(path.join("moved.txt").exists());

        let removed = remove(
            headers.clone(),
            Json(PathRequest {
                path: path.to_string_lossy().into_owned(),
            }),
        )
        .await;
        assert_eq!(removed.status(), StatusCode::OK);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn stat_missing_path_returns_not_found() {
        let response = stat(
            root_headers(),
            Json(PathRequest {
                path: "/tmp/cube-envd-missing-file".to_owned(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
