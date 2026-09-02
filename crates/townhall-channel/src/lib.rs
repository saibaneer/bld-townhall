#![forbid(unsafe_code)]

//! The human edge, normalized — and deciding nothing.
//!
//! Spec §14: a `HumanChannel` "normalizes human communication into typed
//! inbound/outbound events. It must not own booking state, policy, authority or
//! model decisions."
//!
//! That is enforced here by what this crate can *name*. Its manifest excludes
//! `townhall-gateway`, `townhall-service`, `townhall-store`, `townhall-http`,
//! `bld-kernel`, `sqlx` and `reqwest` — in dev-dependencies as well as normal
//! ones, because this crate's tests need no server and the exemption the gateway
//! needs would otherwise let a `#[cfg(test)]` module reach the store. A crate
//! with no route to a mutation surface cannot mutate, so §14's prohibition is a
//! fact about the crate graph rather than a rule someone has to remember.
//!
//! What lives here: address normalization, inbound bounds, segment counting,
//! one definition of message identity with its replay window, classification
//! into three arms, the suppression *mechanism*, and an in-process simulator.
//!
//! What does not: any answer. `HELP` is answered by the dispatcher from a port,
//! `CANCEL` needs a lookup this crate cannot perform, and `BOOK date=…` is
//! `Freeform` — the channel does not know what an attendee count is.

use async_trait::async_trait;
use thiserror::Error;

pub mod address;
pub mod body;
pub mod grammar;
pub mod identity;
pub mod simulator;

pub use address::{ChannelAddress, Region};
pub use body::{Alphabet, GSM_BASIC, GSM_EXTENSION, InboundBody, Segmentation};
pub use grammar::{Command, ControlCommand, ResourceCommand, classify};
pub use identity::{InboundIdentity, ReplayWindow, Seen};
pub use simulator::SmsSimulator;

/// Which transport a message travelled on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelKind {
    /// The in-process simulator. M12 adds a real provider beside it.
    SmsSimulator,
}

/// What the provider asserted about a message.
///
/// # Named so it cannot be read as identity
///
/// Spec §3.2 grades provider metadata as "transport evidence; useful but not
/// high-assurance identity by itself". The accessor is [`Self::claimed_sender`]
/// — not `sender` — because the difference between what a provider claims and
/// who someone is is the difference this whole layer exists to preserve.
#[derive(Clone)]
pub struct TransportEvidence {
    provider: String,
    claimed_sender: String,
    verified: bool,
    signature: Option<String>,
}

impl TransportEvidence {
    #[must_use]
    pub fn new(
        provider: impl Into<String>,
        claimed_sender: impl Into<String>,
        verified: bool,
    ) -> Self {
        Self {
            provider: provider.into(),
            claimed_sender: claimed_sender.into(),
            verified,
            signature: None,
        }
    }

    #[must_use]
    pub fn with_signature(mut self, signature: impl Into<String>) -> Self {
        self.signature = Some(signature.into());
        self
    }

    /// Who the provider *says* sent this. Not authority. Not identity.
    #[must_use]
    pub fn claimed_sender(&self) -> &str {
        &self.claimed_sender
    }

    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// Whether the provider's own verification passed, where it offers one.
    #[must_use]
    pub fn verified(&self) -> bool {
        self.verified
    }
}

/// `TransportEvidence { provider: "sim", verified: true }` — never the raw
/// signature, never the claimed sender's full number.
///
/// §15.1 forbids "secrets, raw signatures or unnecessary PII in logs", and a
/// derived `Debug` would print all three.
impl std::fmt::Debug for TransportEvidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "TransportEvidence {{ provider: {:?}, verified: {} }}",
            self.provider, self.verified
        )
    }
}

/// What a provider handed us, before any of it is trusted.
#[derive(Clone, Debug)]
pub struct RawInbound {
    pub identity: InboundIdentity,
    pub channel: ChannelKind,
    /// Unnormalized, as it arrived.
    pub from: String,
    /// Unbounded, as it arrived.
    pub body: String,
    pub received_at_ms: i64,
    pub evidence: TransportEvidence,
}

/// One inbound message, normalized (spec §14's shape, verbatim).
#[derive(Clone, Debug)]
pub struct InboundMessage {
    pub identity: InboundIdentity,
    pub channel: ChannelKind,
    pub address: ChannelAddress,
    pub received_at_ms: i64,
    pub body: InboundBody,
    pub transport_evidence: TransportEvidence,
}

/// Why an outbound message exists — which decides whether STOP silences it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutboundClass {
    /// Produced in the same turn as an inbound message from that address.
    ///
    /// STOP never suppresses these. If it did, someone who texted STOP could
    /// never discover START, and §14.1's guarantee that HELP is always available
    /// would be false.
    Reply,
    /// Produced by any other trigger — a convergence result arriving later, a
    /// timer, a reconciliation outcome. This is what STOP silences.
    Automated,
}

#[derive(Clone, Debug)]
pub struct OutboundMessage {
    pub text: String,
    pub class: OutboundClass,
}

impl OutboundMessage {
    #[must_use]
    pub fn reply(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            class: OutboundClass::Reply,
        }
    }

    #[must_use]
    pub fn automated(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            class: OutboundClass::Automated,
        }
    }
}

/// What became of one send — explicitly, including the failures.
///
/// §15.1: "outbound delivery failures represented explicitly; they do not roll
/// back already committed business state". So a failure is a value the caller
/// must look at, not an error it can conflate with "the booking didn't happen".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MessageReceipt {
    Delivered {
        segments: u16,
        truncated: bool,
    },
    /// The address asked us to stop, and this was `Automated`.
    ///
    /// A distinct outcome rather than a silent success: a caller that cannot
    /// tell "sent" from "deliberately withheld" cannot report honestly either.
    Suppressed,
    Failed {
        reason: String,
    },
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ChannelError {
    #[error("address {0:?} is not routable")]
    UnroutableAddress(String),
    #[error("message body is {scalars} characters; the limit is {limit}")]
    TooLong { scalars: usize, limit: usize },
    #[error("this message was already handled")]
    Duplicate,
}

/// Whether outbound automation is silenced for an address.
///
/// # Why the trait is here and the policy is not
///
/// The channel owns the *mechanism*: it consults this before every `Automated`
/// send, so no caller can route around suppression by forgetting to ask. The
/// dispatcher owns the *decision*: STOP sets, START clears. Splitting them this
/// way is what lets §14 hold — the channel enforces a rule it does not make.
///
/// The trait lives in this crate because the channel must be able to call it,
/// and this crate cannot name the orchestrator.
pub trait SuppressionStore: Send + Sync + std::fmt::Debug {
    fn is_suppressed(&self, address: &ChannelAddress) -> bool;
    fn suppress(&self, address: &ChannelAddress);
    fn allow(&self, address: &ChannelAddress);
}

/// Spec §14's trait, verbatim in shape.
#[async_trait]
pub trait HumanChannel: Send + Sync {
    type Address: Send + Sync;

    /// Normalize, bound, and dedupe one raw inbound message.
    ///
    /// # Errors
    /// [`ChannelError::UnroutableAddress`], [`ChannelError::TooLong`],
    /// [`ChannelError::Duplicate`].
    async fn receive(&self, raw: RawInbound) -> Result<InboundMessage, ChannelError>;

    /// Send, honouring suppression and the segment ceiling.
    ///
    /// # Errors
    /// Transport-level failure. A *delivery* failure is a
    /// [`MessageReceipt::Failed`], not an `Err` — it is an outcome, not a
    /// broken call.
    async fn send(
        &self,
        to: &Self::Address,
        message: OutboundMessage,
    ) -> Result<MessageReceipt, ChannelError>;
}

/// Everything the channel needs told to it rather than assumed.
#[derive(Clone, Copy, Debug)]
pub struct ChannelConfig {
    /// The national numbering context bare local numbers are read against.
    pub default_region: Region,
    /// Outbound segment ceiling. Longer text is truncated with a marker.
    pub segment_ceiling: u16,
    /// How long a provider message id is remembered.
    pub replay_window_ms: i64,
}

impl Default for ChannelConfig {
    fn default() -> Self {
        Self {
            default_region: Region::Gb,
            segment_ceiling: 3,
            replay_window_ms: 24 * 60 * 60 * 1000,
        }
    }
}
