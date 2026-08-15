# Architecture Decision Log

## ADR-001 — Kernel stays domain-agnostic

The BLD kernel owns sequencing semantics only. Town-hall policy stays in `townhall-domain`.

## ADR-002 — Domain has no HTTP/SMS/model dependency

The domain must run in unit tests with fake context/capabilities. Axum, HumanChannel and Rig are later adapters.

## ADR-003 — Proposal is not authority

Authority is passed separately from proposal data. A proposal cannot grant itself permission.

## ADR-004 — State-scoped behaviours

A state exposes only behaviours that exist there. Invalid state/proposal pairs are `Undefined`, not generic policy failures.

## ADR-005 — External effects are not implemented in M2

M2 uses fake deterministic evidence. Durable effect intents, idempotency and reconciliation arrive in M4 after persistence exists.

## ADR-006 — Open-source model is a replaceable proposer

Rig/model integration is deliberately late (M11). No model is needed to establish kernel/domain safety properties.


## ADR-007 — Durable authority lives in the database, not a process mutex

M3 uses database compare-and-set (`id + expected version`) as the serialization point. Process-local locks may be used for performance later but cannot replace the durable version check.

## ADR-008 — Repository owns version advancement

Callers provide an expected version and next business snapshot. The repository derives exactly `N + 1`; adapters may not manufacture resource versions.

## ADR-009 — Current snapshot + append-oriented local audit

`bookings` is the authoritative current aggregate snapshot. `audit_events` is append-oriented history. Both are updated in the same local SQLite transaction. This is not event sourcing and does not claim external audit anchoring.

## ADR-010 — No network effects in M3

M3 persists an optional active-effect field but does not execute provider calls. M4 must persist a stable effect intent before any external consequence and must reconcile ambiguous outcomes.

## ADR-011 — SQLite is a POC adapter, not a protocol dependency

The repository trait isolates persistence. SQLite + SQLx is selected for the POC's single-file durability and real transactions; a later Postgres adapter must preserve the same CAS and atomic-audit semantics.

## ADR-015 — `commit` uses `BEGIN IMMEDIATE`

`SqliteBookingRepository::commit` opened a deferred transaction, read the version, then
updated. Two measured consequences on the real code:

| | DEFERRED (before) | `BEGIN IMMEDIATE` |
|---|---|---|
| concurrent commits to **disjoint** bookings | **52 of 60 failed** | 0 of 60 |
| loser of a genuine CAS race | `SQLITE_BUSY` | `StaleVersion` |

The disjoint-booking row is the real defect: those commits have no version contention
whatsoever — different resources, different rows — and roughly half to seven-eighths of
them failed outright with "database is locked", with no retry.

**Cause.** `commit` unconditionally writes, so a deferred begin buys nothing. It takes no
lock; the version `SELECT` opens a read transaction; the `UPDATE` must then promote that
read to a write. Under WAL a deferred transaction cannot promote once anyone has written
anywhere in the database. Worse, because `inTransaction` is already `TRANS_READ` the busy
handler is skipped, so `busy_timeout` never applies and the call fails immediately.

**Decision.** Take the write lock at `BEGIN`. SQLite permits only one writer regardless, so
this costs no real concurrency — it moves the serialisation point from mid-transaction,
where it failed, to `BEGIN`, where it waits.

This also restores the three-outcome model at the storage layer. A lost race is now a clean
`Denied(StaleVersion)` rather than an opaque infrastructure error, which is what M5 needs to
map `ETag`/`If-Match` to 412 (versus 503 for genuine infrastructure failure), and what M4's
reconciliation workers need in order to know whether to retry.

### The tradeoff, recorded so it is not rediscovered as a regression

`IMMEDIATE` holds a **database-wide** write lock for the whole transaction, under
`busy_timeout(5s)`. Today every transaction is short and local, so this is free. If a
transaction ever spans a network call, the failure mode changes character: `DEFERRED` fails
fast on the offending path, whereas `IMMEDIATE` turns it into every unrelated commit
blocking five seconds. The symptom becomes "the service stalls in five-second steps" rather
than "this one booking errored".

The protection is structural rather than advisory (ADR-014): the repository's
prepare/finalize methods return *committed* state, leaving no signature through which a
capability could be invoked while a transaction is open.

### Deliberately not done

Do **not** remap `SQLITE_BUSY_SNAPSHOT` to `StaleVersion` in `commit`. The WAL snapshot is
database-wide, so a commit on booking Y invalidates a concurrent commit's snapshot on
unrelated booking X. Remapping would tell that caller "your version is stale" when it is
perfectly current — a false `Denied` is a lie about authoritative state, and worse than an
honest infrastructure error.

Do **not** drop the pre-`UPDATE` `SELECT`. It is not redundant: it supplies `from_state` and
`created_at_ms` for the audit row.

`if result.rows_affected() != 1` is unreachable under SQLite WAL — a writer holding the
write lock has a valid snapshot, so the row still matches — but it stays for the anticipated
Postgres port, where READ COMMITTED re-reads per statement.
