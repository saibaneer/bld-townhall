//! The SMS provider, in-process — inspectable, and able to fail on purpose.

use crate::{
    ChannelAddress, ChannelConfig, ChannelError, HumanChannel, InboundBody, InboundMessage,
    MessageReceipt, OutboundClass, OutboundMessage, RawInbound, ReplayWindow, Seen,
    SuppressionStore, body,
};
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

/// One message the simulator was asked to send, as it went out.
#[derive(Clone, PartialEq, Eq)]
pub struct Sent {
    pub to: ChannelAddress,
    pub text: String,
    pub class: OutboundClass,
    pub receipt: MessageReceipt,
}

/// The outbox record redacts like everything else: the address is already
/// masked by its own `Debug`, and the text becomes a length. Tests that need
/// the delivered text read the field; logs never should.
impl std::fmt::Debug for Sent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Sent {{ to: {:?}, class: {:?}, text: <{} chars>, receipt: {:?} }}",
            self.to,
            self.class,
            self.text.chars().count(),
            self.receipt
        )
    }
}

/// Suppression kept in memory. M6B swaps in a durable one.
#[derive(Debug, Default)]
pub struct InMemorySuppression {
    silenced: Mutex<HashSet<String>>,
}

impl SuppressionStore for InMemorySuppression {
    fn is_suppressed(&self, address: &ChannelAddress) -> bool {
        self.silenced
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(address.revealed())
    }
    fn suppress(&self, address: &ChannelAddress) -> Result<(), String> {
        self.silenced
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(address.revealed().to_owned());
        Ok(())
    }
    fn allow(&self, address: &ChannelAddress) -> Result<(), String> {
        self.silenced
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(address.revealed());
        Ok(())
    }
}

/// An in-process SMS provider.
///
/// Nothing here is a stand-in for behaviour the real thing has and this lacks —
/// the point is the opposite. Every §15.1 behaviour that matters is *here*,
/// where it can be driven deterministically: dedupe, bounds, segmentation,
/// suppression, and delivery failure. M12's adapter implements the same trait
/// and inherits every test.
#[derive(Debug)]
pub struct SmsSimulator {
    config: ChannelConfig,
    window: ReplayWindow,
    suppression: Arc<dyn SuppressionStore>,
    outbox: Mutex<Vec<Sent>>,
    /// Sends to fail, by address, until the count runs out.
    armed_failures: Mutex<HashMap<String, usize>>,
    /// The simulator's own clock, so no test ever sleeps.
    now_ms: AtomicI64,
}

impl SmsSimulator {
    /// # Panics
    /// On a configuration [`ChannelConfig::validated`] refuses — a simulator
    /// that can only send truncation markers should not construct.
    #[must_use]
    pub fn new(config: ChannelConfig, suppression: Arc<dyn SuppressionStore>) -> Self {
        let config = config.validated().expect("a satisfiable channel config");
        Self {
            window: ReplayWindow::new(config.replay_window_ms),
            config,
            suppression,
            outbox: Mutex::new(Vec::new()),
            armed_failures: Mutex::new(HashMap::new()),
            now_ms: AtomicI64::new(0),
        }
    }

    /// Everything sent so far, in order.
    #[must_use]
    pub fn outbox(&self) -> Vec<Sent> {
        self.outbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Fail the next `count` sends to this address.
    ///
    /// §15.1 requires delivery failure to be represented explicitly, which means
    /// a test has to be able to *cause* one. A simulator that could only succeed
    /// would leave that requirement asserted and unexercised.
    pub fn fail_next_sends(&self, to: &ChannelAddress, count: usize) {
        self.armed_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(to.revealed().to_owned(), count);
    }

    /// Move the simulator's clock, for replay-window boundaries.
    pub fn advance_ms(&self, delta: i64) {
        self.now_ms.fetch_add(delta, Ordering::SeqCst);
    }

    fn now(&self) -> i64 {
        self.now_ms.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn config(&self) -> ChannelConfig {
        self.config
    }
}

#[async_trait]
impl HumanChannel for SmsSimulator {
    type Address = ChannelAddress;

    async fn receive(&self, raw: RawInbound) -> Result<InboundMessage, ChannelError> {
        // Order matters, and this is the order: normalize, bound, then dedupe.
        //
        // Deduping first would spend a window entry on a message that turns out
        // to be unroutable, and — worse — would let a malformed flood evict real
        // entries from the window.
        let address = ChannelAddress::parse(&raw.from, self.config.default_region)?;
        let body = InboundBody::parse(&raw.body)?;

        if self.window.insert_if_absent(&raw.identity, self.now()) == Seen::Duplicate {
            return Err(ChannelError::Duplicate);
        }

        Ok(InboundMessage {
            identity: raw.identity,
            channel: raw.channel,
            address,
            received_at_ms: raw.received_at_ms,
            body,
            transport_evidence: raw.evidence,
        })
    }

    async fn send(
        &self,
        to: &Self::Address,
        message: OutboundMessage,
    ) -> Result<MessageReceipt, ChannelError> {
        // Suppression is checked HERE, in the send path, so no caller can route
        // around it by forgetting to ask. The decision about when to suppress
        // belongs to the dispatcher; the obligation to honour it belongs to
        // whatever actually reaches the wire, which is this.
        let receipt =
            if message.class == OutboundClass::Automated && self.suppression.is_suppressed(to) {
                MessageReceipt::Suppressed
            } else {
                let armed = {
                    let mut failures = self
                        .armed_failures
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    match failures.get_mut(to.revealed()) {
                        Some(remaining) if *remaining > 0 => {
                            *remaining -= 1;
                            true
                        }
                        _ => false,
                    }
                };
                if armed {
                    MessageReceipt::Failed {
                        reason: "the provider rejected the message".to_owned(),
                    }
                } else {
                    let (text, truncated) = body::fit(&message.text, self.config.segment_ceiling);
                    let segments = body::segment(&text).segments;
                    self.outbox
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(Sent {
                            to: to.clone(),
                            text,
                            class: message.class,
                            receipt: MessageReceipt::Delivered {
                                segments,
                                truncated,
                            },
                        });
                    return Ok(MessageReceipt::Delivered {
                        segments,
                        truncated,
                    });
                }
            };

        // Suppressed and failed sends are recorded too. A message that was
        // deliberately withheld is a fact about the conversation, and one that
        // vanished from the record is indistinguishable from one never sent.
        self.outbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Sent {
                to: to.clone(),
                text: message.text,
                class: message.class,
                receipt: receipt.clone(),
            });
        Ok(receipt)
    }
}
