//! Bakes the version, commit and build date into the binary, the way the Go
//! build did with -ldflags. Release builds set OSTRICH_VERSION, OSTRICH_COMMIT
//! and OSTRICH_DATE in the environment; a plain `cargo build` asks git.

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// A directory git names, made absolute against the package root (git
/// prints `.git` relative to where it runs, which is the package root).
fn git_path(args: &[&str]) -> Option<PathBuf> {
    let dir = git(args)?;
    let root = std::env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from)?;
    Some(root.join(dir))
}

/// Tells cargo to run the script again when `path` changes, if it exists:
/// cargo runs it on every build for a path that is missing.
fn rerun_if_changed(path: &Path) {
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=OSTRICH_VERSION");
    println!("cargo:rerun-if-env-changed=OSTRICH_COMMIT");
    println!("cargo:rerun-if-env-changed=OSTRICH_DATE");
    // What the git answers below depend on: the checked-out commit (HEAD,
    // and the branch it names under refs or in packed-refs), the tags
    // (refs, packed-refs) and the index (--dirty). In a worktree, HEAD and
    // the index are its own and the refs are shared with the main checkout.
    if let Some(git_dir) = git_path(&["rev-parse", "--git-dir"]) {
        let common_dir =
            git_path(&["rev-parse", "--git-common-dir"]).unwrap_or_else(|| git_dir.clone());
        rerun_if_changed(&git_dir.join("HEAD"));
        rerun_if_changed(&git_dir.join("index"));
        rerun_if_changed(&common_dir.join("refs"));
        rerun_if_changed(&common_dir.join("packed-refs"));
    }

    let version = std::env::var("OSTRICH_VERSION")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| git(&["describe", "--tags", "--always", "--dirty"]))
        .unwrap_or_else(|| "dev".to_string());
    let commit = std::env::var("OSTRICH_COMMIT")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| git(&["rev-parse", "--short", "HEAD"]))
        .unwrap_or_else(|| "none".to_string());
    let date = std::env::var("OSTRICH_DATE")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| git(&["log", "-1", "--format=%cI"]))
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=OSTRICH_VERSION={version}");
    println!("cargo:rustc-env=OSTRICH_COMMIT={commit}");
    println!("cargo:rustc-env=OSTRICH_DATE={date}");
}
