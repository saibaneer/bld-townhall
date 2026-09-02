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
use townhall_channel::InboundMessage;

/// The strict grammar:
///
/// - `BOOK date=YYYY-MM-DD from=HH:MM to=HH:MM people=N accessible=yes|no
///   max=PENCE` — all six keys, any order, nothing else.
/// - `CONFIRM` — finish the most recent booking. Deliberately a bare word M7
///   will replace with its challenge flow; the doc on [`Request::Confirm`] says
///   so, so nobody mistakes the stand-in for the design.
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
    async fn propose(&self, _context: &ProjectedContext, message: &InboundMessage) -> Proposed {
        let text = message.body.revealed().trim();

        if text.eq_ignore_ascii_case("confirm") {
            return Proposed::Typed(Request::Confirm);
        }
        if text.eq_ignore_ascii_case("cancel it") {
            return Proposed::Typed(Request::CancelIntent);
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

        let Ok(people) = fields["people"].parse::<u16>() else {
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
