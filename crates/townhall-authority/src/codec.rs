//! Length-prefixed field encoding, used by the canonical scope's digest and by
//! the delegation envelope.
//!
//! One helper for both, because they must agree about what "a field" is. ADR-023
//! recorded the reason the encoding is length-prefixed rather than
//! delimiter-joined when the inbound identity faced the same choice: joining on
//! a separator is not injective the moment any field can contain it.

/// Append one length-prefixed field.
pub(crate) fn push_field(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    out.extend_from_slice(bytes);
}

/// Reads back what [`push_field`] wrote, refusing anything malformed.
///
/// Every accessor returns `Option`, and a truncated or over-long buffer yields
/// `None` rather than a partial value. A decoder that guessed at a damaged
/// envelope would be a minting path with extra steps.
pub(crate) struct Reader<'bytes> {
    bytes: &'bytes [u8],
    at: usize,
}

impl<'bytes> Reader<'bytes> {
    pub(crate) fn new(bytes: &'bytes [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    pub(crate) fn field(&mut self) -> Option<&'bytes [u8]> {
        let header = self.bytes.get(self.at..self.at + 8)?;
        let length = usize::try_from(u64::from_be_bytes(header.try_into().ok()?)).ok()?;
        let start = self.at + 8;
        let field = self.bytes.get(start..start.checked_add(length)?)?;
        self.at = start + length;
        Some(field)
    }

    pub(crate) fn text(&mut self) -> Option<String> {
        std::str::from_utf8(self.field()?).ok().map(str::to_owned)
    }

    pub(crate) fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.field()?.try_into().ok()?))
    }

    pub(crate) fn bytes32(&mut self) -> Option<[u8; 32]> {
        self.field()?.try_into().ok()
    }

    /// Whether every byte has been consumed.
    ///
    /// Checked at the end of a decode: trailing bytes mean the envelope is not
    /// the one this version wrote, and accepting the prefix would be accepting
    /// a grant somebody else's encoder produced.
    pub(crate) fn is_finished(&self) -> bool {
        self.at == self.bytes.len()
    }
}
