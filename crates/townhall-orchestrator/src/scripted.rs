//! The deterministic proposer — a grammar wearing the model's seat.
//!
//! M6's gate says the conversation works *"without … LLM"*, so this seat is
//! filled by something that cannot pretend: a strict structured grammar. The
//! §15.2 verbatim natural-language line is **M11's** gate, where a model takes
//! this seat through the same trait; a hand-fitted "natural language parser"
//! here would pass that test early and prove nothing (the standing rule).

use crate::ports::{BookingRequest, ProjectedContext, Proposed, Proposer, Request};
use async_trait::async_trait;
use std::collections::HashMap;
use townhall_channel::Utterance;

/// The strict grammar:
///
/// - `BOOK date=YYYY-MM-DD from=HH:MM to=HH:MM people=N accessible=yes|no
///   max=PENCE` — all six keys, any order, nothing else.
/// - `YES <code>` / `NO <code>` — approve or decline a pending challenge. The
///   grammar only CLASSIFIES; the code after the word is the deterministic
///   dispatcher's to read, so this seat never carries it (ADR-026).
/// - `cancel it` — case-insensitive, exactly — the cancel-intent phrase whose
///   referent is resolved authoritatively downstream.
///
/// Everything else is [`Proposed::Unclear`], including near-misses: a `BOOK`
/// missing a key or carrying an unknown one is a request half-understood, and
/// acting on half an understanding is the failure mode this whole layer exists
/// to refuse.
#[derive(Debug, Default)]
pub struct ScriptedProposer;

#[async_trait]
impl Proposer for ScriptedProposer {
    async fn propose(&self, _context: &ProjectedContext, utterance: &Utterance) -> Proposed {
        let text = utterance.body.revealed().trim();

        if text.eq_ignore_ascii_case("cancel it") {
            return Proposed::Typed(Request::CancelIntent);
        }
        // A reply to a challenge: the first word decides YES or NO. The code that
        // may follow it is not this seat's business.
        match text.split_whitespace().next() {
            Some(word) if word.eq_ignore_ascii_case("yes") => {
                return Proposed::Typed(Request::Approve);
            }
            Some(word) if word.eq_ignore_ascii_case("no") => {
                return Proposed::Typed(Request::Decline);
            }
            _ => {}
        }

        let mut words = text.split_whitespace();
        if !words
            .next()
            .is_some_and(|first| first.eq_ignore_ascii_case("book"))
        {
            return Proposed::Unclear;
        }

        let mut fields: HashMap<&str, &str> = HashMap::new();
        for word in words {
            let Some((key, value)) = word.split_once('=') else {
                return Proposed::Unclear;
            };
            // A repeated key is two competing answers; refusing beats picking.
            if fields.insert(key, value).is_some() {
                return Proposed::Unclear;
            }
        }

        let expected = ["date", "from", "to", "people", "accessible", "max"];
        if fields.len() != expected.len() || !expected.iter().all(|key| fields.contains_key(key)) {
            return Proposed::Unclear;
        }

        // The formats the contract advertises are the formats it checks — the
        // review found "date=tomorrow from=noon people=0" sailing through as a
        // typed request, and nothing downstream compensates.
        if !looks_like_date(fields["date"])
            || !looks_like_time(fields["from"])
            || !looks_like_time(fields["to"])
        {
            return Proposed::Unclear;
        }
        let Ok(people @ 1..) = fields["people"].parse::<u16>() else {
            return Proposed::Unclear;
        };
        let Ok(max_pence) = fields["max"].parse::<u64>() else {
            return Proposed::Unclear;
        };
        let accessible = match fields["accessible"] {
            value if value.eq_ignore_ascii_case("yes") => true,
            value if value.eq_ignore_ascii_case("no") => false,
            _ => return Proposed::Unclear,
        };

        Proposed::Typed(Request::Book(BookingRequest {
            date: fields["date"].to_owned(),
            from: fields["from"].to_owned(),
            to: fields["to"].to_owned(),
            people,
            accessible,
            max_pence,
        }))
    }
}

/// `YYYY-MM-DD`, by shape — four digits, dash, two, dash, two. Shape only:
/// whether the 31st of February exists is the council's business, not a
/// grammar's.
fn looks_like_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && [0, 1, 2, 3, 5, 6, 8, 9]
            .iter()
            .all(|&i| bytes[i].is_ascii_digit())
}

/// `HH:MM`, by shape.
fn looks_like_time(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 5 && bytes[2] == b':' && [0, 1, 3, 4].iter().all(|&i| bytes[i].is_ascii_digit())
}
