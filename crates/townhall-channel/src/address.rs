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
        let unroutable = || ChannelError::UnroutableAddress(Self::mask_raw(raw));

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

        // E.164 country codes never begin with zero, so `+0…` is not a number
        // anywhere — refusing it here catches the general case the trunk-zero
        // check below only catches for the configured region.
        if international && digits.starts_with('0') {
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

    /// A Telegram chat, addressed by its numeric chat id (M12 / ADR-033).
    ///
    /// A SECOND kind of address the human edge can carry, named for what it is
    /// (`tg:`) rather than disguised as a phone number. It bypasses [`Self::parse`]'s
    /// phone grammar entirely and on purpose: a chat id is not an E.164 number and
    /// must never be read as one — a negative group id would even collide under
    /// `parse`'s separator stripping (`-` is dropped as presentation). Keeping the
    /// two kinds textually distinct (`+…` vs `tg:…`) is what stops a chat id and a
    /// phone number from ever comparing equal in the suppression list.
    #[must_use]
    pub fn telegram(chat_id: i64) -> Self {
        Self(format!("tg:{chat_id}"))
    }

    /// The Telegram chat id, iff this is a Telegram address (`tg:<id>`); `None`
    /// for a phone address. How a Telegram channel recovers the id to reply to.
    #[must_use]
    pub fn telegram_chat_id(&self) -> Option<i64> {
        self.0.strip_prefix("tg:").and_then(|id| id.parse().ok())
    }

    /// A raw, unparseable input, masked for an error message: the first three
    /// characters and the length. Enough to see "started with +44, 30 chars
    /// long" — not enough to identify a subscriber, which an unroutable string
    /// may still very nearly do.
    #[must_use]
    pub fn mask_raw(raw: &str) -> String {
        let count = raw.chars().count();
        if count <= 3 {
            return format!("<{count} chars>");
        }
        let prefix: String = raw.chars().take(3).collect();
        format!("{prefix}\u{2026} ({count} chars)")
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

#[cfg(test)]
mod telegram_tests {
    use super::*;

    #[test]
    fn a_telegram_address_round_trips_its_chat_id() {
        let addr = ChannelAddress::telegram(5_741_534_028);
        assert_eq!(addr.revealed(), "tg:5741534028");
        assert_eq!(addr.telegram_chat_id(), Some(5_741_534_028));
    }

    #[test]
    fn a_negative_group_id_survives_where_a_phone_parse_would_corrupt_it() {
        // Group chat ids are negative; `parse` strips `-` as a separator and would
        // misread them. The `tg:` constructor keeps them exact.
        let addr = ChannelAddress::telegram(-1_001_234_567_890);
        assert_eq!(addr.telegram_chat_id(), Some(-1_001_234_567_890));
    }

    #[test]
    fn a_phone_and_a_telegram_address_never_collide() {
        let phone = ChannelAddress::parse("+447700900123", Region::Gb).expect("valid phone");
        assert_eq!(
            phone.telegram_chat_id(),
            None,
            "a phone is not a telegram chat"
        );
        // The two encodings are textually distinct, so equality can never confuse
        // a chat id with a phone number in the suppression list.
        let tg = ChannelAddress::telegram(447_700_900_123);
        assert_ne!(phone, tg);
        assert_eq!(phone.revealed(), "+447700900123");
        assert_eq!(tg.revealed(), "tg:447700900123");
    }
}
