# RFC — A BLD MCP: help other teams map their domain and replicate the boundary

**Status:** Direction approved (owner) · **Audience:** Rust-first · **Home:** its **own repo** (depends on the kernel, does not live in the town-hall workspace) · **Scope:** productization adjacent to the POC, not a spec amendment (so this note lives here, not in `decisions.md`).

## 1. What this is for

The POC proves one reusable thing wrapped in one throwaway thing. The reusable thing is
the **boundary discipline**: a deterministic core (`bld-kernel`) over which a probabilistic
component proposes and cannot bypass; behaviour belongs to states; the legal/illegal graph is
*published* so an adversary already has it; and an adversarial suite proves the boundary holds.
The throwaway thing is town-hall booking.

The goal of a BLD MCP is to let **another developer's agent** replicate the reusable thing in
*their* domain — loan approval, content moderation, order fulfilment, escrow — without
re-deriving the pattern from scratch. At minimum (the owner's stated floor): help them **map
their domain and build out their topology**. The topology is the right anchor because it is
where BLD's distinctive value lives — "security by structure, not obscurity" — and because it
can be produced *before any code exists*.

## 2. Why this is a fit, not a stretch (grounded in the code)

- **The kernel is already the reusable part.** `bld-kernel` names no booking in its code; the
  only "townhall" strings are doc-comment examples. `BoundaryDomain`
  ([`crates/bld-kernel/src/lib.rs:293`](../crates/bld-kernel/src/lib.rs)) — 9 associated types
  (`State`, `Proposal`, `Effect`, `Authority`, `Context`, `ProviderFact`, `SystemEvent`,
  `FactContext`, `Error`) plus the three doors `resolve_proposal` / `resolve_fact` /
  `resolve_system_event` — *is* the seam another domain implements. It is publishable as-is.
- **The topology was designed for exactly this.** [`tests/topology.rs`](../crates/townhall-domain/tests/topology.rs)
  states it: *"For an implementation, tests holding the line is enough. For a principle others
  adopt, the graph is the deliverable,"* and it derives the graph through the **public API only**
  precisely so *"an external toolchain (a diagram renderer, a synthesis step) needs exactly this
  access."*
- **"The match IS the topology"** ([`bld-kernel/src/lib.rs:483`](../crates/bld-kernel/src/lib.rs)).
  The graph is not a second source of truth to maintain — it falls out of the resolvers, which
  is what makes it automatable.

## 3. The artifact we already know how to produce

[`docs/topology.json`](topology.json) is the target shape. Any BLD domain's topology is:

- `states`: the state list.
- `doors`: `proposal` (intent), `fact` (verified external truth), `system_event` (deterministic
  runtime fact) — each with its `inputs` and a `fixed_table` flag (true = decided by
  `(state, input)` alone; false = the door reads persisted data, so `dimensions` names the axes
  varied).
- `cells`: one per `(state, input)`, each an `edge` of:
  - `local` — a state-only transition (`to`);
  - `external` — a transition that carries an `effect` (`to` + `effect`);
  - `no_edge` — the transition **does not exist** (the input is `Undefined` here).

The one guarantee the artifact enforces is **totality**: every `(state, input)` pair has a cell,
so no input sequence can reach a transition nobody specified. An illegal transition is *absent*,
not guarded — the difference between "a rule enforced at runtime" and "a path that does not exist."

The kernel's outcome vocabulary the cells encode: `Undefined` (no behaviour → `no_edge`),
`Denied(e)` (behaviour exists but a guard refused), `Ready(plan)` / `Committed(s)` (permitted).

## 4. The one design decision, decided: spec-driven, Rust-first

Rust cannot enumerate an enum's variants generically, so today the graph is derived by a
**bespoke loop welded to town-hall's types** in `topology.rs`. To generalize, the developer
describes their domain as **data** — a declarative spec — and the tool:

1. **validates** the topology from the spec (totality + illegal-edge structure), and
2. **generates** the Rust `BoundaryDomain` skeleton and the adversarial harness from the spec.

This is the key move: the spec lets us help them **map the domain before a line of Rust exists**,
which is the owner's floor. Rust-first means the spec *targets* a real `impl BoundaryDomain`
against the published `bld-kernel` crate — the determinism guarantees stay intact — and a later
step verifies the generated/edited code still matches the spec (§7, Stage 4).

### 4.1 The domain spec (sketch, not final)

```yaml
domain: loan-approval
states: [Submitted, UnderReview, Approved, Declined, Disbursed]
initial: Submitted

# The three doors. Every (state, input) pair MUST appear, or `validate` fails on totality.
proposal:                 # intent — what someone WANTS
  inputs: [StartReview, Approve, Decline, Disburse]
  edges:
    - { from: Submitted,   input: StartReview, to: UnderReview }          # local
    - { from: UnderReview, input: Approve,     to: Approved }             # local (guarded by authority)
    - { from: UnderReview, input: Decline,     to: Declined }
    - { from: Approved,    input: Disburse,    to: Disbursed, effect: SendFunds }  # external
    # …every other (state,input) is `no_edge` — the validator lists what you omitted

fact:                     # verified external truth — what the WORLD confirmed
  fixed_table: false      # this door reads the persisted plan
  inputs: [FundsSent]
  edges:
    - { from: Disbursing, input: FundsSent, to: Disbursed }

system_event:             # deterministic runtime fact — retries, timeouts
  inputs: [DisburseTimedOut]
  edges:
    - { from: Disbursing, input: DisburseTimedOut, records: escalate }
```

`validate` turns "you never said what `Declined → Disburse` is" into an error, and reports it as
`no_edge` (absent, uncrossable) — not a guard someone could get wrong. `render` emits the same
`topology.json` shape + a diagram for review.

## 5. What the MCP exposes

- **Resources** (teach the pattern to any agent): the `BoundaryDomain` contract, the ten core
  invariants (README), the town-hall domain as a worked reference, and the load-bearing ADRs
  (001 kernel-knows-no-domain, 012 recovery-without-a-model, 019 pursuit-vs-outcome).
- **Tools** (thin wrappers over a `bld` CLI — see §6):
  - `bld.map_domain` — from a natural-language description, draft the spec (states, doors, edges).
  - `bld.topology.validate` — totality + illegal-edge soundness on a spec.
  - `bld.topology.render` — `topology.json` + a diagram.
  - `bld.scaffold.domain` — generate the `impl BoundaryDomain` skeleton from the spec.
  - `bld.scaffold.adversarial` — generate the topology-adversary + hostile-proposer harness.
- **Prompts** (guide the human): a "map your domain" interview — *what are your states? what does
  each state let someone do? which transitions reach out to the world (effects)? what is the
  authority, and what evidence closes the loop?*

## 6. Where the logic lives: a `bld` CLI, MCP as a thin skin

The real work belongs in a `bld` CLI (`bld topology validate|render`, `bld scaffold
domain|adversarial`) so **humans and agents call the same thing** and it is testable on its own.
The MCP server is a thin adapter exposing those commands as tools plus the resources/prompts.
Server language: Rust (`rmcp`) so it can link the BLD libraries and the CLI directly; the CLI is
the source of truth either way.

## 7. Staged plan (value lands at Stage 1)

| Stage | Deliverable | Delivers |
|---|---|---|
| **1** | Domain-spec format + `bld topology validate` + `bld topology render` | The owner's floor: **map a domain, get the topology, prove totality/illegal-edge soundness** — before any code |
| **2** | `bld scaffold domain` + `bld scaffold adversarial` | A compiling `BoundaryDomain` skeleton + their adversarial suite |
| **3** | The MCP server (`rmcp`) wrapping the CLI + resources + the map-your-domain prompt | Any agent can drive it |
| **4** | A `TopologyProbe` trait (`all_states()`, `all_inputs()`) + `bld topology verify`; **publish `bld-kernel`** (§10.4) | Proves the *shipped Rust* still matches the spec — closes the loop the way `topology.rs` does for town-hall — and this is the first stage that links the kernel, so it is when publishing pays off |

## 8. What needs extraction vs what is ready

| Piece | State |
|---|---|
| `bld-kernel` + `BoundaryDomain` | **Ready** — needs a publish + a public-API stability pass |
| Topology derivation | **Extract** — bespoke test code today → spec-validator (Stage 1) + `TopologyProbe` (Stage 4) |
| Scaffolding + adversarial generation | **New** — spec-driven templates |
| `bld` CLI + MCP server | **New** — small once the above exist |

## 9. Non-goals (for the first cut)

- **Not language-agnostic.** The kernel's determinism guarantees are Rust-typed; a
  cross-language version is a separate, fuzzier effort. Rust-first keeps the guarantees honest.
- **Not a runtime.** The MCP helps you *build and prove* a boundary; it does not host one.
- **Not the persistence/HTTP/authority layers.** Those are town-hall's concrete choices; the MCP
  scaffolds the domain + topology + adversarial suite, and points at the town-hall crates as the
  reference for wiring the rest.
- **No spec edits.** This introduces nothing into `technical-spec-v0.4.2.md`.

## 10. Resolved decisions (owner review, this iteration)

1. **Home of the code — its own repo.** The MCP + `bld` CLI live in a **new repository**, not as
   workspace members here. They need `bld-kernel` only at Stage 4 (§7); Stages 1–3 operate on the
   YAML spec alone, so the new repo is standalone until then (git-depends on the kernel when Stage
   4 lands, or the published crate once it exists).
2. **Spec surface — YAML, and as generic as possible.** The domain spec is YAML (§4.1). The
   vocabulary stays domain-neutral; concrete domains (loan-approval, town-hall) appear only as
   **inline example comments** in the spec template and the generated skeleton — guidance an
   adopting agent can follow — never baked into the format.
3. **Scaffolding — generic skeleton + guidance comments.** `scaffold.domain` emits the most
   generic `impl BoundaryDomain` it can (types + `todo!()` guards), with example comments showing
   how a real domain fills each seam, rather than domain-specific stubs. Same for the adversarial
   harness.
4. **Publishing `bld-kernel` — deferred, and not a blocker.** Stages 1–3 never link the kernel
   (pure YAML → `topology.json` + generated text); only Stage 4 does. Publishing waits until Stage
   4 or the first real adopter, after one API-stability pass. Mechanics for then: crates.io forbids
   path deps, so `bld-types` publishes first (as a version), then `bld-kernel`; licensing is
   already dual MIT/Apache (`LICENSE-APACHE` + `LICENSE-MIT`).

### Benefits of eventually publishing the kernel (why Stage 4 ends in a publish)
Frictionless adoption (`bld-kernel = "0.1"`, no git URLs or repo access); a semver-stable
`BoundaryDomain` contract adopters can pin; discoverability + rendered docs on docs.rs; and the
MCP's generated `Cargo.toml` referencing a real version so an adopter's scaffold compiles at once.
The cost that justifies deferral: versions are permanent and the name is a public commitment, so
the API-stability pass must come first.
