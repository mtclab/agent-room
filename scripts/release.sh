#!/usr/bin/env bash
# Cut a release: one static binary per target, a tarball each, and SHA256SUMS.
#
# Everything happens on this machine; there is no CI (house rule, private repo).
# The binaries are STATIC musl builds so a friend can copy one onto any Linux of
# the right architecture and run it - no Python, no glibc version to match, no
# shared library to install.
#
#   make release                      # every target in TARGETS
#   TARGETS=x86_64-unknown-linux-musl make release
#
# Cross-compiling needs a C toolchain for the target, because two dependencies
# are C: aws-lc-sys (the rustls crypto provider) and the bundled SQLite the
# crypto store is kept in. `zig cc` is that toolchain here, driven by
# cargo-zigbuild:
#
#   cargo install cargo-zigbuild
#   python3 -m venv ~/.local/share/agent-room-build/zig-venv
#   ~/.local/share/agent-room-build/zig-venv/bin/pip install ziglang
#   printf '#!/bin/sh\nexec ~/.local/share/agent-room-build/zig-venv/bin/python -m ziglang "$@"\n' \
#       > ~/.local/bin/zig && chmod +x ~/.local/bin/zig
#
# (Or any other zig on PATH: https://ziglang.org/download/.)
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

TARGETS="${TARGETS:-x86_64-unknown-linux-musl aarch64-unknown-linux-musl}"
DIST="$REPO/dist"

VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
if [ -z "$VERSION" ]; then
    echo "release: cannot read the version out of Cargo.toml" >&2
    exit 1
fi

if ! command -v zig >/dev/null 2>&1; then
    echo "release: no 'zig' on PATH - see the header of scripts/release.sh." >&2
    echo "         A musl build needs a C compiler for the target: aws-lc-sys" >&2
    echo "         and the bundled SQLite are C." >&2
    exit 1
fi
if ! cargo zigbuild --help >/dev/null 2>&1; then
    echo "release: cargo-zigbuild is not installed ('cargo install cargo-zigbuild')." >&2
    exit 1
fi

# Nothing about the machine this was built on may travel to a friend's disk.
# rustc bakes the SOURCE PATH of every panic location into the binary, and for a
# dependency that path is this user's $CARGO_HOME - so an unremapped build ships
# several hundred copies of the builder's home directory and login name to
# everybody who is sent a tarball. Remapping is the fix (`trim-paths` is still
# nightly-only on 1.96, so this is the stable form), and check_no_build_paths
# below is what stops a release going out without it.
CARGO_DIR="${CARGO_HOME:-$HOME/.cargo}"
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$CARGO_DIR=/cargo --remap-path-prefix=$REPO=/src"

# A build path that reached the binary is a release defect, not a warning.
check_no_build_paths() {
    local binary="$1"
    local hits
    # An empty or "/" HOME would make the pattern match everything; a build
    # machine like that has nothing to leak anyway.
    if [ -z "${HOME:-}" ] || [ "$HOME" = "/" ]; then
        return 0
    fi
    hits="$(LC_ALL=C grep -aoF -e "$CARGO_DIR" -e "$HOME" -- "$binary" | sort | uniq -c || true)"
    if [ -n "$hits" ]; then
        echo >&2
        echo "release: $binary leaks build-machine paths:" >&2
        echo "$hits" >&2
        echo "         RUSTFLAGS remapping did not take. Nothing was packaged." >&2
        exit 1
    fi
}

mkdir -p "$DIST"
# Only ever remove THIS version's own artefacts: a dist directory may hold
# releases somebody is still handing out.
rm -f "$DIST/agent-room-$VERSION"-*.tar.gz "$DIST/SHA256SUMS"

for target in $TARGETS; do
    echo "=== $target ==="
    rustup target add "$target" >/dev/null
    cargo zigbuild --release --target "$target"

    binary="target/$target/release/agent-room"
    check_no_build_paths "$binary"
    stage="$DIST/agent-room-$VERSION-$target"
    rm -rf "$stage"
    mkdir -p "$stage"
    install -m 0755 "$binary" "$stage/agent-room"
    install -m 0644 docs/ONBOARDING.md docs/BRAIN_CONTRACT.md docs/MCP.md "$stage/"
    install -m 0644 examples/config.example.yaml examples/session.example.yaml \
        examples/agent-room.service "$stage/"
    tar -czf "$stage.tar.gz" -C "$DIST" "agent-room-$VERSION-$target"
    rm -rf "$stage"
done

cd "$DIST"
sha256sum "agent-room-$VERSION"-*.tar.gz > SHA256SUMS

echo
echo "=== dist/ ==="
ls -l "agent-room-$VERSION"-*.tar.gz SHA256SUMS
echo
cat SHA256SUMS
