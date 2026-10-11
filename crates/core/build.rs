//! Derive the package version from git tags and commit messages at compile
//! time.
//!
//! Bump rules (Conventional Commits), counted over the commits after the
//! latest `vM.I.P` tag (or over all of history when no such tag exists):
//! - `feat:` / `feat(scope):`          -> MINOR +1
//! - `fix:`  / `fix(scope):`           -> PATCH +1
//! - a `!` before the colon
//!   (e.g. `feat!:`, `feat(api)!:`)    -> MAJOR +1
//! - anything else (chore, refactor, docs, test, non-conventional subjects,
//!   merge commits) does not bump the version.
//!
//! The resulting version is `M.I.P+g<short-hash>`. CI tags look like
//! `vM.I.P-g<hash>` and are parsed the same as `vM.I.P`.
//! Git unavailable / outside a git checkout -> the plain Cargo.toml version.

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
            let base = parse_version_tag(&tag)?;
            let subjects = git_log_subjects(Some(&format!("{tag}..HEAD")))?;
            bump(base, &subjects)
        }
        None => {
            let base = parse_semver(cargo_version)?;
            let subjects = git_log_subjects(None)?;
            bump(base, &subjects)
        }
    };

    Some(format!("{major}.{minor}.{patch}+g{hash}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bump {
    None,
    Patch,
    Minor,
    Major,
}

/// Apply the bump implied by each commit subject onto a base `(M, I, P)`.
fn bump(base: (u64, u64, u64), subjects: &[String]) -> (u64, u64, u64) {
    let (mut major, mut minor, mut patch) = base;
    for subject in subjects {
        match classify(subject) {
            Bump::Major => major += 1,
            Bump::Minor => minor += 1,
            Bump::Patch => patch += 1,
            Bump::None => {}
        }
    }
    (major, minor, patch)
}

/// Classify one commit subject per Conventional Commits.
fn classify(subject: &str) -> Bump {
    // Only the header matters: first line, up to the colon.
    let header = subject.lines().next().unwrap_or("").trim();
    let Some((kind, _)) = header.split_once(':') else {
        return Bump::None;
    };
    let breaking = kind.ends_with('!');
    let kind = kind.trim_end_matches('!');
    let ty = kind.split('(').next().unwrap_or(kind).trim();
    if breaking {
        Bump::Major
    } else if ty.eq_ignore_ascii_case("feat") {
        Bump::Minor
    } else if ty.eq_ignore_ascii_case("fix") {
        Bump::Patch
    } else {
        Bump::None
    }
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

/// Latest tag matching `v<digit>...` (covers both `v0.2.0` and CI tags like
/// `v0.1.46-g76a78f8`). Tags without the `v` prefix are ignored.
fn latest_version_tag() -> Option<String> {
    git(&["describe", "--tags", "--abbrev=0", "--match", "v[0-9]*"])
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_conventional_subjects() {
        assert_eq!(classify("feat: new sync stage"), Bump::Minor);
        assert_eq!(classify("feat(app): rounded icon"), Bump::Minor);
        assert_eq!(classify("fix(sync): decode chunks"), Bump::Patch);
        assert_eq!(classify("fix: utf8 boundary"), Bump::Patch);
        assert_eq!(classify("feat!: rework config format"), Bump::Major);
        assert_eq!(classify("feat(api)!: breaking change"), Bump::Major);
        assert_eq!(classify("fix!: changed error contract"), Bump::Major);
        assert_eq!(classify("chore: bump deps"), Bump::None);
        assert_eq!(classify("refactor(cli): drop flag"), Bump::None);
        assert_eq!(classify("docs: note it"), Bump::None);
        assert_eq!(classify("test: add case"), Bump::None);
        assert_eq!(classify("plain message without type"), Bump::None);
        assert_eq!(classify("Merge branch 'feature'"), Bump::None);
    }

    #[test]
    fn bump_counts_each_kind() {
        let s = |x: &str| x.to_string();
        let subjects = vec![
            s("fix: a"),
            s("fix: b"),
            s("feat: c"),
            s("chore: d"),
            s("feat(app)!: e"),
        ];
        assert_eq!(bump((1, 0, 0), &subjects), (2, 1, 2));
    }

    #[test]
    fn empty_range_keeps_base() {
        assert_eq!(bump((1, 2, 3), &[]), (1, 2, 3));
    }

    #[test]
    fn tag_and_semver_parsing() {
        assert_eq!(parse_version_tag("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version_tag("v0.1.47-ge69647e"), Some((0, 1, 47)));
        assert_eq!(parse_version_tag("1.2.3"), None);
        assert_eq!(parse_semver("1.0.0"), Some((1, 0, 0)));
        assert_eq!(parse_semver("1.2.3-rc.1"), Some((1, 2, 3)));
        assert_eq!(parse_semver("1.2.3+meta"), Some((1, 2, 3)));
    }
}
