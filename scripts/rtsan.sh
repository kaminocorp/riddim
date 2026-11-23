#!/usr/bin/env bash
# Runs the test suite with every real-time section under LLVM's RealtimeSanitizer.
#
# RTSan intercepts allocation, locks and blocking system calls made inside a real-time context
# and stops the process with a stack trace when one happens. Our real-time sections
# (riddim_rt::guard) become RTSan contexts when riddim-rt is built with its `rtsan` feature and
# RTSAN_ENABLE=1. Calls into third-party plugin code are excluded (guard::permit).
#
# Linux and macOS only. The first run downloads a pre-built sanitizer runtime (needs curl).
# A separate target directory keeps the instrumented build from evicting the normal one.
set -euo pipefail
cd "$(dirname "$0")/.."

export RTSAN_ENABLE=1
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target/rtsan}"

# Tests that feed the engine or host real audio paths. The sanitizer aborts the test binary at
# the first violation, so a green run means none occurred.
exec cargo test --features riddim-rt/rtsan \
    -p riddim-rt -p riddim-plugin -p riddim-io -p riddim-disk -p riddim-engine "$@"
