use crate::fsutil;
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
    path::{Path, PathBuf},
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
    let path = request.path;
    match fsutil::run_blocking(move || list_dir_blocking(&headers, &path)).await {
        Ok(entries) => json_response(StatusCode::OK, EntriesResponse { entries }),
        Err(error) => fsutil::fs_error_response(error),
    }
}

fn list_dir_blocking(headers: &HeaderMap, raw_path: &str) -> io::Result<Vec<FileEntry>> {
    let username = request_user(headers)?;
    let path = existing_path(raw_path)?;
    validate_directory(&path, &username)?;
    let mut cache = NameCache::default();
    let mut entries = Vec::new();
    for item in fs::read_dir(&path)? {
        let item = item?;
        let child_path = item.path();
        let metadata = fs::symlink_metadata(&child_path)?;
        entries.push(entry_from_meta(&child_path, &metadata, &mut cache));
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(entries)
}

pub async fn stat(headers: HeaderMap, Json(request): Json<PathRequest>) -> Response {
    let path = request.path;
    match fsutil::run_blocking(move || stat_blocking(&headers, &path)).await {
        Ok(entry) => json_response(StatusCode::OK, EntryResponse { entry }),
        Err(error) => fsutil::fs_error_response(error),
    }
}

fn stat_blocking(headers: &HeaderMap, raw_path: &str) -> io::Result<FileEntry> {
    let username = request_user(headers)?;
    let path = existing_path(raw_path)?;
    validate_directory_or_readable(&path, &username)?;
    let metadata = fs::metadata(&path)?;
    Ok(entry_from_meta(&path, &metadata, &mut NameCache::default()))
}

pub async fn remove(headers: HeaderMap, Json(request): Json<PathRequest>) -> Response {
    let path = request.path;
    match fsutil::run_blocking(move || remove_blocking(&headers, &path)).await {
        Ok(()) => empty_response(),
        Err(error) => fsutil::fs_error_response(error),
    }
}

fn remove_blocking(headers: &HeaderMap, raw_path: &str) -> io::Result<()> {
    let username = request_user(headers)?;
    let path = match existing_path(raw_path) {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    ensure_can_modify(path.parent().unwrap_or(Path::new("/")), &username)?;
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.is_dir() {
        fs::remove_dir_all(&path)
    } else {
        fs::remove_file(&path)
    }
}

pub async fn move_entry(headers: HeaderMap, Json(request): Json<MoveRequest>) -> Response {
    match fsutil::run_blocking(move || move_entry_blocking(&headers, request)).await {
        Ok(entry) => json_response(StatusCode::OK, EntryResponse { entry }),
        Err(error) => fsutil::fs_error_response(error),
    }
}

fn move_entry_blocking(headers: &HeaderMap, request: MoveRequest) -> io::Result<FileEntry> {
    let username = request_user(headers)?;
    let source = existing_path(&request.source)?;
    let destination = fsutil::writable_path(&request.destination)?;
    ensure_can_modify(source.parent().unwrap_or(Path::new("/")), &username).and_then(|_| {
        ensure_can_modify(destination.parent().unwrap_or(Path::new("/")), &username)
    })?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(&source, &destination)?;
    let metadata = fs::metadata(&destination)?;
    Ok(entry_from_meta(
        &destination,
        &metadata,
        &mut NameCache::default(),
    ))
}

pub async fn make_dir(headers: HeaderMap, Json(request): Json<PathRequest>) -> Response {
    let path = request.path;
    match fsutil::run_blocking(move || make_dir_blocking(&headers, &path)).await {
        Ok(entry) => json_response(StatusCode::OK, EntryResponse { entry }),
        Err(error) => fsutil::fs_error_response(error),
    }
}

fn make_dir_blocking(headers: &HeaderMap, raw_path: &str) -> io::Result<FileEntry> {
    let username = request_user(headers)?;
    let path = fsutil::writable_path(raw_path)?;
    ensure_can_modify(path.parent().unwrap_or(Path::new("/")), &username)?;
    fs::create_dir_all(&path)?;
    fsutil::apply_owner(&path, &username)?;
    let metadata = fs::metadata(&path)?;
    Ok(entry_from_meta(&path, &metadata, &mut NameCache::default()))
}

#[derive(Default)]
struct NameCache {
    users: std::collections::HashMap<u32, Option<String>>,
    groups: std::collections::HashMap<u32, Option<String>>,
}

impl NameCache {
    fn user(&mut self, uid: u32) -> Option<String> {
        if let Some(name) = self.users.get(&uid) {
            return name.clone();
        }
        let name = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
            .ok()
            .flatten()
            .map(|user| user.name);
        self.users.insert(uid, name.clone());
        name
    }

    fn group(&mut self, gid: u32) -> Option<String> {
        if let Some(name) = self.groups.get(&gid) {
            return name.clone();
        }
        let name = nix::unistd::Group::from_gid(nix::unistd::Gid::from_raw(gid))
            .ok()
            .flatten()
            .map(|group| group.name);
        self.groups.insert(gid, name.clone());
        name
    }
}

fn entry_from_meta(abs_path: &Path, metadata: &Metadata, cache: &mut NameCache) -> FileEntry {
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
        owner: cache
            .user(metadata.uid())
            .unwrap_or_else(|| metadata.uid().to_string()),
        group: cache
            .group(metadata.gid())
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
    fsutil::validate_username(username)?;
    Ok(username.to_owned())
}

pub(crate) fn validate_directory(path: &Path, username: &str) -> io::Result<()> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path is not a directory",
        ));
    }
    fsutil::validate_username(username)
}

fn validate_directory_or_readable(path: &Path, username: &str) -> io::Result<()> {
    fsutil::validate_username(username)?;
    if username == "root" {
        return Ok(());
    }
    let metadata = fs::metadata(path)?;
    if metadata.is_dir() && !can_access_directory(&metadata, username, 0o500, 0o050, 0o005)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "directory access denied",
        ));
    }
    Ok(())
}

fn ensure_can_modify(path: &Path, username: &str) -> io::Result<()> {
    fsutil::validate_username(username)?;
    if username == "root" {
        return Ok(());
    }
    let metadata = fs::metadata(path)?;
    if !can_access_directory(&metadata, username, 0o300, 0o030, 0o003)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "directory modification denied",
        ));
    }
    Ok(())
}

fn can_access_directory(
    metadata: &Metadata,
    username: &str,
    owner_bits: u32,
    group_bits: u32,
    other_bits: u32,
) -> io::Result<bool> {
    let user = nix::unistd::User::from_name(username)
        .map_err(|error| io::Error::other(error.to_string()))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "user not found"))?;
    let mode = metadata.mode();
    let owner = metadata.uid() == user.uid.as_raw();
    let group = metadata.gid() == user.gid.as_raw();
    let bits = if owner {
        owner_bits
    } else if group {
        group_bits
    } else {
        other_bits
    };
    Ok(mode & bits == bits)
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

    #[cfg(unix)]
    #[tokio::test]
    async fn list_dir_reports_symlinks_including_dangling_links() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        fs::write(directory.path().join("target.txt"), b"hi").unwrap();
        symlink("target.txt", directory.path().join("link.txt")).unwrap();
        symlink("missing.txt", directory.path().join("dangling.txt")).unwrap();

        let response = list_dir(
            root_headers(),
            Json(PathRequest {
                path: directory.path().to_string_lossy().into_owned(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let names = json["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(names.contains(&"link.txt"));
        assert!(names.contains(&"dangling.txt"));
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

    #[test]
    fn root_directory_check_bypasses_permission_bits() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let restricted = directory.path().join("restricted");
        fs::create_dir(&restricted).unwrap();
        fs::set_permissions(&restricted, fs::Permissions::from_mode(0o000)).unwrap();

        assert!(validate_directory_or_readable(&restricted, "root").is_ok());
    }
}
