//! Records the git commit scanai is built from, for the startup log line.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// The commit `cargo package` recorded, in a crate downloaded from crates.io
/// (which is not a git repository).
fn packaged_commit() -> Option<String> {
    let info = std::fs::read_to_string(".cargo_vcs_info.json").ok()?;
    let rest = &info[info.find("\"sha1\"")? + 6..];
    let start = rest.find('"')? + 1;
    let sha = &rest[start..start + rest[start..].find('"')?];
    Some(sha.chars().take(10).collect())
}

fn main() {
    println!("cargo:rerun-if-changed=.cargo_vcs_info.json");
    let hash = packaged_commit().or_else(|| {
        git(&["rev-parse", "--short=10", "HEAD"]).map(|h| {
            let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
            if dirty { format!("{h}-dirty") } else { h }
        })
    });
    println!("cargo:rustc-env=SCANAI_GIT_HASH={}", hash.as_deref().unwrap_or("unknown"));
    if let Some(dir) = git(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
        println!("cargo:rerun-if-changed={dir}/index");
        if let Some(r) = git(&["symbolic-ref", "-q", "HEAD"]) {
            println!("cargo:rerun-if-changed={dir}/{r}");
        }
    }
}
