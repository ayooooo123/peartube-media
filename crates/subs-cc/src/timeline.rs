//! Caption data comes out of video packets in decode order; captions play
//! in presentation order. [`CaptionTimeline`] reorders each packet's
//! triplets by the timestamp FFmpeg gives the picture that carries them
//! (its `best_effort_timestamp`, the time `subcc` packets and `-a53cc`
//! side data carry):
//!
//! - a packet with a pts: that pts;
//! - a packet without one (the I- and P-pictures of raw MPEG video with
//!   B-frames): the dts of the next packet without a pts, the packet whose
//!   decoding outputs that picture. The last such picture of a stream has
//!   no time.
//!
//! Data is released once no later packet can come before it: below the
//! current packet's dts (dts never decreases and a pts is never below its
//! dts), or, for containers without dts, once more than [`REORDER_DEPTH`]
//! pictures wait.

/// The most pictures waiting for their place without a dts to bound them:
/// H.264's largest reordering (16 frames).
pub const REORDER_DEPTH: usize = 16;

/// One picture's caption data and its presentation time, in the video
/// stream's time base (`None`: FFmpeg gives the picture no time).
pub type Timed = (Option<i64>, Vec<[u8; 3]>);

/// See the module docs.
#[derive(Clone, Debug, Default)]
pub struct CaptionTimeline {
    /// Data of the last packet without a pts, waiting for the next one's dts.
    untimed: Option<Vec<[u8; 3]>>,
    /// Timed data not yet released, in presentation order (stable).
    waiting: Vec<(i64, Vec<[u8; 3]>)>,
}

impl CaptionTimeline {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one video packet's data (possibly none: every packet moves the
    /// timeline) and returns what can be released, in presentation order.
    pub fn push(&mut self, pts: Option<i64>, dts: Option<i64>, triplets: Vec<[u8; 3]>) -> Vec<Timed> {
        let mut released = Vec::new();
        match pts {
            Some(pts) => self.insert(pts, triplets),
            None => {
                // This packet outputs the previous untimed picture.
                if let Some(previous) = self.untimed.take() {
                    match dts {
                        Some(dts) => self.insert(dts, previous),
                        None => released.push((None, previous)),
                    }
                }
                self.untimed = Some(triplets);
            }
        }
        loop {
            let ready = match (self.waiting.first(), dts) {
                (Some((ts, _)), Some(dts)) if *ts < dts => true,
                (Some(_), _) => self.waiting.len() > REORDER_DEPTH,
                (None, _) => false,
            };
            if !ready {
                break;
            }
            let (ts, data) = self.waiting.remove(0);
            if !data.is_empty() {
                released.push((Some(ts), data));
            }
        }
        released
    }

    /// The end of the stream: everything still waiting, in order; the last
    /// untimed picture's data without a time.
    pub fn finish(&mut self) -> Vec<Timed> {
        let mut released: Vec<Timed> =
            self.waiting.drain(..).filter(|(_, data)| !data.is_empty()).map(|(ts, data)| (Some(ts), data)).collect();
        if let Some(data) = self.untimed.take().filter(|data| !data.is_empty()) {
            released.push((None, data));
        }
        released
    }

    /// Forgets everything (a seek).
    pub fn reset(&mut self) {
        self.untimed = None;
        self.waiting.clear();
    }

    fn insert(&mut self, ts: i64, triplets: Vec<[u8; 3]>) {
        let at = self.waiting.partition_point(|(t, _)| *t <= ts);
        self.waiting.insert(at, (ts, triplets));
    }
}
