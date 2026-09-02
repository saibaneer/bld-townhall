//! The scripted conversation runner — the demo and the acceptance test are the
//! same file BECAUSE they are the same function.
//!
//! The `sms-simulator` binary parses a script and calls [`run`]; M6's gate test
//! parses the same script and calls the same [`run`]. A demo that drifted from
//! the test would be a demo that lies, and this milestone's gate IS "a scripted
//! SMS conversation" — so the script is the deliverable and this is its one
//! interpreter.
//!
//! # Script format
//!
//! ```text
//! # comment
//! > +447700900123 BOOK date=2026-09-10 ...   an inbound message
//! < Maximum booking fee                       the next REPLY, to that sender
//! <! Booked. Council ref                      the next AUTOMATED message
//! !followups                                  drain the follow-up queue
//! ```
//!
//! Every expectation consumes exactly one outbound message, in order, and
//! checks THREE things: the text contains the fragment, the recipient is the
//! current sender, and the class matches the arrow (`<` is a `Reply`, `<!` is
//! `Automated`). The PR review found the first version checking only the text —
//! under which every reply misdelivered to the wrong phone, or every reply sent
//! as `Automated` (and therefore silenceable by someone else's STOP), passed
//! the gate verbatim.

use crate::dispatcher::Dispatcher;
use std::sync::atomic::{AtomicUsize, Ordering};
use townhall_channel::{
    ChannelAddress, ChannelKind, InboundIdentity, RawInbound, Region, SmsSimulator,
    TransportEvidence,
};

#[derive(Debug)]
enum Step {
    Inbound {
        from: String,
        body: String,
    },
    Expect {
        fragment: String,
        class: townhall_channel::OutboundClass,
    },
    Followups,
}

/// A parsed script.
#[derive(Debug)]
pub struct Script {
    steps: Vec<Step>,
}

impl Script {
    /// # Errors
    /// A line that is neither a comment, an inbound, an expectation, nor a
    /// directive — scripts are executable claims, and an unparseable line is a
    /// claim nobody will ever check.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut steps = Vec::new();
        for (number, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("> ") {
                let (from, body) = rest
                    .split_once(char::is_whitespace)
                    .ok_or_else(|| format!("line {}: '>' needs a sender and a body", number + 1))?;
                steps.push(Step::Inbound {
                    from: from.to_owned(),
                    body: body.trim().to_owned(),
                });
            } else if let Some(fragment) = line.strip_prefix("<! ") {
                steps.push(Step::Expect {
                    fragment: fragment.to_owned(),
                    class: townhall_channel::OutboundClass::Automated,
                });
            } else if let Some(fragment) = line.strip_prefix("< ") {
                steps.push(Step::Expect {
                    fragment: fragment.to_owned(),
                    class: townhall_channel::OutboundClass::Reply,
                });
            } else if line == "!followups" {
                steps.push(Step::Followups);
            } else {
                return Err(format!("line {}: unparseable: {line:?}", number + 1));
            }
        }
        Ok(Self { steps })
    }
}

/// Run a script to completion, or say exactly where the conversation diverged.
///
/// # Errors
/// The first divergence, with the transcript position: a missing reply, an
/// extra one, an out-of-order one, or an unroutable script address.
pub async fn run(
    dispatcher: &Dispatcher<SmsSimulator>,
    channel: &SmsSimulator,
    script: &Script,
    region: Region,
) -> Result<(), String> {
    // Message identities are unique across EVERY run in this process, not per
    // script — two runs share the channel's replay window, and a second script
    // whose first message reused "turn-1" was silently deduped as a carrier
    // retry. Found by the gate's own fault leg going quiet.
    static NEXT_IDENTITY: AtomicUsize = AtomicUsize::new(0);

    let mut consumed = channel.outbox().len();
    let mut turn = 0_usize;
    let mut sender: Option<ChannelAddress> = None;

    for step in &script.steps {
        match step {
            Step::Inbound { from, body } => {
                // Every previous turn's replies must have been consumed before
                // the next inbound — an unconsumed message is an EXTRA reply,
                // and letting it slide would let the shape drift.
                let outbox = channel.outbox();
                if outbox.len() > consumed {
                    return Err(format!(
                        "extra reply before turn {turn}: {:?}",
                        outbox[consumed].text
                    ));
                }
                turn += 1;
                let raw = RawInbound {
                    identity: InboundIdentity::new(
                        "sim",
                        "script",
                        format!("script-{}", NEXT_IDENTITY.fetch_add(1, Ordering::SeqCst)),
                    ),
                    channel: ChannelKind::SmsSimulator,
                    from: from.clone(),
                    body: body.clone(),
                    received_at_ms: 0,
                    evidence: TransportEvidence::new("sim", from, true),
                };
                dispatcher
                    .handle(raw)
                    .await
                    .map_err(|error| format!("turn {turn}: channel failure: {error}"))?;
                // The script address must be real, or the silence that follows
                // would read as a missing reply with the wrong culprit — and it
                // becomes the recipient every expectation is checked against.
                sender = Some(
                    ChannelAddress::parse(from, region)
                        .map_err(|error| format!("turn {turn}: script address: {error}"))?,
                );
            }
            Step::Expect { fragment, class } => {
                let outbox = channel.outbox();
                let Some(sent) = outbox.get(consumed) else {
                    return Err(format!(
                        "turn {turn}: missing reply — expected one containing {fragment:?}"
                    ));
                };
                if !sent.text.contains(fragment) {
                    return Err(format!(
                        "turn {turn}: out-of-order or wrong reply — expected {fragment:?}, \
                         got {:?}",
                        sent.text
                    ));
                }
                if sent.class != *class {
                    return Err(format!(
                        "turn {turn}: wrong class — {fragment:?} arrived as {:?}, expected {class:?}",
                        sent.class
                    ));
                }
                match &sender {
                    Some(expected_to) if &sent.to == expected_to => {}
                    _ => {
                        return Err(format!(
                            "turn {turn}: misdelivered — {fragment:?} went to {:?}",
                            sent.to
                        ));
                    }
                }
                consumed += 1;
            }
            Step::Followups => {
                dispatcher.run_followups().await;
            }
        }
    }

    // Trailing extras are drift too.
    let outbox = channel.outbox();
    if outbox.len() > consumed {
        return Err(format!(
            "extra reply after the script ended: {:?}",
            outbox[consumed].text
        ));
    }
    Ok(())
}
