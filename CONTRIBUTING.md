# Contributing

## Boundary-first review checklist

Before merging a new behaviour or transition, answer:

1. Which source state owns the behaviour?
2. Is every other state/proposal pairing explicitly `Undefined` or intentionally handled?
3. Which authority is required, and does it arrive independently of proposal text?
4. Which consequential parameters are reloaded/derived from authoritative context?
5. Which capability can the plan reach?
6. What evidence proves the effect happened?
7. What stable intent identity survives retries?
8. What happens if the process dies immediately before/after the external effect?
9. Which version/CAS guard prevents stale work committing?
10. Which hostile-proposer test proves the invariant?

Do not merge a feature solely because the happy path works.
