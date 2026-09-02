//! Which *kind* of thing a message is — and nothing about what it means.

/// A channel-control command (spec §14.1). Answerable without a booking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlCommand {
    Help,
    Balance,
    Stop,
    Start,
    /// Recognized and deferred with an answer, not silently ignored.
    ///
    /// Spec §2 lists REVOKE among the operations that must never be unavailable,
    /// and to a user an unrecognized safety command is indistinguishable from a
    /// broken system. So it parses, and the dispatcher says "delegations arrive
    /// with M7" rather than falling through to "I didn't understand".
    Revoke,
}

/// A command that names a resource, so it cannot be answered here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResourceCommand {
    /// `CANCEL TH-92718` — the reference as **text**. No lookup is attempted;
    /// this crate cannot reach anything that could perform one.
    Cancel { reference: String },
    /// `STATUS` or `STATUS TH-92718`.
    Status { reference: Option<String> },
}

/// What the channel decided a message is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Answered by the dispatcher from its ports. Never reaches a booking.
    Control(ControlCommand),
    /// Answered by the dispatcher, via an authoritative lookup.
    Resource(ResourceCommand),
    /// Everything else — including `BOOK date=… people=…`.
    ///
    /// The channel does not parse booking fields. Spec §14 forbids a
    /// `HumanChannel` owning "booking state, policy, authority or model
    /// decisions", and a grammar that knows what an attendee count is owns
    /// booking vocabulary. Classifying is the whole job; the proposer reads the
    /// fields, in the position M11's model will occupy. That is what keeps this
    /// crate replaceable by a `WhatsApp` or voice adapter.
    Freeform,
}

/// Classify a body.
///
/// # Strict, and that is the discriminating property
///
/// The whole trimmed body must be the command. `"STOP the booking"` is **not**
/// `STOP` — it is a sentence about a booking, and a `contains("stop")` reading
/// would silently swallow a business request as a channel control. Every
/// malformed command falls through to `Freeform`, where something that can ask a
/// question handles it.
#[must_use]
pub fn classify(body: &str) -> Command {
    let trimmed = body.trim();
    let folded = trimmed.to_ascii_uppercase();
    let mut words = folded.split_whitespace();
    let (Some(first), rest) = (words.next(), words.collect::<Vec<_>>()) else {
        return Command::Freeform;
    };

    match (first, rest.as_slice()) {
        ("HELP", []) => Command::Control(ControlCommand::Help),
        ("BALANCE", []) => Command::Control(ControlCommand::Balance),
        ("STOP", []) => Command::Control(ControlCommand::Stop),
        ("START", []) => Command::Control(ControlCommand::Start),
        ("REVOKE", []) => Command::Control(ControlCommand::Revoke),

        // The argument is taken from the ORIGINAL text, not the case-folded
        // copy: a council reference is an opaque identifier and upper-casing it
        // would be this crate interpreting somebody else's namespace.
        //
        // But the argument must LOOK like a reference at all — see
        // `looks_like_reference` for why "CANCEL it" is not a command.
        ("CANCEL", [argument]) if looks_like_reference(argument) => {
            Command::Resource(ResourceCommand::Cancel {
                reference: original_argument(trimmed, 1),
            })
        }
        ("STATUS", []) => Command::Resource(ResourceCommand::Status { reference: None }),
        ("STATUS", [argument]) if looks_like_reference(argument) => {
            Command::Resource(ResourceCommand::Status {
                reference: Some(original_argument(trimmed, 1)),
            })
        }

        // Bare `CANCEL`, `STATUS a b`, `STOP the booking`, `BOOK …`, prose.
        _ => Command::Freeform,
    }
}

/// Whether a token could be a reference, as opposed to a word.
///
/// The rule is one clause — *contains a digit* — and deliberately no more,
/// because anything richer would be this crate learning the council's reference
/// format, which is somebody else's namespace.
///
/// What it exists to catch: spec §15.2 has Lucy texting **"Cancel it"**, which
/// is a sentence, and §14.1 requires a sentence to reach the referent-resolution
/// path where ambiguity can be ASKED about. Without this clause it parses as
/// `CANCEL` with reference `"it"`, the dispatcher looks up a booking named "it",
/// and Lucy is told it does not exist — a wrong answer produced confidently.
/// (The seam test caught exactly that before M6B was built on it.)
///
/// The cost of the heuristic missing: a genuinely digit-free reference falls
/// through to `Freeform`, where the proposer still sees the whole text and can
/// route it — a graceful degradation, against the alternative of a pronoun
/// treated as an identifier.
fn looks_like_reference(token: &str) -> bool {
    token.chars().any(|c| c.is_ascii_digit())
}

fn original_argument(trimmed: &str, index: usize) -> String {
    trimmed
        .split_whitespace()
        .nth(index)
        .unwrap_or_default()
        .to_owned()
}
