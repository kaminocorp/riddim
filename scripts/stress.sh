#!/usr/bin/env bash
# Runs the stress test for a while: a busy project at a 64-frame buffer with render threads,
# graph churn, buffer-size changes, a slow disk under playback and a slower one under
# recording (crates/riddim-engine/tests/stress.rs).
#
#   scripts/stress.sh            60 seconds, under RealtimeSanitizer
#   scripts/stress.sh 600        ten minutes
#   RIDDIM_STRESS_PLAIN=1 scripts/stress.sh 60
#                                without the sanitizer (our own allocation guard still runs)
#
# The vision's "sustained" real-time-safety runs are this, repeated; it is kept out of CI to
# stay inside the Actions budget.
set -euo pipefail
cd "$(dirname "$0")/.."

export RIDDIM_STRESS_SECONDS="${1:-60}"
if [ -z "${RIDDIM_STRESS_PLAIN:-}" ]; then
    export RTSAN_ENABLE=1
    export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target/rtsan}"
    exec cargo test --features riddim-rt/rtsan -p riddim-engine --test stress -- --nocapture
else
    exec cargo test -p riddim-engine --test stress -- --nocapture
fi
