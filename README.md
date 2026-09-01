# BLD Town Hall Vertical Slice

Reference implementation for the first **Boundary-Led Development (BLD)** vertical slice.

> A probabilistic component proposes; a deterministic boundary disposes.

The repository is intentionally built **dependency-first**. The deterministic kernel and town-hall domain must be independently testable before persistence, HTTP, SMS, payments, or model integration are introduced.

## Current scope

Implemented, milestone by milestone (each gated on the previous — spec §21):

- [x] **M0 — Workspace and quality harness**
- [x] **M1 — Pure BLD kernel**
- [x] **M2 — Town-hall domain in memory**
- [x] **M3 — Durable aggregate + optimistic concurrency**
- [x] **M4 — External effect protocol** (the real mock council, crash matrix, pursuit/reconciliation, in-flight cancellation; all 25 required failure-injection tests mapped and audited in [`docs/m4-acceptance.md`](docs/m4-acceptance.md))
- [x] **M5 — Axum BLD service** (ETag/If-Match preconditions bound inside the trusted turn, spec §10.2 status mapping, the reconciler loop, the whole journey possible with curl alone)
- [x] **M5.1 — Resource visibility and authoritative lookup** (not a spec milestone: bookings acquire an owner, so a principal can no longer cancel a booking they merely know the id of; `?booking_ref=` / `?cancellable=true` give `CANCEL <ref>` the authoritative lookup spec §14.1 requires — [ADR-022](docs/decisions.md))
- [ ] **M6 — HumanChannel core + SMS simulator**
- [ ] **M7 — Approval + VerifiedAuthority**
- [ ] **M8–M13** — usage metering, discovery, payment handoff, Rig, real SMS, hardening

Later milestones are deliberately represented in the repository roadmap but are **not** scaffolded as fake implementations. New crates should be added only when their dependency milestone is accepted.

## Repository layout

```text
bld-townhall/
├── crates/
│   ├── bld-types/          # shared bounded/domain-neutral types
│   ├── bld-kernel/         # deterministic sequencing core (the three doors)
│   ├── townhall-domain/    # town-hall state machine + durable aggregate shape
│   ├── townhall-store/     # SQLite/SQLx repository, CAS + audit + pursuit axis
│   ├── townhall-service/   # the coordinator, the reconciler, the BookingApi facade
│   ├── townhall-http/      # the Axum adapter — can name no store or provider crate
│   ├── council-wire/       # one signed encoder, shared by both sides
│   └── council-client/     # the council over HTTP, wearing the boundary's traits
├── services/
│   ├── mock-council/       # the authoritative external world, built to be killed
│   └── townhall-server/    # the composition root: wiring, DevAuthority, READY <port>
├── docs/
│   ├── technical-spec-v0.4.2.md
│   ├── architecture.md
│   ├── state-machine.md
│   ├── decisions.md
│   ├── development-roadmap.md
│   ├── m3-persistence.md
│   └── m4-effects-guidance.md
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
cargo test -p townhall-store
```

## Development rule

Do **not** start a milestone until the previous milestone's acceptance gate passes. See [`docs/development-roadmap.md`](docs/development-roadmap.md).

## Specification

The execution contract is [`docs/technical-spec-v0.4.2.md`](docs/technical-spec-v0.4.2.md).

## Status

M0–M5 implemented, plus M5.1. The next milestone is M6: the channel-agnostic HumanChannel with the local SMS simulator — the first consumer of the escalated-question queue ADR-019 left waiting for a human who can actually act. Approval/VerifiedAuthority issuance (M7, replacing the dev-authority stand-in in the composition root), usage metering, discovery, Stripe, Rig, and real SMS remain later milestones.

The decision record is [`docs/decisions.md`](docs/decisions.md) (ADR-001–022): the spec is never edited, and the ADRs are the amendment trail against it.


## M3 / M4 engineering notes

Read [`docs/m3-persistence.md`](docs/m3-persistence.md) before modifying persistence. Before starting external provider work, read [`docs/m4-effects-guidance.md`](docs/m4-effects-guidance.md).
