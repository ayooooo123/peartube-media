//! A reader with FFmpeg's out-of-range behaviour, so the ports see the
//! same bytes FFmpeg's decoders do on malformed input.
//!
//! FFmpeg hands a decoder its packet followed by `AV_INPUT_BUFFER_PADDING_SIZE`
//! zero bytes, and its unchecked readers (`bytestream_get_*`) run past a
//! segment's end into whatever follows it in the packet, then into that
//! padding. This reader indexes the whole packet and reads zeros past its
//! end, which is that behaviour without the undefined part (FFmpeg reading
//! past the padding).
//!
//! Semantics ported from FFmpeg `libavcodec/bytestream.h` (commit 2da55bf,
//! LGPL-2.1-or-later).

/// A byte cursor over a whole packet: `bytestream_get_*` without bounds.
#[derive(Clone, Copy)]
pub(crate) struct Bytes<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bytes<'a> {
    pub(crate) fn new(data: &'a [u8], pos: usize) -> Self {
        Self { data, pos }
    }

    /// Current offset in the packet (may lie past its end).
    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    pub(crate) fn skip(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n);
    }

    /// The byte at `offset` from the cursor without moving it.
    pub(crate) fn peek(&self, offset: usize) -> u8 {
        self.pos.checked_add(offset).and_then(|i| self.data.get(i)).copied().unwrap_or(0)
    }

    pub(crate) fn u8(&mut self) -> u8 {
        let v = self.peek(0);
        self.skip(1);
        v
    }

    pub(crate) fn be16(&mut self) -> u16 {
        u16::from(self.u8()) << 8 | u16::from(self.u8())
    }

    pub(crate) fn be24(&mut self) -> u32 {
        u32::from(self.be16()) << 8 | u32::from(self.u8())
    }
}
