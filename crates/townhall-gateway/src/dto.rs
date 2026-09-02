//! The wire's shapes, written from the outside.
//!
//! Deliberately not shared with `townhall-http`. A field renamed on both sides
//! at once would break nothing and prove nothing; written twice, the round-trip
//! tests are a real check that the contract is what both ends believe.

use serde::Deserialize;

/// One booking as the wire reports it.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Projection {
    pub id: String,
    pub version: u64,
    pub state: String,
    pub requirements: Requirements,
    pub selected_venue: Option<SelectedVenue>,
    pub booking_ref: Option<String>,
    pub available_behaviours: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Requirements {
    pub purpose: String,
    pub requested_date: String,
    pub from: String,
    pub to: String,
    pub attendees: u16,
    pub wheelchair_accessible: bool,
    pub max_fee_pence: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct SelectedVenue {
    pub venue_id: String,
    pub slot_id: String,
}

/// One catalogue row as `/venues` actually reports it.
///
/// The first version of this struct had a `name` field the wire has never sent
/// and was missing two it does — and stayed green, because nothing exercised
/// it. An independently written DTO only tests the contract when a test drives
/// it; an undriven one is just a second place to be wrong.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct VenueRow {
    pub venue_id: String,
    pub slot_id: String,
    pub capacity: u16,
    pub accessible: bool,
    pub fee_pence: u64,
    pub available: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct SlotFacts {
    pub venue_id: String,
    pub slot_id: String,
    pub capacity: u16,
    pub accessible: bool,
    pub fee_pence: u64,
    pub available: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct AuditRow {
    pub driver_kind: String,
    pub driver_detail: String,
    pub outcome: String,
    pub from_version: u64,
    pub to_version: u64,
    pub at_ms: i64,
}

/// What the server sent back, before it is classified.
///
/// Kept as a value rather than classified inline because two statuses each cover
/// two unrelated situations, and telling them apart needs the headers and the
/// body together — not the number alone.
#[derive(Clone, Debug)]
pub struct RawResponse {
    pub status: u16,
    pub etag: Option<u64>,
    pub retry_after: Option<u64>,
    pub request_id: Option<String>,
    pub body: serde_json::Value,
}

impl RawResponse {
    /// Whether the body carries a domain error *name* rather than prose.
    ///
    /// This is the distinguisher that separates the two 422s and the two 503s.
    /// A domain denial answers `{"error": "FeeExceededAuthority", "detail": …}`
    /// and carries an `ETag`; a malformed request answers `{"error": "<prose>"}`
    /// with no `ETag`. Keying on the status alone would tell Lucy her request was
    /// malformed when the council had merely gone quiet — a wrong answer, to a
    /// person, about whose fault it was.
    #[must_use]
    pub fn is_domain_denial(&self) -> bool {
        // All three or nothing: the ETag, the error NAME as a string, and the
        // detail. Requiring only two let `422 + ETag + {"detail"}` with no name
        // become a `Denied("(no error field)")` — a refusal quoting an error
        // that was never sent.
        self.etag.is_some()
            && self
                .body
                .get("error")
                .and_then(serde_json::Value::as_str)
                .is_some()
            && self.body.get("detail").is_some()
    }

    #[must_use]
    pub fn error_text(&self) -> String {
        self.body
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("(no error field)")
            .to_owned()
    }
}
