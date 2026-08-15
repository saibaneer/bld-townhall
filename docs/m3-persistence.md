# M3 — Durable Aggregate and Optimistic Concurrency

Status: **implemented in the foundation repository**.

M3 exists to move BLD's authoritative state from an in-memory object into a durable business object without yet introducing external side effects. The technical specification requires SQLite + SQLx for the POC, a repository abstraction, resource versions, compare-and-set commits, persisted audit events, and restart survival.

## Acceptance gate

M3 is accepted only when all of the following hold:

1. A newly created booking is persisted as `Draft`, version `0`.
2. Loading the same SQLite file through a newly created repository returns the same booking state and version.
3. Two writers based on version `N` cannot both advance the resource. Exactly one compare-and-set can move `N -> N+1`; the stale writer receives `StoreError::StaleVersion`.
4. The repository, not the caller, derives `N+1`.
5. A successful state commit and its local audit event are in the same SQLite transaction.
6. The domain remains independent of SQLx and SQLite.
7. M3 performs **no real external effects**. Booking/cancellation provider calls remain M4 work.

## Design

### Domain aggregate, storage implementation

`townhall-domain::BookingAggregate` is the durable business shape:

```text
BookingAggregate
├── BookingId
├── version
├── BookingState
├── BookingRequirements
├── selected venue
├── verified availability snapshot (when present)
├── council booking reference (when present)
├── active effect identity (reserved for M4)
└── timestamps
```

`townhall-store` owns persistence. The dependency direction is:

```text
bld-types
    ↑
townhall-domain
    ↑
townhall-store
```

The domain therefore has no SQLx dependency. Persistence can be replaced without changing the state machine.

## Why SQLite + SQLx

For the POC, SQLite gives us a real transactional database while retaining a one-file deployment and extremely small operational footprint. SQLx keeps the persistence layer async and explicit and provides a straightforward path to a later Postgres implementation behind the same repository contract.

The repository uses **runtime `sqlx::query` calls** rather than database-connected compile-time query macros in M3. That keeps clean-machine builds from requiring a prepared development database. Migrations plus integration tests are the schema contract for now. We can adopt SQLx offline query metadata later if compile-time SQL shape checking becomes worth the added workflow.

## Why database CAS instead of a Rust mutex

A process-local `Mutex` can coordinate threads in one process but does not establish durable business-object ownership. Another process, worker, server instance, or restarted process could still act on the same stale resource.

The mutation is therefore guarded where authoritative state lives:

```sql
UPDATE bookings
SET version = ?, ...
WHERE id = ? AND version = ?;
```

A transition is valid against a **specific revision** of the booking. This is the storage-level form of the BLD rule:

> A proposal is valid against a specific version of authoritative state, not against a resource forever.

M5 will expose the same concept over HTTP as `ETag` / `If-Match`; the database CAS remains the final authority.

## Why `BEGIN IMMEDIATE`

The CAS is only half the story. `commit` originally opened a *deferred* transaction, which
under WAL cannot promote its read to a write once anyone has written anywhere in the
database — and because the version `SELECT` has already opened a read transaction, SQLite
skips the busy handler entirely, so `busy_timeout` never applies.

Measured: **52 of 60** concurrent commits to *disjoint* bookings failed with "database is
locked", despite having no version contention at all. A genuine CAS loser received
`SQLITE_BUSY` rather than `StaleVersion`.

Taking the write lock at `BEGIN` fixes both. SQLite permits one writer regardless, so this
costs no concurrency; it moves the serialisation point from mid-transaction, where it
failed, to `BEGIN`, where it waits. See ADR-015 for the tradeoff it carries into M4.

## Why the repository owns version increments

Callers supply only `expected_version`. They do not submit an authoritative new version number. The repository computes `expected_version + 1` after proving that the expected revision still exists.

This prevents an adapter or caller from manufacturing a jump such as version `3 -> 99` and makes every committed change one serializable revision.

## Why snapshot JSON plus an explicit discriminator

The database stores the typed Rust state as versionable JSON and also stores `state_name` separately.

The JSON snapshot keeps M3 simple while the domain is still evolving quickly. The discriminator makes rows human-inspectable, enables future indexing/operations, and allows the loader to detect a corrupted mismatch between the discriminator and payload.

This is deliberately **not** an event-sourced architecture. `bookings` is the authoritative current snapshot; `audit_events` records committed transition history. We can change this later only through an explicit ADR/migration.

## Why audit and state commit share one transaction

A successful local transition should not leave either of these states:

```text
state advanced, audit missing
```

or

```text
audit says committed, state did not advance
```

M3 inserts the audit event inside the same SQLite transaction as the CAS update. This guarantees local atomicity.

It does **not** prove that the audit is externally immutable or that an external provider effect is true. Audit anchoring and reconciliation remain separate concerns.

## Why `active_effect` exists before M4

The aggregate reserves an optional `EffectIntentId` field because the specification already defines it as part of the durable business object. M3 does not create or execute effect intents. Keeping the field now avoids reshaping the aggregate when M4 arrives, while still preserving the dependency gate: **no provider side effect occurs in M3**.

## Tests

`townhall-store` includes tests for:

- create + reload after repository restart;
- one CAS winner and deterministic stale-writer rejection;
- audit persistence coupled to the state change;
- duplicate resource creation rejection.

Before M4 begins, run:

```bash
cargo test -p townhall-store
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

## Known M3 boundary

M3 deliberately does not solve:

- provider idempotency;
- network timeouts;
- effect-intent ownership;
- booking reconciliation;
- cancellation while an external booking is in flight;
- multi-worker leasing/reconciliation scheduling.

Those belong to M4 and must not be hidden inside repository methods.
