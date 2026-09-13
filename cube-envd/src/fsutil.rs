use axum::{
    body::Body,
    http::{header, StatusCode},
    response::Response,
};
use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};

/// Runs a blocking filesystem operation on the tokio blocking pool so a slow
/// `read_dir`/`canonicalize`/NSS lookup cannot stall the async reactor.
pub(crate) async fn run_blocking<T, F>(operation: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(operation).await {
        Ok(result) => result,
        Err(error) => Err(io::Error::other(error)),
    }
}

/// Resolves a path that may not exist yet, canonicalizing the deepest existing
/// ancestor so symlinks are followed exactly once.
pub(crate) fn writable_path(raw_path: &str) -> io::Result<PathBuf> {
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

/// Lexically collapses `.` and `..` without touching the filesystem. Leading
/// `..` is preserved for relative paths so they cannot silently escape upward.
pub(crate) fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() && !path.is_absolute() {
                    normalized.push("..");
                }
            }
            Component::Normal(component) => normalized.push(component),
        }
    }
    normalized
}

pub(crate) fn validate_username(username: &str) -> io::Result<()> {
    if username.is_empty() || username == "root" {
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

pub(crate) fn apply_owner(path: &Path, username: &str) -> io::Result<()> {
    if username.is_empty() || username == "root" {
        return Ok(());
    }
    let user = nix::unistd::User::from_name(username)
        .map_err(|error| io::Error::other(error.to_string()))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "user not found"))?;
    nix::unistd::chown(path, Some(user.uid), Some(user.gid))
        .map_err(|error| io::Error::other(error.to_string()))
}

pub(crate) fn fs_error_response(error: io::Error) -> Response {
    let (status, code) = match error.kind() {
        io::ErrorKind::NotFound => (StatusCode::NOT_FOUND, "not_found"),
        io::ErrorKind::PermissionDenied => (StatusCode::FORBIDDEN, "permission_denied"),
        io::ErrorKind::InvalidInput => (StatusCode::BAD_REQUEST, "invalid_argument"),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
    };
    error_response_with_code(status, code, error.to_string())
}

pub(crate) fn error_response_with_code(
    status: StatusCode,
    code: &str,
    message: String,
) -> Response {
    let payload = serde_json::json!({"code": code, "message": message});
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(payload.to_string()))
        .expect("valid error response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_normalize_preserves_leading_parent_for_relative_paths() {
        assert_eq!(
            lexical_normalize(Path::new("a/../../b")),
            PathBuf::from("../b")
        );
    }

    #[test]
    fn lexical_normalize_collapses_absolute_parent() {
        assert_eq!(
            lexical_normalize(Path::new("/a/b/../c")),
            PathBuf::from("/a/c")
        );
        assert_eq!(lexical_normalize(Path::new("/..")), PathBuf::from("/"));
    }

    #[test]
    fn writable_path_resolves_new_file_under_existing_parent() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("nested").join("file.txt");
        let resolved = writable_path(&target.to_string_lossy()).unwrap();
        assert_eq!(resolved, target);
        assert!(!directory.path().join("nested").exists());
    }
}
