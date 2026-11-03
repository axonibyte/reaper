#!/bin/sh
#
# Everything that can be checked without a hypervisor, a token or a network.
#
# This is the gate a commit is expected to pass. It deliberately runs every
# check rather than stopping at the first failure, because knowing that three
# things broke is worth more than knowing that one did.
#
# Exit 0 if everything passed, 1 otherwise.
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "${root}"

failed=''

run() { # run <label> <command...>
    label=$1
    shift
    printf '\n=== %s ===\n' "${label}"
    if "$@"; then
        :
    else
        failed="${failed} ${label}"
    fi
}

# Warnings are errors here, every target -- tests and benches included. A
# warning the compiler prints and nobody reads is a decision this project
# quietly made; a release build that fails on a stricter machine is how that
# decision got noticed. Its own target directory, so the strict flags do not
# thrash the ordinary build cache.
rust_warnings() {
    RUSTFLAGS="-D warnings" CARGO_TARGET_DIR=target/warnings         cargo build --workspace --all-targets --locked --quiet
}

# Formatting, and clippy. Both were already enforced in CI and NOT here, which
# is the worst arrangement of the three available: a contributor runs the local
# gate, sees it pass, pushes, and learns from the forge that the gate they were
# told to trust does not cover what the forge checks. The 2026-11-02 `auto`
# branch went out that way -- green locally, red on the forge at the formatting
# step, two dozen diffs and five lints, every one of them in the new code.
#
# --all and --workspace are spelled out rather than left to the virtual root's
# defaults. They happen to be the same thing today; one `default-members` line
# in the workspace manifest would silently make them different, and a gate that
# narrows when someone edits an unrelated file is not a gate.
#
# `--check` rather than a rewrite: a gate that edited the tree would make "the
# gate is green" mean something other than "what I wrote is what is here".
rust_format() {
    cargo fmt --version >/dev/null 2>&1 || {
        echo "no rustfmt for this toolchain. Install it rather than skipping the" >&2
        echo "check: rustup component add rustfmt." >&2
        return 1
    }
    cargo fmt --all --check
}

# Its own target directory for the same reason the strict build has one:
# clippy's flags differ from a plain build's, so a shared cache rebuilds the
# world on every switch between them. --all-targets because tests are code too,
# and a lint that fires in a test is usually telling you the test is weaker
# than it looks.
rust_clippy() {
    cargo clippy --version >/dev/null 2>&1 || {
        echo "no clippy for this toolchain. Install it rather than skipping the" >&2
        echo "check: rustup component add clippy." >&2
        return 1
    }
    CARGO_TARGET_DIR=target/clippy cargo clippy --workspace --all-targets --locked --quiet -- -D warnings
}

run "rust format"      rust_format
run "rust warnings"    rust_warnings
run "rust clippy"      rust_clippy
run "shell lint"       ./tools/lint-shell.sh
run "seam guards"      ./tools/guards.sh
run "invariants"       ./tools/invariants.sh
run "manifest schema"  ./manifest/test/run.sh
run "runner decisions" ./runner/test/run.sh

# The sweeper is provider-specific and arrives with its provider. Absent is
# fine; present but unrunnable is not.
for t in cull/*/test/run.sh; do
    [ -x "${t}" ] || continue
    run "sweeper: ${t}" "${t}"
done

printf '\n'
if [ -n "${failed}" ]; then
    echo "FAILED:${failed}"
    exit 1
fi
echo "all checks passed"
