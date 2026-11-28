#!/usr/bin/env bash
# Run a command inside the isolated v6cast build container (rootless podman).
#
#   dev/run.sh cargo test --workspace
#   dev/run.sh vainfo
#
# The image is built on first use. The repository is mounted at /work; the
# cargo registry and build output live in named volumes, so the host's target/
# directory is never touched. No sudo, no --privileged.
set -euo pipefail

IMAGE="localhost/v6cast-dev:latest"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

if ! podman image exists "$IMAGE"; then
    echo "Building $IMAGE (first run)..." >&2
    podman build -t "$IMAGE" -f "$HERE/Containerfile" "$HERE"
fi

if [ "$#" -eq 0 ]; then
    set -- bash
fi

tty_args=()
if [ -t 0 ] && [ -t 1 ]; then
    tty_args=(-it)
fi

exec podman run --rm "${tty_args[@]}" \
    --userns=keep-id \
    --device /dev/dri \
    -v "$ROOT:/work:Z" \
    -w /work \
    -v v6cast-cargo-home:/cargo-home \
    -v v6cast-target:/cargo-target \
    -e CARGO_TARGET_DIR=/cargo-target \
    -e XDG_CACHE_HOME=/tmp/cache \
    "$IMAGE" "$@"
