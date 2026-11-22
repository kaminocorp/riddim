#!/usr/bin/env bash
# Runs what .github/workflows/ci.yml runs, and the heavier checks that are kept out of CI to
# stay inside the Actions budget. Run it before every push.
#
#   scripts/ci.sh                 everything that needs no extra setup
#   RIDDIM_TEST_VST3="/path/A.vst3;/path/B.vst3" RIDDIM_TEST_CLAP="/path/A.clap" scripts/ci.sh
#                                 also run the third-party conformance test on those bundles,
#                                 and bounce a demo song through them
#   RIDDIM_SKIP_RTSAN=1 scripts/ci.sh
#                                 skip the RealtimeSanitizer run (it downloads a runtime the
#                                 first time, and only exists for Linux and macOS)
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

# The core must stay portable: nothing outside the platform backends may assume macOS or Unix.
if rustup target list --installed 2>/dev/null | grep -q '^x86_64-pc-windows-msvc$'; then
    step "clippy (x86_64-pc-windows-msvc, type-check only)"
    cargo clippy --workspace --all-targets --target x86_64-pc-windows-msvc -- -D warnings
else
    echo "note: x86_64-pc-windows-msvc target not installed; skipping the Windows type-check"
fi

step "tests"
cargo test --workspace

if [ -n "${RIDDIM_TEST_VST3:-}${RIDDIM_TEST_CLAP:-}" ]; then
    step "third-party conformance run"
    cargo test -p riddim-plugin --test third_party -- --nocapture
    step "third-party demo song"
    cargo test -p riddim-engine --test third_party -- --nocapture
else
    echo "note: neither RIDDIM_TEST_VST3 nor RIDDIM_TEST_CLAP is set; no third-party plugins were tested"
fi

if [ -z "${RIDDIM_SKIP_RTSAN:-}" ]; then
    step "RealtimeSanitizer"
    scripts/rtsan.sh --quiet
else
    echo "note: RIDDIM_SKIP_RTSAN set; skipping the RealtimeSanitizer run"
fi

if command -v cargo-deny >/dev/null 2>&1; then
    step "cargo-deny"
    cargo deny --all-features check
else
    echo "note: cargo-deny not installed; skipping the licence check"
fi

printf '\nAll checks passed.\n'
