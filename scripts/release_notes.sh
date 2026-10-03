#!/usr/bin/env bash
#
# Print the GitHub release notes for a version tag: install steps, a table of the archives found
# in DIST_DIR, and the Conventional Commit subjects since the previous tag.
#
# Usage: GITHUB_REPOSITORY=owner/repo scripts/release_notes.sh TAG DIST_DIR
set -euo pipefail

tag=${1:?usage: release_notes.sh TAG DIST_DIR}
dist=${2:?usage: release_notes.sh TAG DIST_DIR}
repo=${GITHUB_REPOSITORY:?GITHUB_REPOSITORY must be set}
version=${tag#v}
base="https://github.com/$repo"

cat <<EOF
## Install

Download the archive for your system, then unpack and run it:

    tar xzf silver-$version-linux-x86_64.tar.gz
    ./silver-$version-linux-x86_64/silver

Open <http://127.0.0.1:7777> and pick a provider under Settings → Providers.

On Windows, unzip the archive and run \`silver.exe\`. silver runs commands through bash, so install
[Git for Windows](https://git-scm.com/download/win) first.

Check a download with \`sha256sum -c --ignore-missing SHA256SUMS\`. The macOS binaries are not
signed; if macOS blocks one downloaded in a browser, run \`xattr -d com.apple.quarantine silver\`.

## Downloads

| System | Architecture | Archive |
| --- | --- | --- |
EOF

while IFS='|' read -r name system arch ext; do
    file="silver-$version-$name.$ext"
    if [ -f "$dist/$file" ]; then
        echo "| $system | $arch | [\`$file\`]($base/releases/download/$tag/$file) |"
    fi
done <<'EOF'
linux-x86_64|Linux|x86_64|tar.gz
linux-arm64|Linux|ARM64|tar.gz
macos-arm64|macOS|Apple Silicon|tar.gz
macos-x86_64|macOS|Intel|tar.gz
windows-x86_64|Windows|x86_64|zip
EOF

prev=$(git describe --tags --abbrev=0 --match 'v*' "$tag^" 2>/dev/null || true)
[ -n "$prev" ] || exit 0

subject_re='^([a-z]+)(\(([^)]*)\))?(!)?: (.+)$'
breaking="" features="" fixes="" perf=""
while IFS=$'\t' read -r sha subject; do
    [[ $subject =~ $subject_re ]] || continue
    type=${BASH_REMATCH[1]} scope=${BASH_REMATCH[3]} bang=${BASH_REMATCH[4]} text=${BASH_REMATCH[5]}
    line="- ${scope:+**$scope:** }$text ([\`$sha\`]($base/commit/$sha))"$'\n'
    if [ -n "$bang" ]; then
        breaking+=$line
    else
        case $type in
            feat) features+=$line ;;
            fix) fixes+=$line ;;
            perf) perf+=$line ;;
        esac
    fi
done < <(git log --no-merges --format='%h%x09%s' "$prev..$tag")

section() {
    if [ -n "$2" ]; then
        printf '\n### %s\n\n%s' "$1" "$2"
    fi
}

printf '\n## What'\''s changed\n'
section "Breaking changes" "$breaking"
section "Features" "$features"
section "Fixes" "$fixes"
section "Performance" "$perf"
printf '\n**Full changelog**: %s/compare/%s...%s\n' "$base" "$prev" "$tag"
