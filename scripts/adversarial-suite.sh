#!/usr/bin/env bash
# The combined adversarial suite (M13). Runs, in one command on a clean machine
# (Rust toolchain only — no Docker, no network, no model), every deterministic
# test that stands for a §M13 adversarial deliverable: topology, hostile
# proposer, race, crash/retry, authority, usage-metering, payment-evidence/replay
# and evidence-forgery. The boundary is meant to refuse all of it and change
# nothing; a green run is that refusal, witnessed.
#
# Usage:
#   scripts/adversarial-suite.sh           # the deterministic suite (clean machine)
#   RUN_LIVE=1 scripts/adversarial-suite.sh # also the live model lanes (needs a served
#                                           # model at AGENT_BASE_URL + AGENT_MODEL)
#
# Notes for a reader:
# - dev-authority is a FEATURE, not a default. services/townhall-server's http and
#   payments test files are `#![cfg(feature = "dev-authority")]`: WITHOUT the feature
#   they compile to zero tests and pass green while testing nothing. We always pass it.
# - authority_lane.rs is deliberately NOT feature-gated (it runs the REAL resolver so
#   nothing can mint); we target it explicitly so that lane stays honest.
# - loom is a feature lane, never `--cfg loom` (a global RUSTFLAGS breaks tokio's own
#   loom branches). It is a deterministic model-checker, not a live test.
# - concurrent_webhook_advances_confirm_exactly_once is a LIB unit test in
#   townhall-store, so the bare `cargo test -p townhall-store` (lib + all integration
#   targets) is used rather than `--test payment`, which would miss it.
# - We NEVER export UPDATE_TOPOLOGY: the domain topology test regenerates the published
#   graph under that flag, which would mask a real drift between graph and domain.
set -euo pipefail
cd "$(dirname "$0")/.."

pass=0
fail=0
declare -a failed=()

# Run one labelled lane; keep going on failure so the summary names every break.
lane() {
  local label="$1"; shift
  printf '\n\033[1m=== %s ===\033[0m\n%s\n' "$label" "$*"
  if "$@"; then
    pass=$((pass + 1))
  else
    fail=$((fail + 1))
    failed+=("$label")
    printf '\033[31m--- FAILED: %s ---\033[0m\n' "$label"
  fi
}

echo "###############################################################"
echo "# M13 adversarial suite — the boundary refuses, and proves it #"
echo "###############################################################"

# ---- topology: security by structure, the graph is published (§19) ----------
lane "topology · published graph matches the domain" \
  cargo test -p townhall-domain --test topology
lane "topology · no illegal edge is reachable (deterministic adversary)" \
  cargo test -p townhall-agent --test topology_adversary

# ---- hostile proposer: same boundary surface, malicious proposals (§819) -----
lane "hostile proposer · malicious proposals refused and inert" \
  cargo test -p townhall-agent --test adversarial

# ---- evidence-forgery: structurally inert, never offered the mutation --------
lane "evidence-forgery · fabricated payment evidence is structurally inert" \
  cargo test -p townhall-agent --test adversarial_payment

# ---- crash / retry + race: process death, dropped answers, real threads ------
lane "crash/retry · the crash matrix converges under one identity" \
  cargo test -p council-client --test reconciliation
lane "race · a redelivered inbound BOOK leaves one continuation" \
  cargo test -p townhall-orchestrator --test dispatch

# ---- authority: issuance / expiry / replay / revocation / delegation ---------
lane "authority · lifecycle (in-memory) + sealing + scope" \
  cargo test -p townhall-authority --test issuance --test sealing --test scope
lane "authority · the REAL resolver mints nothing without a grant" \
  cargo test -p townhall-server --test authority_lane

# ---- usage-metering + payment-evidence + store races (real SQLite) -----------
lane "usage-metering · idempotent ledger, quotas, rate limits" \
  cargo test -p townhall-usage --test metering
lane "store · metering/payment/authority + concurrency (incl. the --lib witness)" \
  cargo test -p townhall-store

# ---- feature-gated deterministic lanes (still no external network) -----------
lane "http+payments lane · races, crash-recovery, signed/forged/replayed webhooks" \
  cargo test -p townhall-server --features dev-authority --test http --test payments
lane "loom · every interleaving of the replay window admits exactly one" \
  cargo test -p townhall-channel --features loom --test loom

# ---- live model lanes: OPT-IN only (needs a served model) --------------------
if [ "${RUN_LIVE:-}" = "1" ]; then
  echo
  echo "### live model lanes (RUN_LIVE=1) — needs AGENT_BASE_URL + AGENT_MODEL ###"
  lane "live · a prompt injection cannot bypass the boundary" \
    cargo test -p townhall-agent --features agent-live --test injection -- --nocapture
  lane "live · a topology-aware model still cannot reach an illegal outcome" \
    cargo test -p townhall-agent --features agent-live --test topology_adversary_live -- --nocapture
else
  printf '\n(skipping the live model lanes — set RUN_LIVE=1 with a served model to include them)\n'
fi

echo
echo "==============================================================="
if [ "$fail" -eq 0 ]; then
  printf '\033[32mALL %d ADVERSARIAL LANES GREEN — the boundary held.\033[0m\n' "$pass"
  exit 0
else
  printf '\033[31m%d GREEN, %d FAILED:\033[0m\n' "$pass" "$fail"
  printf '  - %s\n' "${failed[@]}"
  exit 1
fi
