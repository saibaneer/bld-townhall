//! What a person typed, bounded — and how long it will take to send back.

use crate::ChannelError;
use std::fmt;

/// The inbound ceiling, in Unicode scalars: ten GSM-7 segments.
///
/// Scalars rather than bytes because the cap is about what a person typed. A
/// 600-emoji message is 2400 bytes and 600 characters, and it is not four times
/// too long — it is well inside what somebody could plausibly mean to send.
pub const MAX_INBOUND_SCALARS: usize = 1600;

/// One inbound message body, within bounds.
///
/// # Why this is not `bld_types::BoundedString`
///
/// That type is `truncating`: it silently drops everything past 512 **bytes**
/// and returns success. Reusing it here would have capped every SMS at under a
/// third of the documented limit while the plan claimed 1600 — and silently, so
/// the first evidence would have been a user whose message stopped mid-sentence.
///
/// It is right for what it does (provider detail, where a cap is a courtesy).
/// It is wrong for a human's request, where a truncated message changes what
/// they asked for. So: fallible, and measured in scalars.
#[derive(Clone, PartialEq, Eq)]
pub struct InboundBody(String);

impl InboundBody {
    /// # Errors
    /// [`ChannelError::TooLong`] past [`MAX_INBOUND_SCALARS`] — rejected, never
    /// truncated.
    pub fn parse(raw: &str) -> Result<Self, ChannelError> {
        let scalars = raw.chars().count();
        if scalars > MAX_INBOUND_SCALARS {
            return Err(ChannelError::TooLong {
                scalars,
                limit: MAX_INBOUND_SCALARS,
            });
        }
        Ok(Self(raw.to_owned()))
    }

    /// The text, for parsing — never for logging.
    #[must_use]
    pub fn revealed(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn len_scalars(&self) -> usize {
        self.0.chars().count()
    }
}

/// `InboundBody(len=8 scalars)` — the length, and nothing else.
///
/// # Why not a hash
///
/// Hashing is normally redaction. It is not when the input space is small, and
/// this one is: from M7 a body can be `YES 7312`, so ten thousand candidates.
/// Anyone holding the log hashes all ten thousand and reads the code off. The
/// same is true of a phone number, a booking reference, or any short structured
/// token — which is most of what an SMS body contains here. A digest of a
/// low-entropy value is an encoding of it.
///
/// A keyed digest would restore the property and is not taken: it buys key
/// management purely to correlate log lines, and correlation is already
/// available from `InboundIdentity`, safely, because a provider message id is
/// high-entropy and carries no content.
impl fmt::Debug for InboundBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InboundBody(len={} scalars)", self.len_scalars())
    }
}

/// How a body is encoded on the wire, which decides how much fits in a segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alphabet {
    /// GSM 03.38: seven bits per character, with an escape table costing two.
    Gsm7,
    /// Anything the GSM tables cannot express drags the WHOLE message here.
    Ucs2,
}

/// GSM 03.38 §6.2.1, the basic table — one septet each.
///
/// The complete 128, as data the tests iterate rather than a sample. An
/// "including…" list cannot discriminate an implementation that omits precisely
/// the characters nobody thought to name — LF, CR, `¤` and `¡` are the ones that
/// go missing.
pub const GSM_BASIC: [char; 128] = [
    '@', '£', '$', '¥', 'è', 'é', 'ù', 'ì', 'ò', 'Ç', '\n', 'Ø', 'ø', '\r', 'Å', 'å', 'Δ', '_',
    'Φ', 'Γ', 'Λ', 'Ω', 'Π', 'Ψ', 'Σ', 'Θ', 'Ξ', '\u{1b}', 'Æ', 'æ', 'ß', 'É', ' ', '!', '"', '#',
    '¤', '%', '&', '\'', '(', ')', '*', '+', ',', '-', '.', '/', '0', '1', '2', '3', '4', '5', '6',
    '7', '8', '9', ':', ';', '<', '=', '>', '?', '¡', 'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I',
    'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z', 'Ä', 'Ö',
    'Ñ', 'Ü', '§', '¿', 'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o',
    'p', 'q', 'r', 's', 't', 'u', 'v', 'w', 'x', 'y', 'z', 'ä', 'ö', 'ñ', 'ü', 'à',
];

/// The extension table — **two** septets each, being an escape plus a character.
///
/// Form feed is the one everybody forgets, and `£` is the one everybody wrongly
/// adds: it is *basic*.
pub const GSM_EXTENSION: [char; 10] = ['\u{c}', '^', '{', '}', '\\', '[', '~', ']', '|', '€'];

/// What one message costs to send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segmentation {
    pub alphabet: Alphabet,
    pub segments: u16,
}

/// Count the segments a body occupies.
///
/// The boundaries are not arbitrary: a single GSM-7 message holds 160 septets,
/// but a concatenated one spends six per part on the user-data header, leaving
/// 153. UCS-2 is 70 code units single, 67 concatenated. Getting this wrong
/// overcharges every long message by a segment, or undercharges and truncates.
#[must_use]
pub fn segment(text: &str) -> Segmentation {
    let mut septets = 0_usize;
    let mut gsm = true;
    for character in text.chars() {
        if GSM_BASIC.contains(&character) {
            septets += 1;
        } else if GSM_EXTENSION.contains(&character) {
            septets += 2;
        } else {
            gsm = false;
            break;
        }
    }

    if gsm {
        let segments = if septets <= 160 {
            1
        } else {
            septets.div_ceil(153)
        };
        return Segmentation {
            alphabet: Alphabet::Gsm7,
            segments: u16::try_from(segments.max(1)).unwrap_or(u16::MAX),
        };
    }

    // UCS-2 counts UTF-16 code units, so a character outside the Basic
    // Multilingual Plane — every modern emoji — costs TWO. Counting scalars here
    // would let 35 emoji look like half a segment when they are a whole one.
    let units: usize = text.chars().map(char::len_utf16).sum();
    let segments = if units <= 70 { 1 } else { units.div_ceil(67) };
    Segmentation {
        alphabet: Alphabet::Ucs2,
        segments: u16::try_from(segments.max(1)).unwrap_or(u16::MAX),
    }
}

/// Truncate `text` so it fits `ceiling` segments, marker included.
///
/// The marker is reserved **inside** the ceiling rather than appended after it:
/// appending is the obvious implementation and it pushes a message that exactly
/// filled its budget into one more segment, which is the case the ceiling
/// existed to prevent.
#[must_use]
pub fn fit(text: &str, ceiling: u16) -> (String, bool) {
    const MARKER: char = '…';
    if segment(text).segments <= ceiling {
        return (text.to_owned(), false);
    }
    // Walk back a character at a time. Linear, and the inputs are one SMS.
    let mut kept: Vec<char> = text.chars().collect();
    while !kept.is_empty() {
        kept.pop();
        let mut candidate: String = kept.iter().collect();
        candidate.push(MARKER);
        if segment(&candidate).segments <= ceiling {
            return (candidate, true);
        }
    }
    (MARKER.to_string(), true)
}
