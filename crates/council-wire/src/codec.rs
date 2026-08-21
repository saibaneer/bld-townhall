//! The bytes that get signed.
//!
//! Not a `Serialize` impl, and deliberately not derived from one. What a
//! signature attests is a specific byte string, so the byte string is built by
//! code that says exactly what it emits — a serde format that changed its field
//! order or its integer width in a point release would silently change what every
//! signature means.
//!
//! # The three rules that make it injective
//!
//! An encoding is only useful here if two different values can never produce the
//! same bytes. Three things buy that:
//!
//! - **Every field is length-prefixed.** Without it `("ab", "c")` and
//!   `("a", "bc")` encode identically, and a signature over one verifies the
//!   other.
//! - **Presence is its own byte.** The obvious alternative — a reserved length
//!   like `0xFFFFFFFF` meaning "absent" — collides with a present value of
//!   exactly that many bytes. A separate byte cannot collide with any length.
//! - **Every type has exactly one representation.** A `bool` is one byte and only
//!   `0x00` or `0x01`; anything else is a decode failure, not a coerced `true`.
//!   Numbers are all `u64` big-endian. Nothing on this wire is negative, and a
//!   value that does not fit its destination is refused rather than truncated —
//!   see [`Decoder::u16`].
//!
//! # Domain separation
//!
//! Every payload starts with [`DOMAIN`] and a message-type byte, so a signature
//! over one kind of message can never verify as another. The council signs three
//! kinds with one key; without the type byte, an availability answer could be
//! replayed as an effect answer.

use thiserror::Error;

/// Protocol name and version. Bumping this invalidates every existing signature,
/// which is the intended effect of a breaking protocol change.
pub const DOMAIN: &[u8] = b"bld.townhall.council.v1\x00";

/// The most bytes any single textual field may carry.
///
/// Matches `bld_types::MAX_BOUNDED_STRING_BYTES`, and applies to *every* text
/// field including identifiers — which are otherwise unbounded, so a caller could
/// otherwise make the council sign a megabyte.
pub const MAX_FIELD_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    Effect = 1,
    Availability = 2,
    Grant = 3,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CodecError {
    #[error("a text field was {len} bytes, over the {MAX_FIELD_BYTES} limit")]
    FieldTooLong { len: usize },
    #[error("the payload ended mid-field")]
    Truncated,
    #[error("trailing bytes after the payload")]
    Trailing,
    #[error("not this protocol, or not this version")]
    WrongDomain,
    #[error("unknown message type {found}")]
    UnknownMessageType { found: u8 },
    #[error("expected message type {expected:?}, found {found:?}")]
    WrongMessageType {
        expected: MessageType,
        found: MessageType,
    },
    #[error("a bool field held {found:#04x}, which is neither 0 nor 1")]
    NotABool { found: u8 },
    #[error("{value} does not fit in the {width}-bit field it decodes into")]
    OutOfRange { value: u64, width: u32 },
    #[error("a presence byte held {found:#04x}, which is neither 0 nor 1")]
    NotAPresenceByte { found: u8 },
    #[error("the text field is not UTF-8")]
    NotUtf8,
    #[error("{value} is before the Unix epoch, which this wire cannot represent")]
    NegativeTimestamp { value: i64 },
}

/// Builds a canonical payload.
pub struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    #[must_use]
    pub fn new(message: MessageType) -> Self {
        let mut bytes = Vec::with_capacity(128);
        bytes.extend_from_slice(DOMAIN);
        bytes.push(message as u8);
        Self { bytes }
    }

    /// # Errors
    /// [`CodecError::FieldTooLong`] if the text exceeds [`MAX_FIELD_BYTES`].
    pub fn text(&mut self, value: &str) -> Result<&mut Self, CodecError> {
        let raw = value.as_bytes();
        if raw.len() > MAX_FIELD_BYTES {
            return Err(CodecError::FieldTooLong { len: raw.len() });
        }
        self.bytes.push(1);
        // `as` is safe: the length is bounded above by MAX_FIELD_BYTES.
        self.bytes
            .extend_from_slice(&u32::try_from(raw.len()).unwrap_or(u32::MAX).to_be_bytes());
        self.bytes.extend_from_slice(raw);
        Ok(self)
    }

    /// # Errors
    /// [`CodecError::FieldTooLong`] if a present text exceeds [`MAX_FIELD_BYTES`].
    pub fn optional_text(&mut self, value: Option<&str>) -> Result<&mut Self, CodecError> {
        if let Some(text) = value {
            return self.text(text);
        }
        self.bytes.push(0);
        Ok(self)
    }

    pub fn number(&mut self, value: u64) -> &mut Self {
        self.bytes.push(1);
        self.bytes.extend_from_slice(&value.to_be_bytes());
        self
    }

    /// A wall-clock millisecond timestamp.
    ///
    /// Refuses a negative value rather than clamping it. Clamping is what this
    /// method exists to prevent: `-1` and `0` would encode identically, so two
    /// distinct deadlines would produce the same signature and one would decode as
    /// the other. That destroys the injectivity the whole encoding rests on, and it
    /// is not hypothetical — a grant issued at `-1` would open as `0` and be
    /// accepted a millisecond later.
    ///
    /// # Errors
    /// [`CodecError::NegativeTimestamp`] if `value` is before the Unix epoch.
    pub fn timestamp(&mut self, value: i64) -> Result<&mut Self, CodecError> {
        let unsigned = u64::try_from(value).map_err(|_| CodecError::NegativeTimestamp { value })?;
        Ok(self.number(unsigned))
    }

    pub fn boolean(&mut self, value: bool) -> &mut Self {
        self.bytes.push(1);
        self.bytes.push(u8::from(value));
        self
    }

    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

/// Reads a canonical payload back, refusing anything the encoder could not have
/// produced.
pub struct Decoder<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Decoder<'a> {
    /// # Errors
    /// [`CodecError::WrongDomain`], [`CodecError::UnknownMessageType`] or
    /// [`CodecError::WrongMessageType`] if the header is not this protocol,
    /// version and message kind.
    pub fn new(bytes: &'a [u8], expected: MessageType) -> Result<Self, CodecError> {
        if bytes.len() < DOMAIN.len() + 1 || &bytes[..DOMAIN.len()] != DOMAIN {
            return Err(CodecError::WrongDomain);
        }
        let found = match bytes[DOMAIN.len()] {
            1 => MessageType::Effect,
            2 => MessageType::Availability,
            3 => MessageType::Grant,
            other => return Err(CodecError::UnknownMessageType { found: other }),
        };
        if found != expected {
            return Err(CodecError::WrongMessageType { expected, found });
        }
        Ok(Self {
            bytes,
            at: DOMAIN.len() + 1,
        })
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], CodecError> {
        let end = self.at.checked_add(n).ok_or(CodecError::Truncated)?;
        let slice = self.bytes.get(self.at..end).ok_or(CodecError::Truncated)?;
        self.at = end;
        Ok(slice)
    }

    fn present(&mut self) -> Result<bool, CodecError> {
        match self.take(1)?[0] {
            0 => Ok(false),
            1 => Ok(true),
            found => Err(CodecError::NotAPresenceByte { found }),
        }
    }

    /// # Errors
    /// [`CodecError::Truncated`], [`CodecError::FieldTooLong`] or
    /// [`CodecError::NotUtf8`].
    pub fn text(&mut self) -> Result<String, CodecError> {
        self.optional_text()?.ok_or(CodecError::Truncated)
    }

    /// # Errors
    /// As [`Self::text`].
    pub fn optional_text(&mut self) -> Result<Option<String>, CodecError> {
        if !self.present()? {
            return Ok(None);
        }
        let len_bytes: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| CodecError::Truncated)?;
        let len = u32::from_be_bytes(len_bytes) as usize;
        if len > MAX_FIELD_BYTES {
            return Err(CodecError::FieldTooLong { len });
        }
        let raw = self.take(len)?;
        core::str::from_utf8(raw)
            .map(|text| Some(text.to_owned()))
            .map_err(|_| CodecError::NotUtf8)
    }

    /// # Errors
    /// [`CodecError::Truncated`] or [`CodecError::NotAPresenceByte`].
    pub fn number(&mut self) -> Result<u64, CodecError> {
        if !self.present()? {
            return Err(CodecError::Truncated);
        }
        let raw: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| CodecError::Truncated)?;
        Ok(u64::from_be_bytes(raw))
    }

    /// A number that must fit in a `u16`.
    ///
    /// Refuses rather than truncates. A capacity of `70000` arriving as `4464` is
    /// a guard silently checking a different number than the one the provider
    /// sent, and that is the whole class of defect this project keeps finding.
    ///
    /// # Errors
    /// [`CodecError::OutOfRange`] if the value exceeds `u16::MAX`.
    pub fn u16(&mut self) -> Result<u16, CodecError> {
        let value = self.number()?;
        u16::try_from(value).map_err(|_| CodecError::OutOfRange { value, width: 16 })
    }

    /// A number that must fit in an `i64` — wall-clock milliseconds.
    ///
    /// # Errors
    /// [`CodecError::OutOfRange`] if the value exceeds `i64::MAX`.
    pub fn i64(&mut self) -> Result<i64, CodecError> {
        let value = self.number()?;
        i64::try_from(value).map_err(|_| CodecError::OutOfRange { value, width: 64 })
    }

    /// # Errors
    /// [`CodecError::NotABool`] if the byte is neither `0x00` nor `0x01`.
    pub fn boolean(&mut self) -> Result<bool, CodecError> {
        if !self.present()? {
            return Err(CodecError::Truncated);
        }
        match self.take(1)?[0] {
            0 => Ok(false),
            1 => Ok(true),
            found => Err(CodecError::NotABool { found }),
        }
    }

    /// Assert the payload is fully consumed.
    ///
    /// # Errors
    /// [`CodecError::Trailing`] if bytes remain.
    pub fn finish(self) -> Result<(), CodecError> {
        if self.at == self.bytes.len() {
            Ok(())
        } else {
            Err(CodecError::Trailing)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CodecError, Decoder, Encoder, MAX_FIELD_BYTES, MessageType};

    fn two_texts(a: &str, b: &str) -> Vec<u8> {
        let mut encoder = Encoder::new(MessageType::Effect);
        encoder.text(a).expect("a");
        encoder.text(b).expect("b");
        encoder.finish()
    }

    /// The injectivity property, as the case that motivates length prefixes.
    #[test]
    fn adjacent_text_fields_cannot_be_confused() {
        assert_ne!(two_texts("ab", "c"), two_texts("a", "bc"));
    }

    /// The case a reserved-length sentinel would have collided on.
    #[test]
    fn absent_and_empty_are_different_bytes() {
        let mut absent = Encoder::new(MessageType::Effect);
        absent.optional_text(None).expect("absent");
        let mut empty = Encoder::new(MessageType::Effect);
        empty.optional_text(Some("")).expect("empty");
        assert_ne!(absent.finish(), empty.finish());
    }

    #[test]
    fn a_message_type_cannot_be_read_as_another() {
        let bytes = Encoder::new(MessageType::Availability).finish();
        assert_eq!(
            Decoder::new(&bytes, MessageType::Effect).err(),
            Some(CodecError::WrongMessageType {
                expected: MessageType::Effect,
                found: MessageType::Availability,
            })
        );
    }

    #[test]
    fn a_foreign_domain_is_refused() {
        assert_eq!(
            Decoder::new(b"some.other.protocol\x00\x01", MessageType::Effect).err(),
            Some(CodecError::WrongDomain)
        );
    }

    #[test]
    fn the_field_limit_is_exact() {
        let mut ok = Encoder::new(MessageType::Effect);
        assert!(ok.text(&"x".repeat(MAX_FIELD_BYTES)).is_ok());

        let mut over = Encoder::new(MessageType::Effect);
        assert_eq!(
            over.text(&"x".repeat(MAX_FIELD_BYTES + 1)).err(),
            Some(CodecError::FieldTooLong {
                len: MAX_FIELD_BYTES + 1
            })
        );
    }

    /// A truncation here would have a guard check a different number than the
    /// council sent.
    #[test]
    fn a_number_too_large_for_its_field_is_refused_not_truncated() {
        let mut encoder = Encoder::new(MessageType::Effect);
        encoder.number(70_000);
        let bytes = encoder.finish();

        let mut decoder = Decoder::new(&bytes, MessageType::Effect).expect("header");
        assert_eq!(
            decoder.u16().err(),
            Some(CodecError::OutOfRange {
                value: 70_000,
                width: 16
            })
        );
    }

    /// Money is `u64` in the domain, so the wire must carry the whole range.
    #[test]
    fn a_money_value_above_i64_max_round_trips() {
        let huge = u64::MAX;
        let mut encoder = Encoder::new(MessageType::Effect);
        encoder.number(huge);
        let bytes = encoder.finish();

        let mut decoder = Decoder::new(&bytes, MessageType::Effect).expect("header");
        assert_eq!(decoder.number().expect("number"), huge);
    }

    #[test]
    fn a_bool_byte_outside_zero_and_one_is_refused() {
        let mut bytes = Encoder::new(MessageType::Effect);
        bytes.boolean(true);
        let mut bytes = bytes.finish();
        *bytes.last_mut().expect("a byte") = 2;

        let mut decoder = Decoder::new(&bytes, MessageType::Effect).expect("header");
        assert_eq!(
            decoder.boolean().err(),
            Some(CodecError::NotABool { found: 2 })
        );
    }

    /// The injectivity failure a clamp would have introduced, as its own gate.
    ///
    /// `-1` must not encode as `0`. If it did, a deadline of `-1` and a deadline of
    /// `0` would share a signature, and a grant minted at the first would verify as
    /// the second.
    #[test]
    fn a_negative_timestamp_is_refused_not_clamped() {
        let mut encoder = Encoder::new(MessageType::Grant);
        assert_eq!(
            encoder.timestamp(-1).err(),
            Some(CodecError::NegativeTimestamp { value: -1 })
        );

        let mut zero = Encoder::new(MessageType::Grant);
        zero.timestamp(0).expect("zero is representable");
        assert!(!zero.finish().is_empty());
    }

    #[test]
    fn trailing_bytes_are_refused() {
        let mut bytes = Encoder::new(MessageType::Effect);
        bytes.text("only").expect("text");
        let mut bytes = bytes.finish();
        bytes.push(0);

        let mut decoder = Decoder::new(&bytes, MessageType::Effect).expect("header");
        assert_eq!(decoder.text().expect("text"), "only");
        assert_eq!(decoder.finish().err(), Some(CodecError::Trailing));
    }

    #[test]
    fn a_payload_ending_mid_field_is_refused() {
        let mut bytes = Encoder::new(MessageType::Effect);
        bytes.text("truncate me").expect("text");
        let mut bytes = bytes.finish();
        bytes.truncate(bytes.len() - 4);

        let mut decoder = Decoder::new(&bytes, MessageType::Effect).expect("header");
        assert_eq!(decoder.text().err(), Some(CodecError::Truncated));
    }
}
