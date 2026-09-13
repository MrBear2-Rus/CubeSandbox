use std::{env, process::Command};

fn main() {
    println!("cargo:rerun-if-env-changed=CUBE_ENVD_COMMIT");

    let commit = env::var("CUBE_ENVD_COMMIT")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map(|value| value.chars().take(7).collect::<String>())
        .or_else(git_short_sha)
        .unwrap_or_else(|| "unknown".to_owned());

    println!("cargo:rustc-env=GIT_SHORT_SHA={commit}");
}

fn git_short_sha() -> Option<String> {
    let manifest_dir = env::var_os("CARGO_MANIFEST_DIR")?;
    let output = Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .current_dir(manifest_dir)
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    (value.len() >= 7).then(|| value.to_owned())
}
