#!/usr/bin/env bash
# Generate a GitHub Release body from git history.
#
# The release body lists every commit between the previous release tag and the
# version being released, grouped by Conventional Commits type, with short-hash
# links to each commit on GitHub.
#
# Usage:
#   tools/release-notes.sh --version 1.7.2 --tag v1.7.2 --prev v1.7.1
#   tools/release-notes.sh --version 1.0.0 --tag v1.0.0        # first release
#
# --repo defaults to $GITHUB_REPOSITORY, then to the origin remote.
# When --tag does not exist locally yet (CI creates the tag after generating
# the notes), HEAD is used as the range end; the compare/commit links still use
# the tag name.
set -euo pipefail

usage() {
    sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'
}

die() {
    echo "release-notes: $*" >&2
    exit 2
}

version=""
tag=""
prev=""
repo="${GITHUB_REPOSITORY:-}"

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            [ "$#" -ge 2 ] || die "--version needs a value"
            version="$2"
            shift 2
            ;;
        --tag)
            [ "$#" -ge 2 ] || die "--tag needs a value"
            tag="$2"
            shift 2
            ;;
        --prev)
            [ "$#" -ge 2 ] || die "--prev needs a value"
            prev="$2"
            shift 2
            ;;
        --repo)
            [ "$#" -ge 2 ] || die "--repo needs a value"
            repo="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "unknown argument: $1"
            ;;
    esac
done

[ -n "$version" ] || die "--version is required"
[ -n "$tag" ] || die "--tag is required"

if [ -z "$repo" ]; then
    repo="$(git remote get-url origin 2>/dev/null |
        sed -E 's#^[A-Za-z][A-Za-z0-9+.-]*://[^/]+/##; s#^git@[^:]+:##; s#\.git$##' || true)"
fi
printf '%s' "$repo" | grep -Eq '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$' ||
    die "cannot determine GitHub repository; pass --repo owner/name"

git rev-parse --git-dir >/dev/null 2>&1 || die "not inside a git repository"

# The tag may be created after this script runs (the release job calls the
# GitHub API / action-gh-release to create it), so fall back to HEAD.
end_rev="$tag"
if ! git rev-parse --verify --quiet "${tag}^{commit}" >/dev/null; then
    end_rev="HEAD"
fi

if [ -n "$prev" ]; then
    git rev-parse --verify --quiet "${prev}^{commit}" >/dev/null ||
        die "previous tag not found: $prev"
    range="$prev..$end_rev"
else
    range="$end_rev"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
: >"$tmp/feat"
: >"$tmp/fix"
: >"$tmp/perf"
: >"$tmp/other"
total=0

# tformat appends a newline after every commit, including the last one.
# (--pretty=format omits the final newline, which made `while read` drop the
# oldest commit; single-commit ranges produced an empty release body.)
while IFS=$'\t' read -r full short subject; do
    [ -n "$full" ] || continue
    case "$subject" in
        chore\(release*|Release\ v*|v[0-9]*.[0-9]*.[0-9]*) continue ;;
    esac
    total=$((total + 1))
    case "$subject" in
        feat:*|feat\(*\):*|feat!:*|feat\(*\)!:*) section=feat ;;
        fix:*|fix\(*\):*|fix!:*|fix\(*\)!:*) section=fix ;;
        perf:*|perf\(*\):*|perf!:*|perf\(*\)!:*) section=perf ;;
        *) section=other ;;
    esac
    text="$(printf '%s' "$subject" | sed -E 's/^[a-z]+(\([^)]*\))?!?: //')"
    # Keep link text from breaking the Markdown link.
    text="$(printf '%s' "$text" | sed -e 's/\\/\\\\/g' -e 's/\[/\\[/g' -e 's/\]/\\]/g')"
    printf -- '- [%s](https://github.com/%s/commit/%s) %s\n' \
        "$short" "$repo" "$full" "$text" >>"$tmp/$section"
done < <(git log --no-merges --pretty=tformat:'%H%x09%h%x09%s' "$range")

count() {
    wc -l <"$tmp/$1" | tr -d ' '
}

plural() { # count singular plural
    if [ "$1" -eq 1 ]; then
        printf '%s %s' "$1" "$2"
    else
        printf '%s %s' "$1" "$3"
    fi
}

n_feat="$(count feat)"
n_fix="$(count fix)"
n_perf="$(count perf)"
n_other="$(count other)"

parts=()
[ "$n_feat" -gt 0 ] && parts+=("$(plural "$n_feat" feature features)")
[ "$n_fix" -gt 0 ] && parts+=("$(plural "$n_fix" fix fixes)")
[ "$n_perf" -gt 0 ] && parts+=("$(plural "$n_perf" performance improvement 'performance improvements')")
[ "$n_other" -gt 0 ] && parts+=("$(plural "$n_other" 'other change' 'other changes')")

summary=""
if [ "${#parts[@]}" -gt 0 ]; then
    summary="$(printf '%s, ' "${parts[@]}")"
    summary="${summary%, }"
fi

if [ "$total" -eq 1 ]; then
    commit_word="commit"
else
    commit_word="commits"
fi

section() { # file heading
    if [ -s "$tmp/$1" ]; then
        echo "### $2"
        cat "$tmp/$1"
        echo
    fi
}

{
    echo "## joplin-rs $version"
    echo
    if [ -n "$prev" ]; then
        if [ -n "$summary" ]; then
            echo "**$total $commit_word** since \`$prev\`: $summary."
        else
            echo "**No commits** since \`$prev\`."
        fi
        echo
        echo "[Full comparison](https://github.com/$repo/compare/$prev...$tag)"
        echo
    else
        echo "_Initial release_. **$total $commit_word**."
        echo
    fi
    section feat "✨ New features"
    section fix "🐛 Bug fixes"
    section perf "⚡ Performance"
    section other "🔧 Other changes"
} | sed -e '${/^$/d;}'
