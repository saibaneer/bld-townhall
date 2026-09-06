# Known limitations

§M13 requires that the boundary's limits be **stated in the open**, not discovered.
This is that register: each entry is a place where the POC deliberately stops short
of a production system, why the boundary is still sound despite it, and where the
decision is recorded. A limitation listed here is a *choice with a reason*, not a
bug — a bug is something that violates a property the spec claims; a limitation is a
property the spec never claimed, named so no reader mistakes its absence for an
oversight.

Stream D of M13 extends this register; the entries below are the ones settled so far.

---

## 1. Transport `verified` / `signature` on inbound evidence are recorded, not enforced

**What.** An inbound `InboundEvidenceRecord` (an SMS/Telegram reply that answers a
challenge, or a control message that revokes) carries transport-supplied `verified`
and `signature` fields. They are written to the evidence row as audit metadata and
are **never read as a gate**: a deposit whose `verified` is `false` answers a
challenge exactly as one whose `verified` is `true`.

**Why this is sound.** The spec grades this transport metadata precisely. §3.2 rates
*"SMS provider metadata"* as *"Transport evidence; useful but not high-assurance
identity by itself"*, may-mutate-authoritative-state = **No**. The actual gate in
front of every consequential transition is **provenance, not the flag** (§14,
"valid because of provenance, not shape"):

- **Binding resolution** — a deposit from a sender that does not resolve to a *live
  channel binding* is refused (`ApprovalDenied::WrongChannel`) before any row is
  stored. A forged `verified: true` on an unbound sender never lands.
- **The one-time code** — answering a challenge requires the code sent *to the bound
  holder's own device*, attempt-limited (`ApprovalError::WrongCode { attempts_left }`).
  An attacker who does not hold the device cannot produce it, whatever the transport
  claims.

So a `verified: false` deposit bypasses nothing: it must already be on a bound
channel **and** still produce a code it does not have.

**Why not enforce it anyway.** Gating on `verified == true` would *promote* transport
metadata to an authority signal — the precise thing §3.2 forbids and §2 warns against
("the communication channel does not create authority"). It would also encode a false
assurance: the M7B/M7C relay cannot cryptographically bind an inbound SMS sender, which
is the definition of `AssuranceLevel::SmsReply`. A higher-assurance channel (wallet /
passkey adapter, §M-later) would carry real cryptographic evidence and *would* be
validated — but that is a different assurance level, added without changing the kernel.

**Recorded in.** ADR-034; spec §3.2, §14.

**Scope.** This concerns only the transport `verified` / `signature` fields on inbound
challenge/control evidence. Forged *identity* is defended (forged `claimed_sender` →
binding resolution; forged approval → the code; field-perfect forged *payment* evidence
→ provenance binding + dedupe, §14 / M10) and is covered by the M13 forgery tests.

---

## 2. Real SMS / WhatsApp is gated behind UK telecom compliance; the live channel is Telegram

**What.** The `twilio-client` crate is a proven SMS/WhatsApp adapter, but reaching a
real UK phone over it requires an unbounded chain of telecom compliance — proven live,
each gate clearing only to reveal the next: trial message-template restriction (572006)
→ Trust Hub KYC (20003) → a UK **regulatory bundle** for any UK mobile number → and US
numbers that cannot route SMS to the UK at all (21612). The live end-to-end demo runs
over **Telegram** instead.

**Why this is sound.** Each gate is telephony bureaucracy with its own review delay;
none is a property of the BLD boundary, which is channel-agnostic (§3.2: the channel is
transport, not authority). Telegram is a real two-way human channel to a real device and
proves exactly the property §M12 protects. `twilio-client` is retained unchanged for
whenever that compliance is cleared — nothing is discarded.

**Recorded in.** ADR-032, ADR-033.

---

## 3. Observability is the audit trail + request-id correlation, not distributed tracing

**What.** The POC carries no `tracing` / `tracing-subscriber` crate and emits no
structured (JSON) logs or distributed spans. Its observability is three concrete,
deliberately-scoped things:

- **The `audit_events` trail** — every consequential transition is recorded with its
  identifiers, versions, provenance (which *fact* or *proposal* drove it), outcome and
  effect reference. This is the substantive, replayable record of what the boundary did,
  and the reconciler keys its retries on it. It is witnessed in the store's Phase-C tests
  (e.g. `a_confirmation_commits_state_status_and_provenance_together`).
- **`X-Request-ID` correlation** — the HTTP `request_id` middleware echoes a caller-supplied
  id or mints one, and always rides it back on the response, so a request and its answer
  share one handle. This holds across the gateway hop (a caller's id survives to the server
  and back into `Gateway::last_request_id`) and on a **refused** response, not only a
  successful one. Witnessed by `headers_and_browse_behave`,
  `a_refused_request_still_carries_its_request_id` (server) and
  `a14b_request_ids_survive_the_round_trip` (gateway).
- **Opt-in request logging** — `TOWNHALL_LOG_REQUESTS` turns on a one-line
  `[id] method path -> status` to stderr for a demo or debugging session; a normal
  run (tests, CI) stays silent.

**Why this is sound.** The spec's observability requirement is the **audit trail**
(§ audit_events / provenance), which is present and witnessed; request-id correlation is
the operational handle that ties an external request to its audit rows. Distributed
tracing (spans, a collector, cross-service trace propagation) is production operations
infrastructure, not a property of the deterministic boundary — adding it would change no
boundary behaviour. It can be layered in later at the composition root without touching
the kernel or domain, exactly as the channel and model-hosting substitutions were
(ADR-031/033).

**Recorded in.** M13 stream C (owner decision: document, do not build). Spec: audit_events
/ provenance.
