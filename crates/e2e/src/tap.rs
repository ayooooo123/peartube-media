//! A decoder tap: the player's registry with every subtitle decoder wrapped
//! so the runner sees the cues the player's subtitle pipeline received —
//! text and timing, or the bitmap raster — where the headless capture only
//! keeps how many images each show call carried.
//!
//! A wrapped decoder delegates every call to the decoder the plain registry
//! would have built, so the player decodes exactly as it does without the
//! tap. Decoder factories are plain `fn`s, so the recorder a new decoder
//! writes to is whichever [`record_into`] named last: playbacks run one at a
//! time, and a stray decoder from a timed-out earlier playback can only add
//! cues to a later one, which then fails its comparison.

use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;

use oxideav_core::{
    AudioFormat, CodecId, CodecInfo, CodecParameters, Decoder, ExecutionContext, Frame, MediaType, Packet,
    PixelFormat, Result, RuntimeContext, TimeBase,
};
use serde::Serialize;

/// One cue as the subtitle decoder emitted it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum Cue {
    /// A text cue: its timing and its body rendered as SubRip markup (the
    /// rendering FFmpeg's `srt` encoder produces from its decode).
    Text { start_us: i64, end_us: i64, text: String },
    /// A bitmap cue: an RGBA canvas, timed as the player times it (frame
    /// pts, else packet pts; end from the packet duration when there is one).
    Bitmap { start_us: i64, end_us: Option<i64>, width: usize, height: usize, md5: String, blank: bool },
}

/// What one playback's subtitle decoders emitted.
#[derive(Default)]
pub struct Recorder {
    cues: Mutex<Vec<Cue>>,
    decoders: Mutex<Vec<String>>,
}

impl Recorder {
    pub fn cues(&self) -> Vec<Cue> {
        self.cues.lock().clone()
    }

    /// The codec ids of the subtitle decoders built while recording.
    pub fn decoders(&self) -> Vec<String> {
        self.decoders.lock().clone()
    }
}

/// The player's registry as the player gets it, without taps: what a tap
/// delegates to, and what the runner discovers streams with.
pub static PLAIN: LazyLock<RuntimeContext> = LazyLock::new(codecs::context);
static CURRENT: Mutex<Option<Arc<Recorder>>> = Mutex::new(None);

/// The player's registry ([`codecs::context`]) with a tap in front of every
/// decoder. `first_decoder` picks the first implementation registered for an
/// id, so the taps go in before the real registrations.
pub fn context() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    for id in PLAIN.codecs.decoder_ids() {
        let caps = PLAIN.codecs.implementations(id).iter().find(|i| i.make_decoder.is_some()).map(|i| i.caps.clone());
        if let Some(caps) = caps {
            ctx.codecs.register(CodecInfo::new(id.clone()).capabilities(caps).decoder(make_tap));
        }
    }
    codecs::register_all(&mut ctx);
    ctx
}

/// Subtitle decoders built from now on record into `recorder`.
pub fn record_into(recorder: Arc<Recorder>) {
    *CURRENT.lock() = Some(recorder);
}

fn make_tap(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    let inner = PLAIN.codecs.first_decoder(params)?;
    if params.media_type != MediaType::Subtitle {
        return Ok(inner);
    }
    let Some(recorder) = CURRENT.lock().clone() else {
        return Ok(inner);
    };
    recorder.decoders.lock().push(params.codec_id.as_str().to_string());
    Ok(Box::new(Tap { inner, recorder, last_packet: None }))
}

struct Tap {
    inner: Box<dyn Decoder>,
    recorder: Arc<Recorder>,
    /// pts, duration and time base of the last packet sent.
    last_packet: Option<(Option<i64>, Option<i64>, TimeBase)>,
}

impl Tap {
    fn cue(&self, frame: &Frame) -> Option<Cue> {
        match frame {
            Frame::Subtitle(cue) => Some(Cue::Text {
                start_us: cue.start_us,
                end_us: cue.end_us,
                text: oxideav_subtitle::srt::render_segments(&cue.segments),
            }),
            Frame::Video(vf) => {
                let (pts, duration, tb) = self.last_packet.unwrap_or((None, None, TimeBase::new(1, 1000)));
                let us = |ticks: i64| (tb.seconds_of(ticks) * 1e6).round() as i64;
                let plane = vf.planes.first()?;
                let width = plane.stride / 4;
                let height = if plane.stride == 0 { 0 } else { plane.data.len() / plane.stride };
                let mut rgba = Vec::with_capacity(width * height * 4);
                for row in 0..height {
                    rgba.extend_from_slice(&plane.data[row * plane.stride..row * plane.stride + width * 4]);
                }
                Some(Cue::Bitmap {
                    start_us: us(vf.pts.or(pts).unwrap_or(0).max(0)),
                    end_us: pts.zip(duration).map(|(p, d)| us(p.max(0) + d.max(0))),
                    width,
                    height,
                    blank: rgba.chunks_exact(4).all(|px| px[3] == 0),
                    md5: refcheck::md5_hex(&rgba),
                })
            }
            _ => None,
        }
    }
}

impl Decoder for Tap {
    fn codec_id(&self) -> &CodecId {
        self.inner.codec_id()
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        self.last_packet = Some((packet.pts, packet.duration, packet.time_base));
        self.inner.send_packet(packet)
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        let frame = self.inner.receive_frame()?;
        if let Some(cue) = self.cue(&frame) {
            self.recorder.cues.lock().push(cue);
        }
        Ok(frame)
    }

    fn flush(&mut self) -> Result<()> {
        self.inner.flush()
    }

    fn reset(&mut self) -> Result<()> {
        self.inner.reset()
    }

    fn set_execution_context(&mut self, ctx: &ExecutionContext) {
        self.inner.set_execution_context(ctx)
    }

    fn output_pixel_format(&self) -> Option<PixelFormat> {
        self.inner.output_pixel_format()
    }

    fn output_audio_format(&self) -> Option<AudioFormat> {
        self.inner.output_audio_format()
    }
}
