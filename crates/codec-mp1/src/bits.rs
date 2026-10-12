// FFmpeg's checked big-endian bit reader, as the MPEG audio decoder reads a
// frame with it.
//
// Ported from FFmpeg (commit 2da55bf) libavcodec/get_bits.h (the
// CONFIG_SAFE_BITSTREAM_READER path: the index stops 8 bits past the
// frame, and a read near the end takes the bytes that follow the frame in
// its packet, zeros past the packet as FFmpeg's padding gives).
// Copyright (c) 2004 Michael Niedermayer <michaelni@gmx.at>;
// LGPL-2.1-or-later (see LICENSE).

pub(crate) struct Bits<'a> {
    data: &'a [u8],
    index: usize,
    size_plus8: usize,
}

impl<'a> Bits<'a> {
    /// A reader over the first `size` bytes of `data`; `data` goes on to
    /// the end of the packet.
    pub(crate) fn new(data: &'a [u8], size: usize) -> Self {
        Self { data, index: 0, size_plus8: size * 8 + 8 }
    }

    /// `get_bits`, `n` from 1 to 25.
    #[inline]
    pub(crate) fn get(&mut self, n: u32) -> u32 {
        let p = self.index >> 3;
        let b = |i: usize| u32::from(self.data.get(p + i).copied().unwrap_or(0));
        let cache = (b(0) << 24 | b(1) << 16 | b(2) << 8 | b(3)) << (self.index & 7);
        self.index = self.size_plus8.min(self.index + n as usize);
        cache >> (32 - n)
    }
}
