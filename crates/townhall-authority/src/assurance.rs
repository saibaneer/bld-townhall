//! How much the approval evidence is worth.
//!
//! Spec §13.1: "For the POC, SMS approval has a defined assurance level and is
//! suitable only for the town-hall demo risk profile." That sentence is only
//! true if something compares levels — a stored string reading `SmsReply` that
//! nothing reads is decoration, and would satisfy a schema test while enforcing
//! nothing (ADR-025).

/// An assurance level, ordered weakest first.
///
/// `Ord` is derived from declaration order, so `Dev < SmsReply`. Two variants
/// rather than a speculative third: a level nothing can issue is a level no
/// test can reach, and the ordering needs exactly two points to be a real
/// ordering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AssuranceLevel {
    /// The `--dev-authority` lane's level (ADR-025's amendment to ADR-021).
    ///
    /// Pinned lowest deliberately: once the envelope widened, a dev token had
    /// to fabricate an assurance value like every other field, and the
    /// fabricated value a careless implementation reaches for is the strongest
    /// one. A dev grant that reads as maximally assured is a forged envelope
    /// with a straight face.
    Dev,
    /// A one-time code answered from the bound channel within its expiry.
    SmsReply,
}

impl AssuranceLevel {
    /// The durable spelling. Round-tripped by [`Self::parse`]; the pair is
    /// pinned by a test over every variant, because a level that survives a
    /// restart as something weaker (or stronger) than it was issued is worse
    /// than one that fails to load.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Dev => "dev",
            Self::SmsReply => "sms-reply",
        }
    }

    /// Read a level back, or refuse.
    ///
    /// No `Default` and no lenient fallback: an unrecognised level must not
    /// silently become the weakest (which would un-authorize a real grant) or
    /// the strongest (which would promote a corrupted row). The caller decides
    /// what an unreadable row means.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "dev" => Some(Self::Dev),
            "sms-reply" => Some(Self::SmsReply),
            _ => None,
        }
    }

    /// Whether this level clears a required minimum.
    #[must_use]
    pub fn meets(self, minimum: Self) -> bool {
        self >= minimum
    }
}
