//! Text-only WebVTT packets, whose timing is supplied by their container.
//! Reuse OxideAV's WebVTT inline-markup parser through its cue decoder; its
//! standalone packet format includes a timing line that Matroska omits.

use std::collections::VecDeque;
use oxideav_core::{CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result, TimeBase};

pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    let inner = oxideav_subtitle::codec::make_decoder(params)?;
    if params.options.get("webvtt_packet_format") != Some("text") {
        return Ok(inner);
    }
    Ok(Box::new(WebVttDecoder {
        inner,
        scratch: Packet::new(0, TimeBase::new(1, 1_000_000), Vec::new()),
        pending: VecDeque::new(),
        eof: false,
    }))
}

struct WebVttDecoder {
    inner: Box<dyn Decoder>,
    scratch: Packet,
    pending: VecDeque<Frame>,
    eof: bool,
}

impl Decoder for WebVttDecoder {
    fn codec_id(&self) -> &CodecId { self.inner.codec_id() }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.len() > 1024 * 1024 {
            return Err(Error::invalid("WebVTT cue exceeds 1 MiB"));
        }
        if packet.data.is_empty() { return Ok(()); }
        self.scratch.data.clear();
        self.scratch.data.extend_from_slice(b"00:00:00.000 --> 00:00:00.000\n");
        self.scratch.data.extend_from_slice(&packet.data);
        self.inner.send_packet(&self.scratch)?;
        let Frame::Subtitle(mut cue) = self.inner.receive_frame()? else {
            return Err(Error::invalid("WebVTT decoder returned a non-subtitle frame"));
        };
        let us = TimeBase::new(1, 1_000_000);
        cue.start_us = packet.time_base.rescale(packet.pts.unwrap_or(0), us);
        cue.end_us = cue.start_us.saturating_add(packet.time_base.rescale(packet.duration.unwrap_or(0), us));
        self.pending.push_back(Frame::Subtitle(cue));
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        self.pending.pop_front().ok_or(if self.eof { Error::Eof } else { Error::NeedMore })
    }

    fn flush(&mut self) -> Result<()> { self.eof = true; Ok(()) }

    fn reset(&mut self) -> Result<()> {
        self.pending.clear();
        self.eof = false;
        self.inner.reset()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn text_packet_uses_container_timing_and_inline_markup() {
        let mut params = CodecParameters::subtitle(CodecId::new("webvtt"));
        params.options = params.options.set("webvtt_packet_format", "text");
        let mut decoder = make_decoder(&params).unwrap();
        decoder.send_packet(&Packet::new(0, TimeBase::new(1, 1000), b"<b>Hello</b>\nworld".to_vec())
            .with_pts(1500).with_duration(750)).unwrap();
        let Frame::Subtitle(cue) = decoder.receive_frame().unwrap() else { panic!() };
        assert_eq!((cue.start_us, cue.end_us), (1_500_000, 2_250_000));
        assert!(matches!(&cue.segments[0], oxideav_core::Segment::Bold(_)));
        decoder.flush().unwrap();
        assert!(matches!(decoder.receive_frame(), Err(Error::Eof)));
        decoder.reset().unwrap();
        assert!(matches!(decoder.receive_frame(), Err(Error::NeedMore)));
    }
}
