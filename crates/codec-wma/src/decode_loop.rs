// Ported from FFmpeg (commit 2da55bf): libavcodec/decode.c
// (decode_simple_internal: the consumed-bytes loop over one packet and
// draining for AV_CODEC_CAP_DELAY decoders).
// GNU Lesser General Public License 2.1 or later.

//! FFmpeg's decode loop for decoders with a `FF_CODEC_DECODE_CB` callback,
//! run on demand: `send` stores the packet and `receive` makes one decode
//! call at a time, so a packet that decodes to many frames (a bit
//! reservoir full of tiny frames) never sits in memory all at once.

use std::collections::VecDeque;

use oxideav_core::{AudioFrame, Error, Frame, Result};

/// What one decode callback did.
pub(crate) struct Call {
    /// The bytes it consumed, or `None` for an error return, which drops
    /// its frame and the rest of the packet.
    pub consumed: Option<usize>,
    pub frame: Option<AudioFrame>,
}

/// A decoder's `FF_CODEC_DECODE_CB` callback.
pub(crate) trait DecodeCallback {
    /// Decode from the unread rest `data` of the current packet; empty
    /// `data` drains (decoders with `AV_CODEC_CAP_DELAY`).
    fn decode(&mut self, data: &[u8]) -> Call;
}

/// FFmpeg's limit on errors while draining (`nb_errors_max` without frame
/// threads).
const MAX_DRAIN_ERRORS: u32 = 21;

#[derive(Default)]
pub(crate) struct DecodeLoop {
    /// `AV_CODEC_CAP_DELAY`: drain with empty packets at end of stream.
    delay: bool,
    input: Vec<u8>,
    pos: usize,
    stalls: u32,
    pending: VecDeque<AudioFrame>,
    draining: bool,
    drain_errors: u32,
    eof: bool,
}

impl DecodeLoop {
    pub fn new(delay: bool) -> Self {
        Self { delay, ..Self::default() }
    }

    /// One decode call; false when there is nothing left to call for.
    fn step<D: DecodeCallback>(&mut self, dec: &mut D) -> bool {
        if self.pos < self.input.len() {
            let rest = &self.input[self.pos..];
            let len = rest.len();
            let call = dec.decode(rest);
            let Some(consumed) = call.consumed else {
                self.pos = self.input.len();
                return true;
            };
            // FFmpeg calls again on the same bytes while frames come out
            // (each one advances the decoder's own reader); a call that
            // neither consumes nor decodes would loop forever.
            self.stalls = match call.frame {
                Some(frame) => {
                    self.pending.push_back(frame);
                    0
                }
                None if consumed == 0 => self.stalls + 1,
                None => 0,
            };
            self.pos = if consumed >= len || self.stalls > 2 { self.input.len() } else { self.pos + consumed };
            return true;
        }
        if self.draining && !self.eof {
            let call = dec.decode(&[]);
            match (call.consumed, call.frame) {
                (Some(_), Some(frame)) => self.pending.push_back(frame),
                (Some(_), None) => self.eof = true,
                (None, _) => {
                    self.drain_errors += 1;
                    if self.drain_errors > MAX_DRAIN_ERRORS {
                        self.eof = true;
                    }
                }
            }
            return true;
        }
        false
    }

    /// `send_packet`: what a caller left of the previous packet is decoded
    /// first.
    pub fn send<D: DecodeCallback>(&mut self, dec: &mut D, data: &[u8]) {
        while self.pos < self.input.len() {
            self.step(dec);
        }
        self.input.clear();
        self.input.extend_from_slice(data);
        self.pos = 0;
        self.stalls = 0;
    }

    /// `receive_frame`.
    pub fn receive<D: DecodeCallback>(&mut self, dec: &mut D) -> Result<Frame> {
        loop {
            if let Some(frame) = self.pending.pop_front() {
                return Ok(Frame::Audio(frame));
            }
            if !self.step(dec) {
                return Err(if self.eof { Error::Eof } else { Error::NeedMore });
            }
        }
    }

    /// End of stream: after the current packet, drain the decoder.
    pub fn flush(&mut self) {
        if self.delay {
            self.draining = true;
        } else {
            self.eof = true;
        }
    }

    /// Seek: forget the packet, the decoded frames and the end of stream.
    pub fn reset(&mut self) {
        self.input.clear();
        self.pos = 0;
        self.stalls = 0;
        self.pending.clear();
        self.draining = false;
        self.drain_errors = 0;
        self.eof = false;
    }
}
