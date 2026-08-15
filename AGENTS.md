# Coding Agent Instructions

This repository implements the BLD Town Hall technical specification in `docs/technical-spec-v0.4.2.md`.

Before making architectural changes, read `docs/bld-implementation-guide.md`. It is the normative guide explaining why BLD exists, what must remain true, and the implementation rules that must not be weakened.

## Execution protocol

- Work milestone-by-milestone.
- Do not start milestone N+1 until N's acceptance gate passes.
- Do not weaken a boundary to simplify a later integration.
- Surface contradictions in the specification instead of silently choosing a new architecture.
- Keep model, HTTP, SMS and payment dependencies out of the kernel/domain unless the spec explicitly moves that responsibility.
- Every consequential mutation needs deterministic tests, not merely an end-to-end happy path.

## Current repository milestone

M0–M3 foundation. M3 durable aggregate + optimistic concurrency is implemented in `townhall-store`. The next implementation target is M4 external effect protocol. Read `docs/m3-persistence.md` and `docs/m4-effects-guidance.md` before changing storage or adding provider calls.
