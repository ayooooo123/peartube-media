//! Pure-Rust General MIDI playback, ported from FluidSynth 2.6.1
//! (LGPL-2.1-or-later). No instrument bank is built in. The caller must set
//! `CodecParameters.options["soundfont"]` to a readable local SF2 path.
#![forbid(unsafe_code)]

mod channel;
mod conv;
mod effects;
mod generator;
mod modulator;
mod player;
mod rvoice;
mod sfont;
mod smf;
mod synth;
mod voice;

#[cfg(test)]
mod tests;

use oxideav_core::{
    AudioFormat, AudioFrame, CodecCapabilities, CodecId, CodecInfo, CodecParameters, Decoder,
    Error, Frame, Packet, Result, RuntimeContext, SampleFormat, TimeBase,
};
use std::{fs::File, io::BufReader, sync::Arc};

const RATE: u32 = 44_100;
const FRAME_BLOCKS: usize = 16;

/// Construct the SMF decoder. A SoundFont path is mandatory; no fallback
/// oscillator or bundled SoundFont is used when it is absent.
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    let path = params
        .options
        .get("soundfont")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            Error::unsupported("MIDI track needs a SoundFont: select a local SF2 file")
        })?;
    let file = File::open(path)
        .map_err(|e| Error::invalid(format!("cannot open SoundFont {path}: {e}")))?;
    let font = sfont::SoundFont::load(&mut BufReader::new(file))
        .map_err(|e| Error::invalid(e.to_string()))?;
    Ok(Box::new(MidiDecoder {
        id: CodecId::new("midi"),
        font: Arc::new(font),
        playback: None,
        samples: 0,
        origin: 0,
        time_base: TimeBase::new(1, i64::from(RATE)),
        flushed: false,
    }))
}

struct MidiDecoder {
    id: CodecId,
    font: Arc<sfont::SoundFont>,
    playback: Option<(player::Player, synth::Synth)>,
    samples: u64,
    origin: i64,
    time_base: TimeBase,
    flushed: bool,
}
impl Decoder for MidiDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.id
    }
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if self.playback.is_some() {
            return Err(Error::invalid(
                "MIDI expects one song packet; reset before another song",
            ));
        }
        if packet.time_base.num() <= 0 || packet.time_base.den() <= 0 {
            return Err(Error::invalid("MIDI packet has an invalid time base"));
        }
        let song =
            smf::parse(&packet.data).map_err(|e| Error::invalid(format!("MIDI: {}", e.0)))?;
        if smf::duration_us(&song) > 24 * 60 * 60 * 1_000_000 {
            return Err(Error::invalid("MIDI song exceeds 24 hours"));
        }
        self.playback = Some((
            player::Player::new(song),
            synth::Synth::new(self.font.clone(), f64::from(RATE)),
        ));
        self.origin = packet.pts.unwrap_or(0);
        self.time_base = packet.time_base;
        self.samples = 0;
        self.flushed = false;
        Ok(())
    }
    fn receive_frame(&mut self) -> Result<Frame> {
        let Some((player, synth)) = &mut self.playback else {
            return Err(if self.flushed {
                Error::Eof
            } else {
                Error::NeedMore
            });
        };
        if player.done {
            return Err(if self.flushed {
                Error::Eof
            } else {
                Error::NeedMore
            });
        }
        let mut pcm = Vec::with_capacity(FRAME_BLOCKS * rvoice::BUFSIZE * 2 * 4);
        let mut block = [0.0; rvoice::BUFSIZE * 2];
        for _ in 0..FRAME_BLOCKS {
            // Commands from the preceding timer callback reach DSP first.
            synth.begin_block();
            player.callback(synth).map_err(Error::invalid)?;
            synth.render_block(&mut block);
            if synth.failed {
                return Err(Error::invalid(
                    "MIDI exceeds the bounded DSP command capacity",
                ));
            }
            for sample in block {
                pcm.extend_from_slice(&sample.to_le_bytes());
            }
            if player.done {
                break;
            }
        }
        let samples = (pcm.len() / 8) as u32;
        let ticks = i128::from(self.samples) * i128::from(self.time_base.den())
            / (i128::from(RATE) * i128::from(self.time_base.num()));
        let pts = self
            .origin
            .saturating_add(ticks.min(i128::from(i64::MAX)) as i64);
        self.samples += u64::from(samples);
        Ok(Frame::Audio(AudioFrame {
            samples,
            pts: Some(pts),
            data: vec![pcm],
        }))
    }
    fn flush(&mut self) -> Result<()> {
        self.flushed = true;
        Ok(())
    }
    fn reset(&mut self) -> Result<()> {
        self.playback = None;
        self.samples = 0;
        self.flushed = false;
        Ok(())
    }
    fn output_audio_format(&self) -> Option<AudioFormat> {
        Some(AudioFormat {
            sample_format: SampleFormat::F32,
            sample_rate: RATE,
            channels: 2,
        })
    }
}

/// Register only the SoundFont-backed renderer, without any bundled bank.
pub fn register(ctx: &mut RuntimeContext) {
    ctx.codecs.register(
        CodecInfo::new(CodecId::new("midi"))
            .capabilities(
                CodecCapabilities::audio("codec-midi")
                    .with_intra_only(true)
                    .with_max_channels(2)
                    .with_max_sample_rate(RATE),
            )
            .with_resolution_priority(50)
            .decoder(make_decoder),
    );
}
oxideav_core::register!("codec-midi", register);
