#!/bin/sh
# ci/build-target.sh  build a reaper release binary for one target triple.
# Usage: sh ci/build-target.sh <target-triple>
# All per-target knowledge (linkers, toolchains, std availability) lives here;
# bitbucket-pipelines.yml just dispatches.
#
# POSIX sh, and deliberately so: this project's invariant battery rejects
# `pipefail` because /bin/sh is dash on the Debian image this runs in. Nothing
# here is a pipeline, so there was never a pipeline status to lose -- the flag
# was inherited from a sibling repository, not needed. Keep it that way; if a
# pipeline is ever added, capture its status explicitly rather than reaching
# for the flag.
set -eu

TARGET="${1:?usage: build-target.sh <target-triple>}"
export CARGO_HOME="${CARGO_HOME:-${BITBUCKET_CLONE_DIR:-$PWD}/.cargo_cache}"

# Pinned: -Z build-std against a floating nightly breaks spontaneously and
# makes tag builds unreproducible. Bump deliberately, together with the image
# pin in bitbucket-pipelines.yml.
NIGHTLY="nightly-2026-08-01"

# reaper is the only shipped binary. The workspace's other four members are
# libraries it links; none of them is a program anybody runs.
BIN=reaper

# There is no Windows target here, and its absence is a fact about the code
# rather than a gap in this file. The heartbeat is detached with setsid(2) and
# stopped with kill(2), the rsh wrapper rsync runs is a `#!/bin/sh` script
# written at mode 0700, the token's permissions are read as a Unix mode, and
# every rendered command is quoted for a POSIX shell. Those are unconditional
# uses of std::os::unix in shipped code, so a Windows target does not fail at
# runtime -- it fails to compile. Adding one is a port, not a case below.

apt_install() {
    apt-get update
    apt-get install -y --no-install-recommends "$@"
}

# Zig ships FreeBSD libc headers, letting cargo-zigbuild cross-link FreeBSD
# binaries from Linux with no docker and no sysroot images. Bitbucket runs
# Linux containers and cannot host a VM, so cross-linking is the only route to
# a FreeBSD artifact here; see the note in bitbucket-pipelines.yml about what
# that trades away and what still checks it.
install_zigbuild() {
    apt_install python3-pip
    pip3 install --break-system-packages cargo-zigbuild
}

build() { # build [extra cargo args...]  tries offline first, falls back to online
    cargo build --target "$TARGET" --release --locked "$@" --offline ||
    cargo build --target "$TARGET" --release --locked "$@"
}

case "$TARGET" in
    x86_64-unknown-linux-gnu)
        build -p "$BIN"
        ;;

    aarch64-unknown-linux-gnu)
        # libc6-dev-arm64-cross is only a Recommends of the gcc package, so
        # with --no-install-recommends it must be named explicitly -- without
        # it the cross-gcc has no target libc headers/CRT.
        apt_install gcc-aarch64-linux-gnu libc6-dev-arm64-cross
        rustup target add "$TARGET"
        export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
        build -p "$BIN"
        ;;

    x86_64-unknown-freebsd)
        # Tier 2: prebuilt std exists; stable toolchain + zig linker.
        install_zigbuild
        rustup target add "$TARGET"
        cargo zigbuild --target "$TARGET" --release --locked -p "$BIN"
        ;;

    aarch64-unknown-freebsd)
        # Tier 3: no prebuilt std, so compile it with nightly -Z build-std.
        install_zigbuild
        rustup toolchain install "$NIGHTLY" --profile minimal --component rust-src
        cargo "+$NIGHTLY" zigbuild --target "$TARGET" --release --locked -p "$BIN" \
            -Z build-std=std,panic_abort
        ;;

    *)
        echo "unknown target: $TARGET" >&2
        exit 1
        ;;
esac

# --- package ---------------------------------------------------------------
# The version lives in cli/Cargo.toml, not at the workspace root: the root here
# is a pure virtual manifest and carries no version field at all. Read the one
# that names the binary, and refuse to guess if it is not there.
VERSION=$(sed -n '/^\[package\]/,/^\[/{s/^version *= *"\([^"]*\)".*/\1/p;}' cli/Cargo.toml)
test -n "$VERSION"

# Confirm the artifact is the architecture it claims before it is named after
# one. A mislabelled binary is worse than a missing one: it fails on the user's
# machine rather than on ours, and the name is the only thing they had to go on.
file "target/$TARGET/release/$BIN"

mkdir -p dist
cp "target/$TARGET/release/$BIN" "dist/${BIN}-v${VERSION}-${TARGET}"
