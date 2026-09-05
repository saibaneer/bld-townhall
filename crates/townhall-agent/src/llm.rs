//! The helpful LLM proposer (M11, ADR-031).
//!
//! It asks a [`ChatModel`] for the next step, then a DETERMINISTIC parser turns
//! the model's text into a [`ProposedAction`] — or refuses it. The parser is the
//! load-bearing part of this file, and it is deliberately strict about the two
//! things the model must not decide (spec §18.1):
//!
//! - **The behaviour must be one the projection published.** A hallucinated or
//!   out-of-state behaviour never becomes a `Drive`; it collapses to `Done`. The
//!   boundary would refuse it anyway — this just keeps the proposer honest and
//!   spares a round-trip.
//! - **The model never chooses the version.** Whatever the model says, the parser
//!   pins `if_match` to the version the projection actually carried.
//!
//! So the model *chooses*; the parser *disposes*. A model that returns garbage,
//! an unreachable endpoint, or an unparseable answer all resolve to `Done` — a
//! failing model can never produce an invalid boundary call.

use crate::openai::ChatModel;
use crate::{ProjectedContext, ProposedAction, Proposer};
use async_trait::async_trait;

/// The helpful proposer over any [`ChatModel`]. Generic so the real transport and
/// a canned test responder share one code path.
pub struct LlmProposer<M: ChatModel> {
    model: M,
}

impl<M: ChatModel> LlmProposer<M> {
    #[must_use]
    pub const fn new(model: M) -> Self {
        Self { model }
    }
}

#[async_trait]
impl<M: ChatModel> Proposer for LlmProposer<M> {
    async fn propose(&self, context: &ProjectedContext) -> ProposedAction {
        let (system, user) = build_prompt(context);
        // A model that cannot be reached or fails is "no proposal this turn" — the
        // driver stops rather than the boundary receiving anything.
        let Ok(raw) = self.model.complete(&system, &user).await else {
            return ProposedAction::Done;
        };
        parse_and_validate(&raw, context)
    }
}

/// The system + user prompts. The system prompt pins the JSON contract and the
/// "you decide no authoritative fact" boundary; the user prompt is the projected
/// view. Deterministic in the context, so a test can assert against it.
fn build_prompt(context: &ProjectedContext) -> (String, String) {
    let system = "You are a booking assistant for a town-hall room booking service. \
You NEVER decide prices, resource versions, payment status, or ids — the service does. \
You only choose the next step, and only from the AVAILABLE behaviours listed. \
Reply with ONLY a single JSON object, no prose, no markdown fences, no explanation.\n\
\n\
IMPORTANT: if a Current state is shown (anything other than 'no booking yet'), the \
booking ALREADY EXISTS — do NOT create another; pick one AVAILABLE behaviour, or reply done.\n\
\n\
When there is NO booking yet, create one with EXACTLY these fields (names verbatim):\n\
{\"action\":\"create\",\"body\":{\"purpose\":\"<text>\",\"requested_date\":\"YYYY-MM-DD\",\"from\":\"HH:MM\",\"to\":\"HH:MM\",\"attendees\":<integer>,\"wheelchair_accessible\":<true|false>,\"max_fee_pence\":<integer PENCE, e.g. £50 is 5000>}}\n\
\n\
To take a behaviour from the available list:\n\
{\"action\":\"drive\",\"behaviour\":\"<one available behaviour, verbatim>\",\"body\":{...}}\n\
- SelectVenue body is {\"venue_id\":\"<id>\",\"slot_id\":\"<id>\"}, copied from a venue candidate.\n\
- VerifySlot and Book bodies are {}.\n\
- Cancel body MUST include a reason: {\"reason\":\"<short text>\"}.\n\
\n\
Keep choosing the next available behaviour until the goal is reached — do NOT reply \
done while the goal is still unreached. The states progress \
Draft -> VenueSelected -> AwaitingBooking -> Booked: 'AwaitingBooking' is NOT the end, \
you must still Book to confirm the reservation.\n\
\n\
Reply {\"action\":\"done\"} ONLY when the request is fulfilled — for a booking request, \
once the state is Booked; for a cancellation request, once it is Cancelled."
        .to_owned();

    let venues = serde_json::to_string(&context.venues).unwrap_or_else(|_| "[]".to_owned());
    let behaviours =
        serde_json::to_string(&context.available_behaviours).unwrap_or_else(|_| "[]".to_owned());
    let user = format!(
        "Request: {request}\n\
         Current state: {state}\n\
         Available behaviours: {behaviours}\n\
         Venue candidates: {venues}\n\
         Choose the next step and reply as JSON.",
        request = context.request,
        state = context
            .state
            .as_deref()
            .unwrap_or("(no booking yet — create one)"),
    );
    (system, user)
}

/// Turn the model's raw text into an action, refusing anything the model must not
/// decide. Non-JSON, an unknown action, or a behaviour the projection did not
/// offer all resolve to [`ProposedAction::Done`].
fn parse_and_validate(raw: &str, context: &ProjectedContext) -> ProposedAction {
    let Some(value) = extract_json(raw) else {
        return ProposedAction::Done;
    };
    match value.get("action").and_then(serde_json::Value::as_str) {
        Some("create") => ProposedAction::Create {
            body: value
                .get("body")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({})),
        },
        Some("drive") => {
            let Some(behaviour) = value.get("behaviour").and_then(serde_json::Value::as_str) else {
                return ProposedAction::Done;
            };
            // The parser disposes: a behaviour the projection did not publish is
            // refused HERE, never sent.
            if !context.offers(behaviour) {
                return ProposedAction::Done;
            }
            ProposedAction::Drive {
                behaviour: behaviour.to_owned(),
                body: value
                    .get("body")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
                // The model NEVER chooses the version — the parser pins the one the
                // projection carried, whatever the model wrote.
                if_match: context.version,
            }
        }
        // "done", an unknown action, or a missing one all mean: nothing to do.
        _ => ProposedAction::Done,
    }
}

/// Pull a single JSON object out of the model's text — tolerating a `<think>…</think>`
/// reasoning preamble (Qwen3 emits one) and any prose or code fence around it.
fn extract_json(raw: &str) -> Option<serde_json::Value> {
    // Drop everything up to and including a reasoning block's close, so a `{` INSIDE
    // the model's thinking cannot be mistaken for the answer.
    let body = raw
        .rfind("</think>")
        .map_or(raw, |idx| &raw[idx + "</think>".len()..]);
    let start = body.find('{')?;
    let end = body.rfind('}')?;
    if end < start {
        return None;
    }
    serde_json::from_str(body.get(start..=end)?).ok()
}

#[cfg(test)]
mod tests {
    use super::{LlmProposer, extract_json};
    use crate::openai::{ChatError, ChatModel};
    use crate::{ProjectedContext, ProposedAction, Proposer, VenueOption};
    use async_trait::async_trait;

    /// A model that returns a fixed string — or a fixed error — so the parser is
    /// tested with no network and no real model.
    struct Canned(Result<String, ()>);

    #[async_trait]
    impl ChatModel for Canned {
        async fn complete(&self, _system: &str, _user: &str) -> Result<String, ChatError> {
            self.0
                .clone()
                .map_err(|()| ChatError::Transport("canned failure".to_owned()))
        }
    }

    fn draft() -> ProjectedContext {
        ProjectedContext {
            request: "book the hall for a community meeting".to_owned(),
            state: Some("Draft".to_owned()),
            available_behaviours: vec!["SelectVenue".to_owned(), "Cancel".to_owned()],
            version: Some(2),
            venues: vec![VenueOption {
                venue_id: "TH-A".to_owned(),
                slot_id: "SLOT-A".to_owned(),
                fee_pence: 4_500,
                accessible: true,
                capacity: 30,
            }],
        }
    }

    async fn propose(response: &str, ctx: &ProjectedContext) -> ProposedAction {
        LlmProposer::new(Canned(Ok(response.to_owned())))
            .propose(ctx)
            .await
    }

    #[tokio::test]
    async fn a_valid_offered_proposal_becomes_the_right_drive() {
        let action = propose(
            r#"{"action":"drive","behaviour":"SelectVenue","body":{"venue_id":"TH-A","slot_id":"SLOT-A"}}"#,
            &draft(),
        )
        .await;
        assert_eq!(
            action,
            ProposedAction::Drive {
                behaviour: "SelectVenue".to_owned(),
                body: serde_json::json!({ "venue_id": "TH-A", "slot_id": "SLOT-A" }),
                if_match: Some(2),
            }
        );
    }

    /// THE load-bearing test: a behaviour the projection did not offer is refused
    /// by the parser — it never becomes a Drive. Mutation: drop the `offers` check
    /// and this returns a Book Drive, failing the assert.
    #[tokio::test]
    async fn an_unoffered_behaviour_is_refused_not_driven() {
        let ctx = draft();
        assert!(!ctx.offers("Book"));
        let action = propose(r#"{"action":"drive","behaviour":"Book","body":{}}"#, &ctx).await;
        assert_eq!(
            action,
            ProposedAction::Done,
            "the model proposed Book, which Draft does not offer — it must not be driven"
        );
    }

    #[tokio::test]
    async fn the_model_never_chooses_the_version() {
        // The model tries to pin a bogus version; the parser must ignore it and use
        // the projection's own version (§18.1).
        let action = propose(
            r#"{"action":"drive","behaviour":"SelectVenue","body":{},"version":999,"if_match":999}"#,
            &draft(),
        )
        .await;
        let ProposedAction::Drive { if_match, .. } = action else {
            panic!("expected a Drive");
        };
        assert_eq!(
            if_match,
            Some(2),
            "the version is the projection's, never the model's 999"
        );
    }

    #[tokio::test]
    async fn non_json_prose_is_a_safe_no_op() {
        let action = propose("Sure! I think you should book the hall now.", &draft()).await;
        assert_eq!(action, ProposedAction::Done);
    }

    #[tokio::test]
    async fn a_thinking_preamble_is_stripped_before_parsing() {
        // The <think> block even contains a stray '{' — the strip must not let it
        // hijack extraction.
        let action = propose(
            "<think>The user wants a venue. Maybe {something}?</think>\n\
             {\"action\":\"drive\",\"behaviour\":\"SelectVenue\",\"body\":{}}",
            &draft(),
        )
        .await;
        assert!(
            matches!(action, ProposedAction::Drive { ref behaviour, .. } if behaviour == "SelectVenue"),
            "the real proposal after </think> must be parsed, got {action:?}"
        );
    }

    #[tokio::test]
    async fn an_unreachable_model_is_a_safe_no_op() {
        let action = LlmProposer::new(Canned(Err(()))).propose(&draft()).await;
        assert_eq!(
            action,
            ProposedAction::Done,
            "a failed model call never reaches the boundary"
        );
    }

    #[tokio::test]
    async fn a_create_is_recognized_before_a_booking_exists() {
        let mut ctx = draft();
        ctx.state = None;
        ctx.available_behaviours.clear();
        let action = propose(
            r#"{"action":"create","body":{"purpose":"meeting","attendees":20}}"#,
            &ctx,
        )
        .await;
        assert_eq!(
            action,
            ProposedAction::Create {
                body: serde_json::json!({ "purpose": "meeting", "attendees": 20 }),
            }
        );
    }

    #[test]
    fn extract_json_finds_the_object_amid_fences_and_prose() {
        let value = extract_json("here you go:\n```json\n{\"action\":\"done\"}\n```\nthanks")
            .expect("a JSON object is in there");
        assert_eq!(value["action"], "done");
    }
}
