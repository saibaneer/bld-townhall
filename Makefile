.PHONY: check test fmt clippy adversarial ci

fmt:
	cargo fmt --all

clippy:
	cargo clippy --workspace --all-targets --all-features -- -D warnings

test:
	cargo test --workspace

check: fmt clippy test

# The combined M13 adversarial suite — one command, clean machine (Rust only, no
# Docker/network/model). RUN_LIVE=1 additionally runs the live model lanes.
adversarial:
	./scripts/adversarial-suite.sh

# The full CI sequence exactly as the runner does it, in a clean container
# (fmt + clippy + the cache-poison guard + workspace/dev-authority/loom lanes).
# One command from a fresh checkout; needs Docker.
ci:
	./ci/run.sh
