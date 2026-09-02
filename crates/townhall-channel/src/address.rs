//! Where a message came from, normalized — and what that is *not*.

use crate::ChannelError;
use std::fmt;

/// The national numbering context a bare local number is read against.
///
/// Explicit configuration, not a default buried in a parser. `07700900123` is
/// only `+447700900123` if you already believe the sender is British, and that
/// belief has to come from somewhere nameable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    /// +44, trunk prefix `0`.
    Gb,
    /// +1, trunk prefix `1`.
    Us,
}

impl Region {
    const fn dial_code(self) -> &'static str {
        match self {
            Self::Gb => "44",
            Self::Us => "1",
        }
    }

    /// The digit a national-format number leads with and an international one
    /// does not.
    const fn trunk_prefix(self) -> char {
        match self {
            Self::Gb => '0',
            Self::Us => '1',
        }
    }
}

/// One person's phone number, in E.164, canonical.
///
/// # This is a binding, not an identity
///
/// Spec Appendix C: the internal `PrincipalId` is the identity key and "phone
/// number is a channel binding, not primary identity key". So this type
/// deliberately offers no route to a principal — resolving one is a
/// `PrincipalDirectory`'s job, one layer up, and a caller who wants to treat a
/// number as a person has to go and ask something that could say no.
///
/// # Why malformed input is rejected rather than carried
///
/// A half-parsed phone number is exactly the value that gets compared for
/// equality later and silently fails to match: the suppression list says
/// `+447700900123`, the inbound says `07700900123`, and STOP stops working for
/// reasons nobody can see. Normalization that can fail, failing loudly, is the
/// only version of this that holds.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ChannelAddress(String);

impl ChannelAddress {
    /// The documented subset: E.164 syntax, plus national-form expansion for the
    /// configured region only.
    ///
    /// **Not** a full phone-number library. M6 talks to a simulator, so the
    /// honest move is to narrow the claim and name it; M12's adapter, which meets
    /// real carrier input, gets the library.
    ///
    /// # Errors
    /// [`ChannelError::UnroutableAddress`] for anything outside that subset.
    pub fn parse(raw: &str, region: Region) -> Result<Self, ChannelError> {
        let unroutable = || ChannelError::UnroutableAddress(raw.to_owned());

        // Separators are presentation, not data: humans and providers both
        // sprinkle them, and no two agree where.
        let stripped: String = raw
            .chars()
            .filter(|c| !matches!(c, ' ' | '-' | '(' | ')' | '.' | '\u{a0}'))
            .collect();
        let (digits, international) = match stripped.strip_prefix('+') {
            Some(rest) => (rest.to_owned(), true),
            None => (stripped, false),
        };
        if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
            return Err(unroutable());
        }

        let e164 = if international {
            // A `+` number that kept its trunk zero is the trap: `+44 07700
            // 900123` looks fine, parses fine under a lenient reader, and names
            // a different subscriber than `+447700900123`. There is no such
            // number, so it is refused rather than repaired — repairing it would
            // mean guessing which of two readings the sender meant.
            let after_code = digits
                .strip_prefix(region.dial_code())
                .filter(|rest| rest.starts_with(region.trunk_prefix()));
            if after_code.is_some() {
                return Err(unroutable());
            }
            digits
        } else {
            // National form: swap the trunk prefix for the region's dial code.
            let subscriber = digits
                .strip_prefix(region.trunk_prefix())
                .ok_or_else(unroutable)?;
            format!("{}{subscriber}", region.dial_code())
        };

        // E.164: at most 15 digits, and short enough to be a shortcode is not
        // short enough to be a subscriber.
        if !(8..=15).contains(&e164.len()) {
            return Err(unroutable());
        }
        Ok(Self(format!("+{e164}")))
    }

    /// The full value, for routing — named to be conspicuous at the call site.
    ///
    /// Anything that merely *identifies* this address in a log or an error should
    /// use the masked [`fmt::Debug`] instead.
    #[must_use]
    pub fn revealed(&self) -> &str {
        &self.0
    }
}

/// Masked, always: `ChannelAddress("+4477…0123")`.
///
/// §15.1 forbids unnecessary PII in logs, and a derived `Debug` would put a full
/// phone number into every error, panic and trace that touched a message. The
/// country code and last four survive because they are what a human debugging a
/// routing problem actually needs.
impl fmt::Debug for ChannelAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let digits = &self.0;
        // Every constructed value is at least +8 digits, so these slices hold;
        // the guard is for a value built by a future constructor that forgot.
        if digits.len() < 9 {
            return write!(f, "ChannelAddress(<short>)");
        }
        write!(
            f,
            "ChannelAddress(\"{}…{}\")",
            &digits[..5],
            &digits[digits.len() - 4..]
        )
    }
}

/// Deliberately the same masking as `Debug`.
///
/// `Display` is what ends up in a formatted string somebody then logs, so the
/// two must not disagree — a type with a safe `Debug` and a leaky `Display` is
/// worse than one with neither, because it looks handled.
impl fmt::Display for ChannelAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}
