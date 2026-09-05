# BLD Town Hall Vertical Slice

Reference implementation for the first **Boundary-Led Development (BLD)** vertical slice.

> A probabilistic component proposes; a deterministic boundary disposes.
> The communication channel does not create authority. Funding does not create authority.
> Only independently verified authority plus current authoritative state can permit a consequential transition.

The repository is built **dependency-first**: the deterministic kernel and town-hall domain are independently testable before persistence, HTTP, SMS, authority, metering, discovery, payments, or model integration are introduced. New crates are added only when their dependency milestone is accepted — later milestones are never scaffolded as fake implementations.

## Current scope

Implemented, milestone by milestone (each gated on the previous — spec §21):

- [x] **M0 — Workspace and quality harness**
- [x] **M1 — Pure BLD kernel** — deterministic `Undefined` / `Denied` / `Committed`; the three doors (proposal, fact, system-event)
- [x] **M2 — Town-hall domain in memory** — explicit states, state-scoped behaviours, typed plans/evidence/errors
- [x] **M3 — Durable aggregate + optimistic concurrency** — SQLite repository, versions, CAS, audit, restart survival
- [x] **M4 — External effect protocol** — the real mock council, effect identity, in-flight states, crash matrix, pursuit/reconciliation ([`docs/m4-acceptance.md`](docs/m4-acceptance.md))
- [x] **M5 — Axum BLD service** — ETag/If-Match bound inside the trusted turn, §10.2 status mapping, the reconciler loop, the whole journey possible with `curl` alone
- [x] **M6 — HumanChannel core + SMS simulator** — a scripted SMS conversation creates, reads and cancels a booking with no telecom and no LLM (M6A gateway + M6B conversation)
- [x] **M7 — Approval + VerifiedAuthority** — SMS approval challenges that mint a `VerifiedAuthority` with an assurance level; a delegation envelope with an authentication tag; issuance/expiry/replay/revocation checks; the dev-authority stand-in retired from the composition root (M7A–M7C, [ADR-025/026](docs/decisions.md))
- [x] **M8 — Zero-price usage metering** — a `UsageIntentId`-idempotent ledger, quotas, safety exits (M8-1); per-principal/channel rate limits + a global provider budget (M8-2, [ADR-028](docs/decisions.md))
- [x] **M9 — Discovery + BLD client** — a signed `/.well-known/bld` manifest and a **generic** client that drives the API from it, with no hard-coded behaviour URLs ([ADR-029](docs/decisions.md))
- [x] **M10 — Human payment handoff** — a Stripe sandbox Checkout flow where only a **signature-verified webhook** advances a booking; the agent never touches money; availability becomes a verified fact (Option A). Forged webhook → refused; duplicate → idempotent. Verified live against real Stripe ([ADR-030](docs/decisions.md))
- [x] **M11 — Rig agent, model independence, hostile proposer** — an **untrusted** proposer entirely outside the boundary; a real open-weight model books a room; the deterministic hostile twin and a live prompt injection are refused by the boundary regardless of the model ([ADR-031](docs/decisions.md))
- [ ] **M12 — Real SMS adapter** — one real provider webhook/REST adapter, E.164 normalization, delivery + dedupe
- [ ] **M13 — Adversarial hardening + release**

## Repository layout

```text
bld-townhall/
├── crates/
│   # The generic, reusable BLD core (names no town hall)
│   ├── bld-kernel/            # deterministic sequencing core (the three doors)
│   ├── bld-types/             # shared bounded/domain-neutral types
│   ├── bld-manifest/          # the signed discovery manifest shape
│   ├── bld-client/            # a generic BLD client — discovers + drives any service
│   │
│   # This application: the town-hall booking domain + its service
│   ├── townhall-domain/       # the state machine — `impl BoundaryDomain for TownHallDomain`
│   ├── townhall-service/      # the coordinator, the reconciler, the BookingApi facade
│   ├── townhall-store/        # SQLite/SQLx repository, CAS + audit + pursuit axis
│   ├── townhall-http/         # the Axum adapter — can name no store or provider crate
│   ├── townhall-gateway/      # the untrusted HTTP driver (independently-written DTOs)
│   ├── townhall-orchestrator/ # SMS conversation routing + approval issuing
│   │
│   # Authority, channel, metering
│   ├── townhall-authority/    # approval challenges → VerifiedAuthority; the delegation envelope
│   ├── townhall-channel/      # the channel-agnostic HumanChannel core
│   ├── townhall-usage/        # the zero-price usage ledger + quotas + budget
│   │
│   # External-world adapters (capabilities), wearing the boundary's traits
│   ├── council-wire/          # one signed encoder, shared by both sides
│   ├── council-client/        # the council over HTTP
│   ├── stripe-client/         # the Stripe Checkout adapter (untrusted; mints no facts)
│   ├── townhall-payment/      # the trusted payment verifier (HMAC signature → fact)
│   ├── townhall-effects-router/ # composite Capability/Verifier routing by effect type
│   │
│   # The probabilistic half (M11), strictly outside the boundary
│   ├── townhall-agent/        # the untrusted proposer + the deterministic hostile twin
│   │
│   └── townhall-testkit/      # spawns real binaries for the integration lanes
├── services/
│   ├── mock-council/          # the authoritative external world, built to be killed
│   ├── mock-stripe/           # a hermetic Stripe Checkout double
│   ├── sms-simulator/         # the local SMS provider double
│   └── townhall-server/       # the composition root: wiring, READY <port>
├── docs/
│   ├── technical-spec-v0.4.2.md   # the execution contract (never edited)
│   ├── decisions.md               # the ADR amendment trail (ADR-001–031)
│   ├── architecture.md · state-machine.md · development-roadmap.md
│   └── m3-persistence.md · m4-effects-guidance.md
└── fixtures/venues.json
```

## Core invariants

1. Proposal is data, not authority.
2. A behaviour absent in the current state resolves to `Undefined`.
3. A behaviour that exists but fails authority/policy resolves to `Denied(error)`.
4. Only validated evidence can produce a next state; `Unknown` is never success.
5. A failed turn does not commit authoritative state.
6. State-scoped behaviours are explicit; `Draft` deliberately has no `Book` behaviour.
7. Every consequential mutation is version-checked; stale work loses the right to commit.
8. Effects have stable identity; retries reuse the same effect intent id.
9. **Money moves only on independently verified provider evidence** (a signed Stripe webhook) — never on an agent's claim or a success redirect.
10. **The proposer — including an LLM — is untrusted and outside the commit path.** The boundary refuses out-of-menu, over-authority, stale, and forged proposals *regardless of the model*. A prompt injection can manipulate the model; it cannot bypass the boundary.

## Quick start

Requires a stable Rust toolchain.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace                                  # the hermetic suite (no network)
cargo test -p townhall-server --features dev-authority  # the HTTP + payment E2E lanes
```

The canonical pre-merge gate reproduces the CI lanes in Docker (fmt, clippy, workspace, dev-authority, loom):

```bash
ci/run.sh            # the full sequence
ci/run.sh --fast     # tests only
```

Two **opt-in** lanes reach real external services and are excluded from CI:

```bash
# The real Stripe sandbox (M10 adapter):
STRIPE_SECRET_KEY=sk_test_… cargo test -p stripe-client --features stripe-live

# A real model completing the booking journey, and a live prompt injection (M11):
AGENT_MODEL=glm-5.3:cloud cargo test -p townhall-agent --features agent-live -- --nocapture
```

The agent's model is pure configuration (`AGENT_BASE_URL` / `AGENT_MODEL`) over any OpenAI-compatible endpoint — a local Ollama model, a cloud open-weight model, or the from-scratch reference all swap without changing an invariant.

## Development rule

Do **not** start a milestone until the previous milestone's acceptance gate passes. See [`docs/development-roadmap.md`](docs/development-roadmap.md).

## Status

**M0–M11 complete and merged to `main`.** The vertical slice now proves its full thesis end to end: a real AI model can be useful (it books a room) while the deterministic boundary retains consequential authority — under real money (M10) and a live prompt injection (M11).

A telling signal of the design: **`bld-kernel` has not changed since M4.** Seven later milestones — payment, an LLM agent, SMS, authority, metering, discovery — all bolted on without the deterministic core moving; a new external-effect type (Stripe) and an untrusted AI proposer both slotted into the same `impl BoundaryDomain` seam.

Remaining: **M12** (a real SMS provider adapter) and **M13** (adversarial hardening + release).

## Specification and decisions

The execution contract is [`docs/technical-spec-v0.4.2.md`](docs/technical-spec-v0.4.2.md) — it is **never edited**. Amendments and every consequential engineering decision live in [`docs/decisions.md`](docs/decisions.md) (ADR-001–031), the amendment trail against the spec.
