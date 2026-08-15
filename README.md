# BLD Town Hall Vertical Slice

Reference implementation for the first **Boundary-Led Development (BLD)** vertical slice.

> A probabilistic component proposes; a deterministic boundary disposes.

The repository is intentionally built **dependency-first**. The deterministic kernel and town-hall domain must be independently testable before persistence, HTTP, SMS, payments, or model integration are introduced.

## Current scope

This foundation implements the first three specification milestones:

- **M0 — Workspace and quality harness**
- **M1 — Pure BLD kernel**
- **M2 — Town-hall domain in memory**

Later milestones are deliberately represented in the repository roadmap but are **not** scaffolded as fake implementations. New crates should be added only when their dependency milestone is accepted.

## Repository layout

```text
bld-townhall/
├── crates/
│   ├── bld-types/          # shared bounded/domain-neutral types
│   ├── bld-kernel/         # deterministic sequencing core
│   └── townhall-domain/    # in-memory town-hall state machine
├── docs/
│   ├── technical-spec-v0.4.2.md
│   ├── architecture.md
│   ├── state-machine.md
│   ├── decisions.md
│   └── development-roadmap.md
├── fixtures/
│   └── venues.json
└── tests/
    ├── integration/
    ├── adversarial/
    └── recovery/
```

## Core invariants

1. Proposal is data, not authority.
2. A behaviour absent in the current state resolves to `Undefined`.
3. A behaviour that exists but fails authority/policy resolves to `Denied(error)`.
4. Only validated evidence can produce a next state.
5. A failed turn does not commit authoritative state.
6. State-scoped behaviours are explicit; `Draft` deliberately has no `book()` behaviour.
7. The model/agent is outside the trusted commit path.

## Quick start

Requires a stable Rust toolchain.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

For a focused domain run:

```bash
cargo test -p townhall-domain
```

## Development rule

Do **not** start a milestone until the previous milestone's acceptance gate passes. See [`docs/development-roadmap.md`](docs/development-roadmap.md).

## Specification

The execution contract is [`docs/technical-spec-v0.4.2.md`](docs/technical-spec-v0.4.2.md).

## Status

Foundation repository only. Persistence, Axum, HumanChannel/SMS, VerifiedAuthority, zero-price usage metering, Stripe sandbox handoff, Rig, and real SMS are intentionally later milestones.
