# Dependency-Ordered Development Roadmap

The specification is authoritative. This file is the concise implementation checklist.

| Milestone | Depends on | Gate |
|---|---|---|
| M0 Workspace | — | workspace layout, formatting/lint/test commands |
| M1 Kernel | M0 | Undefined/Denied/Committed sequencing proven |
| M2 Domain | M1 | exhaustive topology + in-memory booking/cancel |
| M3 Persistence | M2 | CAS/versioning; state survives restart |
| M4 Effects | M3 | stable effect intent + retry/reconciliation |
| M5 Axum | M4 | curl can drive flow; stale `If-Match` => 412 |
| M6 HumanChannel | M5 | SMS simulator drives deterministic flow, no LLM |
| M7 Authority | M6 | challenge/replay/expiry/constraints enforced |
| M8 Usage | M6 | £0 metering + idempotent `UsageIntentId` |
| M9 Discovery/client | M5,M7 | generic client drives discovered behaviours |
| M10 Human payment | M5,M7 | Stripe sandbox evidence gates continuation |
| M11 Rig/model independence | M8,M9,M10 | local/open model completes flow; hostile proposer contained |
| M12 Real SMS | M6,M7,M11 | feature-phone happy path + dedupe |
| M13 Hardening | All | happy + adversarial suites pass on clean machine |

## Rule

Never add a later-layer dependency into an earlier crate merely to make a demo easier.
