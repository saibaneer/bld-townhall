-- M10: the human-payment records (ADR-030, spec §9/§17). Two tables, additive
-- only. These are the payment-SPECIFIC layer that sits ON the existing effect
-- machinery: the Pay-kind effect_intents rows are the wire/reconcile spine, and
-- these two rows are the canonical-checkout warrant and the webhook dedup ledger
-- the spec's §9 draws.
--
-- # Why a distinct PaymentIntentId, not the Pay effect id
--
-- The payment flow uses TWO Pay-kind effect intents (create the checkout, then
-- await the webhook), so no single effect id spans it. PaymentIntentId is the
-- payment's own stable identity, derived once at OfferSelected and carried in the
-- Stripe session metadata, so a webhook maps: session -> payment_intents ->
-- BookingId + the await effect-intent id whose active_effect the fact must match.
-- Binding to the session-recorded await id (not the live pointer) is what lets a
-- stale/cross-session late success be rejected by the CAS instead of advancing the
-- wrong intent.

-- One row per intended human payment. Frozen at OfferSelected with the canonical
-- checkout (amount/currency) and the AvailabilityGrant the post-payment council
-- Book re-sends — so the booking can never be placed at a fee that moved. The
-- Stripe session reference and the await effect-intent id are filled in once the
-- checkout session is created.
CREATE TABLE IF NOT EXISTS payment_intents (
    -- The stable PaymentIntentId (e.g. 'PAY-EFF-...'). Derived once; never reissued.
    payment_intent_id        TEXT PRIMARY KEY,
    -- Which booking this checkout is for. The webhook recovers it from here.
    booking_id               TEXT NOT NULL,
    -- The FROZEN verified fee, in pence. What the human is charged and what the
    -- post-payment council Book re-sends — not a currently-valid re-read.
    amount_pence             INTEGER NOT NULL,
    -- ISO 4217, lower-case (Stripe's convention), e.g. 'gbp'.
    currency                 TEXT NOT NULL,
    -- A hash of the canonical checkout: "bound to canonical checkout" (§9.1) as an
    -- integrity witness, so a tampered amount/currency cannot ride this id.
    checkout_hash            TEXT NOT NULL,
    -- The AvailabilityGrant on-the-wire form, frozen. Re-sent verbatim at the
    -- post-payment Book; if the council rejects it (expired during the human's
    -- window) the booking goes to NeedsHuman, never a silent stale book.
    frozen_grant             TEXT NOT NULL,
    -- The policy version that classified this booking as high-value. Persisted so
    -- a threshold change between verify and settle cannot re-classify (config drift).
    threshold_policy_version TEXT NOT NULL,
    -- The Stripe Checkout Session id; NULL until the session is created.
    stripe_session_id        TEXT,
    -- The hosted checkout URL delivered to the human; NULL until created.
    hosted_url               TEXT,
    -- The AWAIT Pay-kind effect intent (#2) the succeeded-webhook advances. NULL
    -- until AwaitingHumanPayment. The webhook builds its fact with THIS id so the
    -- active_effect CAS rejects stale/cross-session evidence.
    await_effect_intent_id   TEXT,
    -- 'prepared' (frozen, no session yet) | 'awaiting' (session created, human
    -- paying) | 'confirmed' (verified webhook) | 'abandoned' (terminal Stripe
    -- expiry/cancel). Transitions are conditional UPDATEs, so each is idempotent.
    status                   TEXT NOT NULL,
    -- The checkout session's expiry, on Stripe's clock (up to 24h). NULL until the
    -- session is created. PaymentAbandoned is derived ONLY from a verified Stripe
    -- terminal event, never synthesised from this column.
    expires_at_ms            INTEGER,
    created_at_ms            INTEGER NOT NULL,
    updated_at_ms            INTEGER NOT NULL
);

-- Recover the intent for a booking (its current or past checkouts).
CREATE INDEX IF NOT EXISTS idx_payment_intents_booking
    ON payment_intents(booking_id);

-- The webhook's lookup path: Stripe session id -> the intent it belongs to.
-- Partial (session set) because it is NULL until the session is created.
CREATE UNIQUE INDEX IF NOT EXISTS idx_payment_intents_session
    ON payment_intents(stripe_session_id) WHERE stripe_session_id IS NOT NULL;

-- The webhook dedup ledger (§9). Every VERIFIED provider event is a row, keyed by
-- Stripe's own event.id so a redelivered webhook is a structural no-op, not a
-- second transition. Written in the SAME transaction as the advance it drives —
-- it is audit + defence-in-depth, NEVER a skip-gate consulted before `observe`
-- (that would strand a confirmed payment if a crash fell between insert and
-- advance; the exactly-once authority is the version CAS + active_effect guard).
CREATE TABLE IF NOT EXISTS payment_events (
    -- Stripe's event.id. The dedup key: PRIMARY KEY makes redelivery a no-op.
    event_id          TEXT PRIMARY KEY,
    -- The intent this event is bound to (must match the session's metadata).
    payment_intent_id TEXT NOT NULL,
    -- e.g. 'payment_intent.succeeded', 'checkout.session.expired'.
    event_type        TEXT NOT NULL,
    -- 'verified' (signature + timestamp passed) | 'rejected' (recorded for audit;
    -- a rejected event never advances state).
    verdict           TEXT NOT NULL,
    received_at_ms    INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_payment_events_intent
    ON payment_events(payment_intent_id);
