//! Closed captions carried in the video (ATSC A/53 in H.264, HEVC and
//! MPEG-1/2) as subtitle tracks.
//!
//! The demux loop hands each packet of the playing video stream to
//! [`Captions::video_packet`]. Its caption data (`subs_cc::CcExtractor`,
//! the triplets FFmpeg attaches to the picture) is put in presentation
//! order (`subs_cc::CaptionTimeline`) and, while a caption track is the
//! selected subtitle stream, goes to the subtitle lane as packets of that
//! synthetic stream, timed like the picture in the video's time base. The
//! subtitle pipeline decodes them as any subtitle stream: `eia_608` (FFmpeg's
//! `cc_dec`) or `cea_708` (VLC's decoder, service 1).
//!
//! Extraction runs as the demuxer reads, up to the video queue's two
//! seconds ahead of the clock, not as the video decodes: a caption screen
//! is final, and so known as a cue, only once the next change arrives, and
//! the earlier it is known the more of them come up on time.
//!
//! The synthetic streams exist from open when the video codec can carry
//! captions (so they are selectable by index); a track is listed in
//! `State::tracks` once its service shows up in the data.

use std::sync::atomic::Ordering;

use oxideav_core::{CodecId, CodecParameters, Packet, PacketMetadata, StreamInfo, TimeBase};
use subs_cc::{CaptionCarrier, CaptionTimeline, CcExtractor};

use super::{notify_changed, QueuedPacket, Run, Track, TrackKind};

/// Stream index of the EIA-608 caption track (field 1 or 2, the first seen,
/// as FFmpeg's decoder picks it). Above any demuxed index (at most 64).
pub(super) const CAPTIONS_608: u32 = 0x1_0000;
/// Stream index of the CEA-708 caption track (service 1).
pub(super) const CAPTIONS_708: u32 = 0x1_0001;

/// The caption streams of `video`, the video stream that will play, are
/// added to `streams` when its codec can carry A/53 captions.
pub(super) fn add_streams(streams: &mut Vec<StreamInfo>, video: Option<u32>) {
    let Some(video) = video.and_then(|v| streams.iter().find(|s| s.index == v)).cloned() else { return };
    if CaptionCarrier::from_codec_id(video.params.codec_id.as_str()).is_none() {
        return;
    }
    for (index, codec) in [(CAPTIONS_608, subs_cc::eia608::CODEC_ID), (CAPTIONS_708, subs_cc::cea708::CODEC_ID)] {
        streams.push(StreamInfo {
            index,
            time_base: video.time_base,
            duration: None,
            start_time: None,
            params: CodecParameters::subtitle(CodecId::new(codec)),
        });
    }
}

/// The demux loop's caption state (see the module docs).
pub(super) struct Captions {
    /// `None` when the playing video carries no captions.
    extractor: Option<CcExtractor>,
    timeline: CaptionTimeline,
    time_base: TimeBase,
    /// The demuxer's applied seek the state belongs to.
    seek: Option<u64>,
}

impl Captions {
    pub(super) fn new(run: &Run<'_>) -> Self {
        let video = run.current_video.and_then(|v| run.streams.iter().find(|s| s.index == v));
        Self {
            extractor: video.and_then(|v| CcExtractor::new(v.params.codec_id.as_str(), &v.params.extradata)),
            timeline: CaptionTimeline::new(),
            time_base: video.map_or(TimeBase::new(1, 1000), |v| v.time_base),
            seek: None,
        }
    }

    /// One packet of the playing video stream, in demux order.
    pub(super) fn video_packet(&mut self, run: &Run<'_>, packet: &Packet) {
        let Some(extractor) = self.extractor.as_mut() else { return };
        // A seek the demuxer applied: the pictures before it never play.
        let seek = (*run.shared.active_seek.lock()).map(|s| s.generation);
        if seek != self.seek {
            self.seek = seek;
            extractor.reset();
            self.timeline.reset();
        }
        let triplets = extractor.extract(&packet.data);
        let released = self.timeline.push(packet.pts, packet.dts, triplets);
        self.deliver(run, released);
    }

    /// The demuxer's end: the pictures still waiting for their place.
    pub(super) fn finish(&mut self, run: &Run<'_>) {
        if self.extractor.is_some() {
            let released = self.timeline.finish();
            self.deliver(run, released);
        }
    }

    fn deliver(&self, run: &Run<'_>, released: Vec<subs_cc::timeline::Timed>) {
        for (pts, triplets) in released {
            list_tracks(run, &triplets);
            let Some(selected) = selected_captions(run) else { continue };
            let mut packet = Packet::new(selected, self.time_base, triplets.into_iter().flatten().collect());
            packet.pts = pts;
            packet.dts = pts;
            run.sub_lane.push(QueuedPacket { packet, metadata: PacketMetadata::default() });
        }
    }
}

/// The caption track playing as the subtitle stream, if one is.
fn selected_captions(run: &Run<'_>) -> Option<u32> {
    run.current_subtitle.filter(|s| *s == CAPTIONS_608 || *s == CAPTIONS_708)
}

/// Lists the caption tracks whose service `triplets` carry, once each:
/// valid non-padding EIA-608 pairs (cc_type 0/1), CEA-708 data (2/3).
fn list_tracks(run: &Run<'_>, triplets: &[[u8; 3]]) {
    let valid = |t: &&[u8; 3]| t[0] & 0x04 != 0;
    let has_608 = triplets.iter().filter(valid).any(|t| t[0] & 0x03 < 2 && (t[1] & 0x7f != 0 || t[2] & 0x7f != 0));
    let has_708 = triplets.iter().filter(valid).any(|t| t[0] & 0x03 >= 2);
    if !has_608 && !has_708 {
        return;
    }
    let shared = run.shared;
    let mut changed = false;
    {
        let mut state = shared.state.lock();
        for (present, index, codec, title) in [
            (has_608, CAPTIONS_608, subs_cc::eia608::CODEC_ID, "Closed captions (EIA-608)"),
            (has_708, CAPTIONS_708, subs_cc::cea708::CODEC_ID, "Closed captions (CEA-708 service 1)"),
        ] {
            if present && !state.tracks.iter().any(|t| t.stream == index) {
                state.tracks.push(Track {
                    stream: index,
                    kind: TrackKind::Subtitle,
                    codec: codec.to_string(),
                    language: None,
                    title: Some(title.to_string()),
                    default: false,
                });
                changed = true;
            }
        }
    }
    if changed && !shared.stopped.load(Ordering::SeqCst) {
        notify_changed(shared);
    }
}
