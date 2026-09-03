#!/usr/bin/env bash
# Run the CI steps the way the runner does — Linux, non-root, runner's tools —
# before pushing. See ci/Dockerfile for why this exists.
#
# Usage:  ci/run.sh            # the full sequence
#         ci/run.sh --fast     # skip fmt/clippy, tests only
#
# It copies the tree into the container's own filesystem (excluding target/), so
# it never touches your local build and never runs a stale macOS binary. First
# run builds the image and compiles cold; after that the image is cached.
set -euo pipefail

cd "$(dirname "$0")/.."
IMAGE=bld-ci:local

docker build -q -t "$IMAGE" ci/ >/dev/null

# The one CI-cache behaviour that bit us, reproduced on purpose: before the
# no-feature test lane, leave a FEATURE-enabled townhall-server at the shared
# path `target/debug/townhall-server`, exactly as a prior run's feature lane
# does through rust-cache. A test whose correctness survives this is a test that
# does not trust the shared path.
POISON='cargo build -p townhall-server --features dev-authority'

FAST="${1:-}"
STEPS='
  if [ "'"$FAST"'" != "--fast" ]; then
    echo "=== fmt ==="   ; cargo fmt --all -- --check
    echo "=== clippy ===" ; cargo clippy --workspace --all-targets --all-features -- -D warnings
  fi
  echo "=== poison the shared binary (reproducing the CI cache) ==="
  '"$POISON"'
  echo "=== workspace lane ==="       ; cargo test --workspace
  echo "=== dev-authority lane ==="   ; cargo test -p townhall-server --features dev-authority
  echo "=== loom lane ==="            ; cargo test -p townhall-channel --features loom --test loom
  echo "=== ALL LANES GREEN ==="
'

# Read-only source mount; the copy and build happen on the container's own fs.
docker run --rm -v "$PWD":/src:ro "$IMAGE" bash -c '
  set -euo pipefail
  cp -r /src/crates /src/services /src/Cargo.toml /src/Cargo.lock \
        /src/rust-toolchain.toml /src/docs /work/ 2>/dev/null || true
  cd /work
  '"$STEPS"'
'
