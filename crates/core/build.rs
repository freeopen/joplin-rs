//! Derive the package version from git tags and commit messages at compile
//! time.
//!
//! The pure logic (Conventional-Commits bumping, tag/semver parsing, display
//! formatting) lives in [`crate::version`] so `cargo test` can exercise it;
//! this script only runs git and forwards the results to the compiler.
//!
//! Two env vars are exported to the crate:
//! - `JOPLIN_RS_GIT_VERSION`: the plain `M.I.P` (what CI tags releases with)
//! - `JOPLIN_RS_VERSION_DISPLAY`: what `--version` and the GUI About panel
//!   show, e.g. `v1.0.0 (b30d7ede3)`; the hash is omitted outside git.

use std::process::Command;

// Share the pure logic with the library crate: the build script is its own
// crate, so `crate::version` would not resolve; include the same source file
// directly (kept in sync via `cargo:rerun-if-changed` on the lib source).
#[path = "src/version.rs"]
mod version;

fn main() {
    let cargo_version = env!("CARGO_PKG_VERSION");
    let tag = latest_version_tag();
    let subjects = match &tag {
        Some(tag) => git_log_subjects(Some(&format!("{tag}..HEAD"))),
        None => git_log_subjects(None),
    }
    .unwrap_or_default();
    let version = version::derive_version(cargo_version, tag.as_deref(), &subjects)
        .unwrap_or_else(|| cargo_version.to_string());
    let display = version::version_display(
        &version,
        git(&["rev-parse", "--short=9", "HEAD"]).as_deref(),
    );
    println!("cargo:rustc-env=JOPLIN_RS_GIT_VERSION={version}");
    println!("cargo:rustc-env=JOPLIN_RS_VERSION_DISPLAY={display}");
    // The build script runs with the crate dir (crates/core) as cwd, so
    // repo-relative paths like `.git/HEAD` would point at a nonexistent file
    // and never trigger a rebuild; anchor them at the repo root instead.
    // Also rerun when the shared version logic changes.
    println!("cargo:rerun-if-changed=src/version.rs");
    if let Some(root) = git(&["rev-parse", "--show-toplevel"]) {
        println!("cargo:rerun-if-changed={root}/.git/HEAD");
        println!("cargo:rerun-if-changed={root}/.git/refs");
    }
}

/// Latest tag matching `v<digit>...` (covers both `v0.2.0` and CI tags like
/// `v0.1.46-g76a78f8`). Tags without the `v` prefix are ignored.
fn latest_version_tag() -> Option<String> {
    git(&["describe", "--tags", "--abbrev=0", "--match", "v[0-9]*"])
}

/// Subject lines of the commits in `range` (e.g. `v1.0.0..HEAD`), or of all
/// history when `range` is `None`. An empty range yields an empty vec.
fn git_log_subjects(range: Option<&str>) -> Option<Vec<String>> {
    let output = match range {
        Some(range) => git_text(&["log", "--format=%s", range]),
        None => git_text(&["log", "--format=%s"]),
    }?;
    Some(
        output
            .lines()
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .collect(),
    )
}

fn git(args: &[&str]) -> Option<String> {
    let s = git_text(args)?;
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Run git and return stdout (may be empty). `None` only on spawn/exit failure.
fn git_text(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}
