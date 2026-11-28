#!/usr/bin/env bash
# Build the manual test programs in the dev container and copy them to
# target/manual/ (ignored by git), ready to run on the host.
#
#   dev/build-manual-tests.sh
#   target/manual/capture_portal --help
#
# The programs need the host's Wayland session and xdg-desktop-portal, so they
# run on the host, not in the container. Runtime libraries they use are part
# of a normal Fedora desktop (libpipewire, libstdc++); nothing is installed.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
mkdir -p "$ROOT/target/manual"

"$HERE/run.sh" bash -c '
    set -euo pipefail
    cargo build --release -p v6cast-media --features pipewire --examples
    for example in capture_portal; do
        cp "$CARGO_TARGET_DIR/release/examples/$example" /work/target/manual/
        echo "built target/manual/$example"
    done
'
