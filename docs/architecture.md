# Architecture

## Current executable slice (M0–M2)

```mermaid
flowchart LR
    P[Proposal] --> K[BLD Kernel]
    A[Verified Authority fixture] --> K
    S[Current Booking State] --> K
    C[Authoritative Context fixture] --> K
    K --> R{Resolution}
    R -->|Undefined| U[No transition]
    R -->|Denied| D[Typed denial]
    R -->|Ready plan| E[Fake capability]
    E --> V[Evidence validation]
    V --> N[Next state]
```

The initial repository stops here on purpose. No HTTP, database, SMS provider, payment provider, or model is necessary to prove M1/M2 semantics.

## Target vertical slice

```mermaid
flowchart TD
    H[Human / feature phone] -->|SMS| HC[HumanChannel]
    HC --> O[Human Orchestrator]
    O --> A[Approval / Authority]
    O --> R[Rig Agent]
    R --> C[BLD Client]
    C --> S[Axum Town Hall BLD Service]
    S --> K[BLD Kernel]
    K --> D[Town Hall Domain]
    D --> CAP[Scoped Capabilities]
    CAP --> MC[Mock Council]
    S --> DB[(Repository / Audit)]
    S --> P[Stripe Sandbox Checkout]
    P -->|Verified provider evidence| S
    HC --> U[Zero-price Usage Ledger]
```

## Trust rule

The proposer can select **which declared behaviour to propose**, but it cannot:

- mutate booking state;
- create transition edges;
- choose authoritative fee/provider parameters;
- mint authority;
- declare external evidence valid;
- commit state.
