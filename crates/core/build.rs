//! Derive the package version from git tags at compile time.
//!
//! Rules:
//! - Latest version tag (`vM.I.P`, or a CI tag `vM.I.P-g<hash>`), N commits
//!   ahead of it -> `M.I.(P+N)+g<short-hash>`.
//! - No version tag yet -> Cargo.toml version as the base, patch plus the
//!   total commit count.
//! - Git unavailable / outside a git checkout -> the plain Cargo.toml version.

use std::process::Command;

fn main() {
    let cargo_version = env!("CARGO_PKG_VERSION");
    let version = git_version(cargo_version).unwrap_or_else(|| cargo_version.to_string());
    println!("cargo:rustc-env=JOPLIN_RS_GIT_VERSION={version}");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");
}

fn git_version(cargo_version: &str) -> Option<String> {
    let hash = git(&["rev-parse", "--short", "HEAD"])?;

    let (major, minor, patch) = match latest_version_tag() {
        Some(tag) => {
            let (major, minor, patch) = parse_version_tag(&tag)?;
            let n: u64 = git(&["rev-list", &format!("{tag}..HEAD"), "--count"])?
                .parse()
                .ok()?;
            (major, minor, patch + n)
        }
        None => {
            let (major, minor, patch) = parse_semver(cargo_version)?;
            let n: u64 = git(&["rev-list", "--count", "HEAD"])?.parse().ok()?;
            (major, minor, patch + n)
        }
    };

    Some(format!("{major}.{minor}.{patch}+g{hash}"))
}

/// Latest tag matching `v<digit>...` (covers both `v0.2.0` and CI tags like
/// `v0.1.46-g76a78f8`). Tags without the `v` prefix are ignored.
fn latest_version_tag() -> Option<String> {
    git(&[
        "describe",
        "--tags",
        "--abbrev=0",
        "--match",
        "v[0-9]*",
    ])
}

/// Parse `vM.I.P` or `vM.I.P-g<hash>` into `(M, I, P)`.
fn parse_version_tag(tag: &str) -> Option<(u64, u64, u64)> {
    let body = tag.strip_prefix('v')?;
    // CI tags carry a `-g<hash>` prerelease-style suffix; strip it.
    let semver_part = body.split_once("-g").map_or(body, |(v, _)| v);
    parse_semver(semver_part)
}

fn parse_semver(v: &str) -> Option<(u64, u64, u64)> {
    // Tolerate build metadata / prerelease suffixes on the Cargo version too.
    let core = v.split(['+', '-']).next().unwrap_or(v);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?;
    let s = s.trim();
    if s.is_empty() { None } else { Some(s.to_string()) }
}
