#!/usr/bin/env bash
#
# Build silver (HTTP API with the web UI embedded) and install the binary.
#
# Usage: scripts/install.sh [OPTIONS]
#
# The destination is $SILVER_PREFIX when set, otherwise $HOME/.local/bin.
# The script never uses sudo and never escalates privileges.
set -euo pipefail

PROGRAM="$(basename "$0")"

die() {
    printf 'install: %s\n' "$*" >&2
    exit 1
}

info() {
    printf 'install: %s\n' "$*"
}

usage() {
    cat <<EOF
Usage: $PROGRAM [OPTIONS]

Build the web UI and silver in release mode and install the binary into
\$SILVER_PREFIX (default: \$HOME/.local/bin).

Options:
  --prefix DIR   Install into DIR instead of the default.
  --build        Force a release build even when binaries already exist.
  --no-build     Never build; install the release binaries already present.
  -h, --help     Show this help.
EOF
}

PREFIX="${SILVER_PREFIX:-$HOME/.local/bin}"
FORCE_BUILD=0
NO_BUILD=0

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix)
            [ $# -ge 2 ] || die "--prefix needs a directory"
            PREFIX="$2"
            shift 2
            ;;
        --prefix=*)
            PREFIX="${1#--prefix=}"
            shift
            ;;
        --build)
            FORCE_BUILD=1
            shift
            ;;
        --no-build)
            NO_BUILD=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "unknown argument: $1 (try --help)"
            ;;
    esac
done

[ "$FORCE_BUILD" -eq 0 ] || [ "$NO_BUILD" -eq 0 ] || die "--build and --no-build are mutually exclusive"
[ -n "$PREFIX" ] || die "the install prefix must not be empty"

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
RELEASE_DIR="$TARGET_DIR/release"
BINARIES="silver"

need_build=0
for bin in $BINARIES; do
    if [ ! -x "$RELEASE_DIR/$bin" ]; then
        need_build=1
    fi
done

if [ "$NO_BUILD" -eq 1 ]; then
    [ "$need_build" -eq 0 ] || die "release binaries not found in $RELEASE_DIR; run without --no-build"
fi

if [ "$FORCE_BUILD" -eq 1 ] || { [ "$NO_BUILD" -eq 0 ] && [ "$need_build" -eq 1 ]; }; then
    command -v cargo >/dev/null 2>&1 \
        || die "cargo not found on PATH; install Rust (https://rustup.rs) or pass --no-build"
    # The release build builds and embeds the web UI, which needs npm.
    command -v npm >/dev/null 2>&1 \
        || die "npm not found on PATH; install Node.js to build the web UI, or pass --no-build"
    info "building the web UI and the release binary with cargo build --release -p silver"
    cargo build --release -p silver
elif [ "$need_build" -eq 0 ]; then
    info "using existing release binaries in $RELEASE_DIR (pass --build to rebuild)"
fi

install -d "$PREFIX"
for bin in $BINARIES; do
    src="$RELEASE_DIR/$bin"
    [ -x "$src" ] || die "missing binary: $src"
    install -m 0755 "$src" "$PREFIX/$bin"
    info "installed $PREFIX/$bin"
done

case ":${PATH:-}:" in
    *":$PREFIX:"*) ;;
    *)
        printf '\nwarning: %s is not on your PATH.\n' "$PREFIX" >&2
        printf 'Add it with:\n\n    export PATH="%s:$PATH"\n' "$PREFIX" >&2
        ;;
esac

printf '\nNext step: run `silver` and open http://127.0.0.1:7777 to configure a provider.\n'
