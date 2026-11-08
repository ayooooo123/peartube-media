//! Audio access units from real remuxes must restart independently of video keyframes.
use std::{fs::File, path::PathBuf};
use oxideav_core::{Error, Frame, MediaType};

#[test]
fn remux_audio_midfile_restart() {
    let root = std::env::var_os("PEARTUBE_CORPUS_DIR").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join("projects/peartube-media-corpus"));
    let flac = std::env::temp_dir().join(format!("peartube-audio-resync-{}.mka", std::process::id()));
    let status = std::process::Command::new(refcheck::pinned_ffmpeg())
        .args(["-v", "error", "-nostdin", "-y", "-i"]).arg(root.join("audio.flac"))
        .args(["-map", "0:a:0", "-c:a", "copy"]).arg(&flac).status().unwrap();
    assert!(status.success(), "FLAC Matroska remux failed");
    let ctx = codecs::context();
    for path in [root.join("h264_truehd.mkv"), root.join("h264_eac3.mkv"), root.join("h264_dts.mkv"), flac.clone()] {
        let name = path.file_name().unwrap().to_string_lossy();
        let format = refcheck::probe_container(&ctx, &path).unwrap();
        let mut demux = ctx.containers.open_demuxer(&format, Box::new(File::open(&path).unwrap()), &ctx.codecs).unwrap();
        let stream = demux.streams().iter().find(|s| s.params.media_type == MediaType::Audio).unwrap().clone();
        let mut packets = Vec::new();
        loop {
            match demux.next_packet() {
                Ok(p) if p.stream_index == stream.index => packets.push(p),
                Ok(_) => {},
                Err(Error::Eof) => break,
                Err(e) => panic!("{name}: {e}"),
            }
        }
        assert!(packets.len() > 10, "{name}: too few packets");
        for start in [packets.len()/3+1, packets.len()/2+1, packets.len()*2/3+1] {
            let mut decoder = ctx.codecs.first_decoder(&stream.params).unwrap();
            let mut output_at = None;
            for (offset, packet) in packets[start..].iter().take(256).enumerate() {
                if let Err(e) = decoder.send_packet(packet) {
                    panic!("{name}: AU {start}+{offset}, pts={:?} size={}: {e}", packet.pts, packet.data.len());
                }
                loop {
                    match decoder.receive_frame() {
                        Ok(Frame::Audio(a)) if a.samples > 0 => { output_at = Some(offset); break; },
                        Ok(_) => {},
                        Err(Error::NeedMore | Error::Eof) => break,
                        Err(e) => panic!("{name}: receive AU {start}+{offset}: {e}"),
                    }
                }
                if output_at.is_some() { break; }
            }
            assert!(output_at.is_some(), "{name}: no audio within 256 access units at {start}");
            eprintln!("{name}: restart AU {start}, first audio after {} AUs", output_at.unwrap()+1);
        }
    }
    std::fs::remove_file(flac).unwrap();
}

use std::sync::{Arc, mpsc};
use std::time::Duration;
use player::{AudioOutputMode, Headless, PcmFormat, PcmSlice, Player, PlayerOptions};
use player::backend::{AudioSink, Backend, Clock, SinkError, SubtitleSink, VideoSink};
use oxideav_core::ChannelLayout;

struct OpeningBackend {
    capture: Arc<Headless>,
    opened: mpsc::SyncSender<()>,
    release: Arc<std::sync::Mutex<mpsc::Receiver<()>>>,
}
struct OpeningAudio {
    sink: Box<dyn AudioSink>,
    opened: mpsc::SyncSender<()>,
    release: Arc<std::sync::Mutex<mpsc::Receiver<()>>>,
    first: bool,
}
impl Backend for OpeningBackend {
    fn audio(&self) -> Box<dyn AudioSink> {
        Box::new(OpeningAudio { sink: self.capture.audio(), opened: self.opened.clone(),
            release: self.release.clone(), first: true })
    }
    fn video(&self, clock: Arc<dyn Clock>) -> Box<dyn VideoSink> { self.capture.video(clock) }
    fn subtitles(&self) -> Box<dyn SubtitleSink> { self.capture.subtitles() }
}
impl AudioSink for OpeningAudio {
    fn open(&mut self, rate: u32, layout: ChannelLayout) -> Result<(), SinkError> { self.sink.open(rate, layout) }
    fn open_pcm(&mut self, rate: u32, layout: ChannelLayout, format: PcmFormat, mode: AudioOutputMode) -> Result<(), SinkError> {
        if self.first {
            self.first = false;
            self.opened.send(()).unwrap();
            self.release.lock().unwrap().recv_timeout(Duration::from_secs(10)).unwrap();
        }
        self.sink.open_pcm(rate, layout, format, mode)
    }
    fn write(&mut self, pcm: &[f32], pts: Duration) -> Result<usize, SinkError> { self.sink.write(pcm, pts) }
    fn write_pcm(&mut self, pcm: PcmSlice<'_>, pts: Duration) -> Result<usize, SinkError> { self.sink.write_pcm(pcm, pts) }
    fn play(&mut self) { self.sink.play(); }
    fn pause(&mut self) { self.sink.pause(); }
    fn flush(&mut self) { self.sink.flush(); }
    fn clock(&self) -> Arc<dyn Clock> { self.sink.clock() }
}

#[test]
fn saved_resume_during_pcm_open_drops_the_old_compressed_packet() {
    let root = std::env::var_os("PEARTUBE_CORPUS_DIR").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join("projects/peartube-media-corpus"));
    for name in ["h264_truehd.mkv", "h264_eac3.mkv"] {
        let (opened_tx, opened_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let capture = Headless::new();
        let backend = Arc::new(OpeningBackend { capture: capture.clone(), opened: opened_tx,
            release: Arc::new(std::sync::Mutex::new(release_rx)) });
        let player = Player::open(&root.join(name).to_string_lossy(), backend, Arc::new(codecs::context()),
            PlayerOptions { realtime: false, ..PlayerOptions::default() }, |_| {});
        opened_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        player.seek(Duration::from_millis(2123));
        release_tx.send(()).unwrap();
        let state = player.wait();
        assert!(state.ended && state.error.is_none(), "{name}: {state:?}");
        let captured = capture.capture();
        assert!(!captured.audio[0].pcm.is_empty(), "{name}: no resumed audio");
        assert!(!captured.audio[0].flushes.is_empty(), "{name}: decoder epoch was not restarted after PCM open");
    }
}
