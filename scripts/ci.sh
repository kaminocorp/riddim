#!/usr/bin/env bash
# Runs the same checks as .github/workflows/ci.yml, locally.
#
#   scripts/ci.sh                 everything that needs no extra setup
#   RIDDIM_TEST_VST3="/path/A.vst3;/path/B.vst3" scripts/ci.sh
#                                 also run the third-party conformance test on those bundles
#
# cargo-deny is used if installed (cargo install cargo-deny --locked).
set -euo pipefail
cd "$(dirname "$0")/.."

step() { printf '\n==> %s\n' "$*"; }

step "rustfmt"
cargo fmt --all --check

step "clippy (host)"
cargo clippy --workspace --all-targets -- -D warnings

if rustup target list --installed 2>/dev/null | grep -q '^aarch64-apple-darwin$'; then
    if [ "$(uname -s)" != "Darwin" ] || [ "$(uname -m)" != "arm64" ]; then
        step "clippy (aarch64-apple-darwin, type-check only)"
        cargo clippy --workspace --all-targets --target aarch64-apple-darwin -- -D warnings
    fi
else
    echo "note: aarch64-apple-darwin target not installed; skipping the macOS type-check"
fi

step "tests"
cargo test --workspace

if [ -n "${RIDDIM_TEST_VST3:-}" ]; then
    step "third-party conformance run"
    cargo test -p riddim-plugin --test third_party -- --nocapture
else
    echo "note: RIDDIM_TEST_VST3 not set; no third-party plugins were tested"
fi

if command -v cargo-deny >/dev/null 2>&1; then
    step "cargo-deny"
    cargo deny --all-features check
else
    echo "note: cargo-deny not installed; skipping the licence check"
fi

printf '\nAll checks passed.\n'
