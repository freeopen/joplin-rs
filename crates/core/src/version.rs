//! Pure version-string helpers shared between `build.rs` and the tests.
//!
//! `build.rs` cannot be exercised by `cargo test` (its `#[cfg(test)]` code
//! never runs there), so the testable logic lives here and the build script
//! only orchestrates git calls. The CI workflow
//! (`.github/workflows/release.yml`) resolves the release version from the
//! binary's `--version` output with
//! `sed -E 's/.* v([0-9]+\.[0-9]+\.[0-9]+).*/\1/'`; keep [`version_display`]
//! and that expression in sync (guarded by tests in this module).

/// The plain `M.I.P` version that CI tags releases with, derived from the
/// latest `vM.I.P` tag plus the Conventional-Commits bumps after it
/// (`feat` -> minor, `fix` -> patch, `!` -> major), or from the Cargo.toml
/// semver over the full history when no version tag exists.
pub fn derive_version(cargo_version: &str, tag: Option<&str>, subjects: &[String]) -> Option<String> {
    let (major, minor, patch) = match tag {
        Some(tag) => {
            let base = parse_version_tag(tag)?;
            bump(base, subjects)
        }
        None => {
            let base = parse_semver(cargo_version)?;
            bump(base, subjects)
        }
    };
    Some(format!("{major}.{minor}.{patch}"))
}

/// The human-facing `--version` string: `vM.I.P (<hash>)`, or just `vM.I.P`
/// when there is no commit hash (outside a git checkout).
pub fn version_display(version: &str, hash: Option<&str>) -> String {
    match hash {
        Some(hash) => format!("v{version} ({hash})"),
        None => format!("v{version}"),
    }
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

    #[test]
    fn derive_from_tag_and_from_cargo() {
        let s = |x: &str| x.to_string();
        // Tag path: bumps count only after the tag.
        assert_eq!(
            derive_version("9.9.9", Some("v1.0.0"), &[s("feat: a"), s("fix: b")]),
            Some("1.1.1".to_string())
        );
        // No tag: bumps count over the Cargo.toml base.
        assert_eq!(
            derive_version("1.0.0", None, &[s("feat: a")]),
            Some("1.1.0".to_string())
        );
        // Unparseable tag -> falls back to None (build.rs then uses Cargo).
        assert_eq!(derive_version("1.0.0", Some("not-a-tag"), &[]), None);
    }

    #[test]
    fn display_format_with_and_without_hash() {
        assert_eq!(
            version_display("1.0.0", Some("b30d7ede3")),
            "v1.0.0 (b30d7ede3)"
        );
        assert_eq!(version_display("1.0.0", None), "v1.0.0");
        // A hash that itself starts with `v` must not confuse consumers.
        assert_eq!(version_display("1.1.0", Some("va2751c")), "v1.1.0 (va2751c)");
    }

    /// The CI workflow resolves the release version from the binary's
    /// `--version` output with a sed expression; the display format built
    /// here must stay parseable by it. This test mirrors that expression.
    #[test]
    fn display_string_is_parseable_like_ci_sed() {
        let sed_extract = |display: &str| -> String {
            extract_like_ci_sed(display).expect("CI sed must find a version in the display string")
        };
        assert_eq!(
            sed_extract(&version_display("1.1.0", Some("a2751c434"))),
            "1.1.0"
        );
        assert_eq!(sed_extract(&version_display("1.1.0", None)), "1.1.0");
        assert_eq!(
            sed_extract(&version_display("1.1.0", Some("va2751c"))),
            "1.1.0"
        );
    }

    /// Minimal stand-in for the CI sed expression
    /// `sed -E 's/.* v([0-9]+\.[0-9]+\.[0-9]+).*/\1/'`: find the LAST ` v`
    /// (sed's leading `.*` is greedy) followed by `M.I.P` and capture the
    /// digits. Deliberately written as a scan rather than a regex crate
    /// dependency (the core crate should stay lean).
    fn extract_like_ci_sed(s: &str) -> Option<String> {
        // CI sees `joplin-rs vM.I.P ...`; display strings built here start
        // with `v`, so scan for `v` either at the start or after a space.
        let mut starts = vec![0];
        starts.extend(
            s.match_indices(" v")
                .filter(|(i, _)| *i > 0)
                .map(|(i, _)| i + 1),
        );
        starts.iter().rev().find_map(|i| semver_at(&s[*i..]))
    }

    /// If `s` starts with `M.I.P` (digits and dots), return it.
    fn semver_at(s: &str) -> Option<String> {
        let s = s.strip_prefix('v')?;
        let end = s
            .char_indices()
            .find(|(_, c)| !c.is_ascii_digit() && *c != '.')
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        let candidate = &s[..end];
        let mut parts = candidate.split('.');
        let ok = [0, 0, 0]
            .iter()
            .all(|_| {
                parts
                    .next()
                    .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            })
            && parts.next().is_none();
        ok.then(|| candidate.to_string())
    }
}
