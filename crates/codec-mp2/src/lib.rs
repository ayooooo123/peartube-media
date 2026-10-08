//! Layer II decoding with the same fixed-point synthesis used by Layer I
//! and Musepack. Packet buffering and Q23 requantization live in the MP2 fork;
//! this adapter owns no second DCT, window table or rounding implementation.

#![forbid(unsafe_code)]

use mpegaudiodsp::MpaSynth;
use oxideav_core::{CodecRegistry, RuntimeContext};
use oxideav_mp2::fixed::FixedSynthesis;

#[derive(Default)]
struct Synthesis {
    filter: MpaSynth,
    dither: i32,
}

impl FixedSynthesis for Synthesis {
    fn synthesize(&mut self, subbands: &[[[i32; 32]; 36]], pcm: &mut [[i16; 1152]]) {
        // FFmpeg's mp_decode_frame order. The remainder is shared across
        // channels, not reset per block/frame or held separately per channel.
        for (channel, blocks) in subbands.iter().enumerate() {
            for (block, samples) in blocks.iter().enumerate() {
                self.filter.filter(channel, &mut self.dither,
                    &mut pcm[channel][block * 32..(block + 1) * 32], samples);
            }
        }
    }

    fn reset(&mut self) {
        // mp_flush clears history/remainder but leaves the ring offsets.
        self.filter.synth_buf = [[0; 1024]; 2];
        self.dither = 0;
    }
}

pub fn register_codecs(registry: &mut CodecRegistry) {
    oxideav_mp2::codec_decoder::register_codecs_with_synthesis::<Synthesis>(registry);
}

pub fn register(context: &mut RuntimeContext) {
    register_codecs(&mut context.codecs);
}
