//! The decode route's audio decoding, for the native route's bridge: the
//! same decoder, decoder delay, container trims and timestamps as
//! `run_audio_thread`, without its sink and clock.

use super::*;
use crate::native::bridge::{AudioDecode, DecoderFactory, Pcm};

struct EngineDecode {
    decoder: Box<dyn Decoder>,
    stream: StreamInfo,
    trimmer: Trimmer<Chunk>,
    kept: Vec<Chunk>,
    /// Where the decoder's output so far ends (a frame without a pts of
    /// its own starts there).
    decoded_end: Option<f64>,
}

/// Makes the engine's decoder for `stream`, as many times as asked (each
/// production restart needs a fresh one).
pub(super) fn factory(ctx: Arc<RuntimeContext>, stream: StreamInfo) -> DecoderFactory {
    Arc::new(move || {
        let mut params = stream.params.clone();
        let delay = audio_trim::take_decoder_delay(&mut params);
        let decoder = make_decoder(&ctx, &params).map_err(|e| format!("no audio decoder: {e}"))?;
        Ok(Box::new(EngineDecode {
            decoder,
            stream: stream.clone(),
            trimmer: Trimmer::with_decoder_delay(delay),
            kept: Vec::new(),
            decoded_end: None,
        }) as Box<dyn AudioDecode>)
    })
}

impl EngineDecode {
    fn take(&mut self, out: &mut Vec<Pcm>) {
        out.extend(self.kept.drain(..).map(|chunk| Pcm {
            samples: chunk.pcm,
            channels: chunk.channels,
            rate: chunk.rate,
            pts: chunk.pts,
            begins: chunk.begins,
        }));
    }
}

impl AudioDecode for EngineDecode {
    fn packet(&mut self, packet: &Packet, trim: Option<oxideav_core::AudioTrim>, out: &mut Vec<Pcm>) -> Result<(), String> {
        let decoder = &mut self.decoder;
        match std::panic::catch_unwind(AssertUnwindSafe(|| decoder.send_packet(packet))) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e.to_string()),
            Err(_) => return Err("the audio decoder panicked".into()),
        }
        self.trimmer.packet(packet, trim);
        // The first frame without a pts of its own takes the packet's.
        let mut packet_pts = packet.pts;
        let result = loop {
            let decoder = &mut self.decoder;
            let af = match std::panic::catch_unwind(AssertUnwindSafe(|| decoder.receive_frame())) {
                Ok(Ok(Frame::Audio(af))) => af,
                Ok(Ok(_)) => continue,
                Ok(Err(oxideav_core::Error::NeedMore)) | Ok(Err(oxideav_core::Error::Eof)) => break Ok(()),
                Ok(Err(e)) => break Err(e.to_string()),
                Err(_) => break Err("the audio decoder panicked".into()),
            };
            let chunk = decoded_chunk(self.decoder.as_ref(), &self.stream, &af, &mut packet_pts, &mut self.decoded_end);
            self.trimmer.frame(chunk, af.pts, &mut self.kept);
        };
        // Drained after every packet, as the decode route does: the
        // trimmer holds a bounded queue.
        self.take(out);
        result
    }

    fn finish(&mut self, out: &mut Vec<Pcm>) {
        // The last packet's span ends first; what the trimmer still holds
        // after the tail is the stream's end padding.
        self.trimmer.drain(&mut self.kept);
        let _ = self.decoder.flush();
        let mut packet_pts = None;
        loop {
            let decoder = &mut self.decoder;
            let af = match std::panic::catch_unwind(AssertUnwindSafe(|| decoder.receive_frame())) {
                Ok(Ok(Frame::Audio(af))) if af.samples > 0 => af,
                Ok(Ok(Frame::Audio(_))) => break,
                Ok(Ok(_)) => continue,
                _ => break,
            };
            let chunk = decoded_chunk(self.decoder.as_ref(), &self.stream, &af, &mut packet_pts, &mut self.decoded_end);
            self.trimmer.frame(chunk, af.pts, &mut self.kept);
        }
        self.trimmer.finish(&mut self.kept);
        self.take(out);
    }
}
