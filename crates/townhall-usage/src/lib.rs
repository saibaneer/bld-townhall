//! The trusted usage-metering component: it reserves, debits and releases
//! zero-price usage units, and it cannot touch a booking or a grant.
//!
//! Spec §5/§16 grade the usage ledger as a trusted resource-accounting component
//! that "may reserve/meter/release units; no booking mutation". ADR-025's crate-
//! graph discipline puts that grading in the dependency list, exactly as it does
//! for `townhall-authority`: this crate sits BELOW `townhall-domain`, names no
//! mutation surface, no socket and no connection pool, and no envelope or key —
//! a usage unit is £0 and grants NOTHING, so there is nothing to sign and no
//! authority to widen (ADR-027).
//!
//! # What this crate refuses to make easy
//!
//! Confusing a unit with a permission. A successful reserve or debit returns a
//! [`store::Balance`] and nothing an authority check would read — no grant, no
//! reference. The SQL implementation of [`store::UsageStore`] lives in
//! `townhall-store`; the in-memory one here is for this crate's own tests and the
//! composition roots' test doubles, never a fallback.

pub mod service;
pub mod store;

pub use service::{PricingSchedule, UsageDenied, UsagePolicy, UsageService};
pub use store::{Balance, MemoryUsageStore, StoreError, UsageStore};
