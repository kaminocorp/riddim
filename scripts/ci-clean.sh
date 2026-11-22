#!/usr/bin/env bash
# Runs the CI job (.github/workflows/ci.yml) the way a GitHub runner would, without GitHub:
# a clean Ubuntu 24.04 container, a fresh clone of the given branch, a fresh toolchain and
# cargo home, two CPU cores, and the same three steps (fmt --check, clippy, test).
#
#   scripts/ci-clean.sh                    the current branch
#   scripts/ci-clean.sh feat/p3-mix        another branch (it must be committed; nothing
#                                          uncommitted is seen)
#
# Needs podman or docker and network access to crates.io and rustup. Takes a few minutes.
# It exists because Actions could not start jobs on this repository (see decision D23): this
# is the nearest thing to a CI run that can be had without one.
set -euo pipefail
cd "$(dirname "$0")/.."

branch="${1:-$(git branch --show-current)}"
runtime="$(command -v podman || command -v docker)" || {
    echo "ci-clean.sh needs podman or docker" >&2
    exit 2
}

exec "$runtime" run --rm --memory 7g -v "$PWD:/src:ro" docker.io/library/ubuntu:24.04 bash -c '
set -uo pipefail
branch="$1"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq >/dev/null
apt-get install -y -qq curl ca-certificates git build-essential util-linux >/dev/null 2>&1
curl -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none --profile minimal >/dev/null 2>&1
. "$HOME/.cargo/env"
git config --global --add safe.directory "*"
git clone -q --branch "$branch" /src /job && cd /job || exit 90
echo "== $branch at $(git rev-parse --short HEAD), on $(taskset -c 0,1 nproc) cores"
rustup show active-toolchain || rustup toolchain install
export CARGO_TERM_COLOR=never CARGO_INCREMENTAL=0
step() {
    echo "== step: $*"
    local started=$SECONDS
    taskset -c 0,1 "$@"
    local code=$?
    echo "== result: $* -> exit $code in $((SECONDS - started)) s"
    return $code
}
step cargo fmt --all --check || exit 1
step cargo clippy --workspace --all-targets -- -D warnings || exit 2
step cargo test --workspace || exit 3
echo "== job passed: $branch"
' ci-clean "$branch"
