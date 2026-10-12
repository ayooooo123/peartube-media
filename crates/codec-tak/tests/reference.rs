//! codec-tak against FFmpeg 2da55bf (`-cpuflags 0`): the FATE sample of
//! tests/fate/lossless-audio.mak (fate-lossless-tak:
//! lossless-audio/luckynight-partial.tak, the only TAK file FATE has; TAK's
//! encoder runs on Windows only) decodes to FFmpeg's samples byte for byte
//! with its sample count, and the demuxer's packets (the TAK parser's
//! frames) and seeks equal ffprobe's.

use std::path::Path;
use std::process::Command;

use oxideav_core::{Demuxer, Frame, MediaType, RuntimeContext};
use refcheck::{decode, fate, pinned_ffmpeg};

fn run(program: std::path::PathBuf, args: &[&str]) -> Vec<u8> {
    let out = Command::new(&program).args(["-v", "error"]).args(args).output().expect("pinned FFmpeg runs");
    assert!(out.status.success(), "{} {args:?}: {}", program.display(), String::from_utf8_lossy(&out.stderr));
    out.stdout
}

fn ffprobe_csv(args: &[&str]) -> Vec<Vec<String>> {
    String::from_utf8(run(pinned_ffmpeg().with_file_name("ffprobe"), args))
        .expect("utf-8")
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split(',').map(str::to_string).collect())
        .collect()
}

/// Planar frames interleaved, as FFmpeg writes them.
fn interleaved(frames: &[Frame], channels: usize, bytes: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for f in frames {
        let Frame::Audio(a) = f else { continue };
        for i in 0..a.samples as usize {
            for plane in a.data.iter().take(channels) {
                out.extend_from_slice(&plane[i * bytes..(i + 1) * bytes]);
            }
        }
    }
    out
}

fn open(path: &Path) -> Box<dyn Demuxer> {
    let mut ctx = RuntimeContext::new();
    codec_tak::register(&mut ctx);
    assert_eq!(refcheck::probe_container(&ctx, path).as_deref(), Ok("tak"));
    let file = std::fs::File::open(path).expect("open");
    ctx.containers.open_demuxer("tak", Box::new(file), &ctx.codecs).expect("open tak demuxer")
}

#[test]
fn luckynight_matches_ffmpeg() {
    let path = fate("lossless-audio/luckynight-partial.tak");
    let decoded = decode(&path, &[codec_tak::register], MediaType::Audio, 0);
    let format = decoded.audio_format.expect("format");
    assert_eq!(format.sample_format, oxideav_core::SampleFormat::S16P);
    let ours = interleaved(&decoded.frames, usize::from(format.channels), 2);
    let theirs = run(
        pinned_ffmpeg(),
        &["-nostdin", "-cpuflags", "0", "-i", path.to_str().unwrap(), "-map", "0:a:0", "-f", "s16le", "-"],
    );
    let first = ours.iter().zip(&theirs).position(|(a, b)| a != b);
    assert_eq!(first, None, "first differing byte");
    assert_eq!(ours.len(), theirs.len(), "bytes");
    eprintln!("luckynight-partial.tak: {} samples/channel, bit-exact", ours.len() / 4);
}

#[test]
fn packets_equal_ffprobe() {
    let path = fate("lossless-audio/luckynight-partial.tak");
    let expected = ffprobe_csv(&[
        "-select_streams",
        "a:0",
        "-show_entries",
        "packet=pts,duration,size,flags",
        "-of",
        "csv=p=0",
        path.to_str().unwrap(),
    ]);
    let mut demuxer = open(&path);
    let mut ours = Vec::new();
    while let Ok(p) = demuxer.next_packet() {
        let flags = if p.flags.keyframe { "K__" } else { "___" };
        ours.push(vec![
            p.pts.unwrap().to_string(),
            p.duration.unwrap().to_string(),
            p.data.len().to_string(),
            flags.to_string(),
        ]);
    }
    assert_eq!(ours, expected);
}

#[test]
fn seeks_land_where_ffprobe_lands() {
    let path = fate("lossless-audio/luckynight-partial.tak");
    for seconds in ["0", "1.3", "4.2", "100000"] {
        let expected = ffprobe_csv(&[
            "-select_streams",
            "a:0",
            "-read_intervals",
            &format!("{seconds}%+#1"),
            "-show_entries",
            "packet=pts,size",
            "-of",
            "csv=p=0",
            path.to_str().unwrap(),
        ]);
        let mut demuxer = open(&path);
        let target = (seconds.parse::<f64>().unwrap() * 44100.0) as i64;
        let landed = demuxer.seek_to(0, target).expect("seek");
        let p = demuxer.next_packet().expect("packet after seek");
        assert_eq!(
            vec![landed.to_string(), p.data.len().to_string()],
            expected[0],
            "seek to {seconds} s"
        );
    }
}

// ---------------------------------------------------------------------------
// A TAK stream writer: random but valid frames for the paths the FATE file
// does not take (8- and 24-bit, mono, 3 to 6 channels with and without
// multichannel decorrelation, every stereo decorrelation mode, filtered and
// windowed subframes, escapes, frames under 16 samples), decoded by FFmpeg
// and by codec-tak.

/// A little-endian bit writer, as TAK's reader reads.
struct W {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl W {
    fn new() -> Self {
        Self { out: Vec::new(), acc: 0, n: 0 }
    }
    fn put(&mut self, bits: u32, v: u64) {
        for i in 0..bits {
            self.acc |= (v >> i & 1) << self.n;
            self.n += 1;
            if self.n == 8 {
                self.out.push(self.acc as u8);
                self.acc = 0;
                self.n = 0;
            }
        }
    }
    fn sput(&mut self, bits: u32, v: i64) {
        self.put(bits, v as u64 & ((1u64 << bits) - 1));
    }
    fn align(&mut self) {
        while self.n != 0 {
            self.put(1, 0);
        }
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// FFmpeg's AV_CRC_24_IEEE byte loop (its register form).
fn crc24(data: &[u8]) -> u32 {
    let table: Vec<u32> = (0..256u32)
        .map(|i| {
            let mut c = i << 24;
            for _ in 0..8 {
                c = (c << 1) ^ ((0x86_4CFB << 8) & ((c as i32 >> 31) as u32));
            }
            c.swap_bytes()
        })
        .collect();
    data.iter().fold(0xCE_04B7, |crc, &b| table[usize::from(crc as u8 ^ b)] ^ (crc >> 8))
}

const XCODES: [(u32, u32, u32); 50] = [
    (1, 1, 3), (2, 3, 7), (3, 5, 14), (3, 3, 13), (4, 11, 28), (4, 6, 26), (5, 22, 56), (5, 12, 52), (6, 44, 112),
    (6, 24, 104), (7, 88, 224), (7, 48, 208), (8, 176, 448), (8, 96, 416), (9, 352, 896), (9, 192, 832),
    (10, 704, 1792), (10, 384, 1664), (11, 1408, 3584), (11, 768, 3328), (12, 2816, 7168), (12, 1536, 6656),
    (13, 5632, 14336), (13, 3072, 13312), (14, 11264, 28672), (14, 6144, 26624), (15, 22528, 57344),
    (15, 12288, 53248), (16, 45056, 114688), (16, 24576, 106496), (17, 90112, 229376), (17, 49152, 212992),
    (18, 180224, 458752), (18, 98304, 425984), (19, 360448, 917504), (19, 196608, 851968), (20, 720896, 1835008),
    (20, 393216, 1703936), (21, 1441792, 3670016), (21, 786432, 3407872), (22, 2883584, 7340032),
    (22, 1572864, 6815744), (23, 5767168, 14680064), (23, 3145728, 13631488), (24, 11534336, 29360128),
    (24, 6291456, 27262976), (25, 23068672, 58720256), (25, 12582912, 54525952), (26, 46137344, 117440512),
    (26, 25165824, 109051904),
];
const PREDICTOR_SIZES: [usize; 16] = [4, 8, 12, 16, 24, 32, 48, 64, 80, 96, 128, 160, 192, 224, 256, 0];

struct Spec {
    name: &'static str,
    bps: u32,
    channels: usize,
    rate: u32,
    frame_type: u32,
    frames: usize,
    multichannel: bool,
    seed: u64,
}

/// `tak_get_nb_samples` for the frame size types the writer uses.
fn frame_samples(rate: u32, frame_type: u32) -> u32 {
    let quants = [3, 4, 6, 8, 4096, 8192, 16384, 512, 1024, 2048];
    if frame_type <= 3 { rate * quants[frame_type as usize] >> 5 } else { quants[frame_type as usize] }
}

struct Gen<'a> {
    w: W,
    rng: Rng,
    spec: &'a Spec,
    uval: usize,
    subframe_scale: usize,
}

impl Gen<'_> {
    fn esc4(&mut self, v: u32) {
        if v == 0 {
            self.w.put(1, 0);
        } else {
            self.w.put(1, 1);
            self.w.put(4, u64::from(v - 1));
        }
    }

    fn segment(&mut self, mode: u32, len: usize) {
        if mode == 0 {
            return;
        }
        let (init, escape, aescape) = XCODES[mode as usize - 1];
        for _ in 0..len {
            if escape < 1 << init && self.rng.chance(15) {
                let mut x = escape + self.rng.below(u64::from((1u32 << init) - escape)) as u32;
                self.w.put(init, u64::from(x));
                if self.rng.chance(50) {
                    self.w.put(1, 1);
                    x |= 1 << init;
                    if x >= aescape {
                        let scale = self.rng.below(10) as u32;
                        if scale < 9 {
                            self.w.put(scale + 1, 1 << scale);
                        } else {
                            self.w.put(9, 0);
                            let bits = self.rng.below(5) as u32;
                            self.w.put(3, u64::from(bits));
                            if bits > 0 {
                                self.w.put(bits, self.rng.below(1 << bits));
                            }
                        }
                    }
                } else {
                    self.w.put(1, 0);
                }
            } else {
                self.w.put(init, self.rng.below(u64::from(escape)));
            }
        }
    }

    fn residues(&mut self, len: usize) {
        let uval = self.uval;
        let mut wlength = len / uval;
        let mut rval = len - wlength * uval;
        if rval < uval / 2 {
            rval += uval;
        } else {
            wlength += 1;
        }
        let small = |r: &mut Rng| 1 + r.below(10) as u32;
        if len > 0 && wlength > 1 && wlength <= 128 && self.rng.chance(60) {
            self.w.put(1, 1);
            let mut mode = small(&mut self.rng);
            self.w.put(6, u64::from(mode));
            let mut modes = vec![mode];
            for _ in 1..wlength {
                match self.rng.below(5) {
                    0 => self.w.put(1, 1),
                    1 if mode > 1 => {
                        self.w.put(2, 0b10);
                        mode -= 1;
                    }
                    2 if mode < 12 => {
                        self.w.put(3, 0b100);
                        mode += 1;
                    }
                    3 => {
                        let c = 3 + self.rng.below(3) as u32;
                        let step = c - 1;
                        let up = mode + step <= 14;
                        if up || mode > step {
                            self.w.put(c + 1, 1 << c);
                            self.w.put(1, u64::from(!up));
                            mode = if up { mode + step } else { mode - step };
                        } else {
                            self.w.put(1, 1);
                        }
                    }
                    _ => {
                        self.w.put(6, 0);
                        mode = small(&mut self.rng);
                        self.w.put(6, u64::from(mode));
                    }
                }
                modes.push(mode);
            }
            for (i, &m) in modes.iter().enumerate() {
                let l = if i + 1 >= wlength { rval } else { uval };
                self.segment(m, l);
            }
        } else {
            self.w.put(1, 0);
            let mode = if self.rng.chance(10) { 0 } else { small(&mut self.rng) };
            self.w.put(6, u64::from(mode));
            self.segment(mode, len);
        }
    }

    fn subframe(&mut self, len: usize, prev: usize) {
        if self.rng.chance(30) {
            self.w.put(1, 0);
            self.residues(len);
            return;
        }
        self.w.put(1, 1);
        let use_prev = prev > 0 && self.rng.chance(40);
        let limit = if use_prev { prev } else { len };
        let choices: Vec<usize> = (0..16).filter(|&i| PREDICTOR_SIZES[i] <= limit).collect();
        let idx = choices[self.rng.below(choices.len() as u64) as usize];
        let order = PREDICTOR_SIZES[idx];
        self.w.put(4, idx as u64);
        if prev > 0 {
            self.w.put(1, u64::from(use_prev));
        }
        if !use_prev {
            self.w.put(2, self.rng.below(3));
            self.residues(order);
        }
        let dshift = self.rng.below(3) as u32;
        self.esc4(dshift);
        let size_bit = self.rng.below(2) as u32;
        self.w.put(1, u64::from(size_bit));
        let size = 6 + size_bit;
        if self.rng.chance(50) {
            self.w.put(1, 1);
            self.w.put(3, self.rng.below(7));
        } else {
            self.w.put(1, 0);
        }
        self.w.sput(10, self.rng.below(200) as i64 - 100);
        self.w.sput(10, self.rng.below(200) as i64 - 100);
        self.w.sput(size, self.rng.below(16) as i64 - 8);
        self.w.sput(size, self.rng.below(16) as i64 - 8);
        if order > 4 {
            let b = self.rng.below(2) as u32;
            self.w.put(1, u64::from(b));
            let tmp = size - b;
            let mut x = 0;
            for i in 4..order {
                if i & 3 == 0 {
                    let d = self.rng.below(4) as u32;
                    self.w.put(2, u64::from(d));
                    x = tmp - d;
                }
                self.w.sput(x, self.rng.below(4) as i64 - 2);
            }
        }
        self.residues(if use_prev { len } else { len - order });
    }

    fn channel(&mut self, nb: usize) {
        let bps = self.spec.bps;
        let shift = if self.rng.chance(20) { 1 + self.rng.below(3) as u32 } else { 0 };
        self.esc4(shift);
        let width = bps - shift;
        self.w.sput(width, self.rng.below(64) as i64 - 32);
        self.w.put(2, self.rng.below(4));
        let scale = self.subframe_scale;
        let max_parts = ((nb - 2) / scale).min(7);
        let parts = if max_parts > 0 && self.rng.chance(40) { 1 + self.rng.below(max_parts as u64) as usize } else { 0 };
        self.w.put(3, parts as u64);
        let mut lens = Vec::new();
        let mut v = 0usize;
        let mut used = 0usize;
        for k in 0..parts {
            // Positions stay below what the remaining subframes need.
            let room = (nb - 2 - used) / scale - (parts - 1 - k);
            let step = 1 + self.rng.below(room.clamp(1, 63 - v - (parts - 1 - k)).max(1) as u64) as usize;
            v += step;
            self.w.put(6, v as u64);
            lens.push(step * scale);
            used += step * scale;
        }
        lens.push(nb - 1 - used);
        let mut prev = 0;
        for len in lens {
            self.subframe(len, prev);
            prev = len;
        }
    }

    fn dmode_params(&mut self, dmode: u32) {
        match dmode {
            4 | 5 => {
                let dshift = self.rng.below(3) as u32;
                self.esc4(dshift);
                self.w.sput(10, self.rng.below(400) as i64 - 200);
            }
            6 | 7 => {
                let dshift = self.rng.below(3) as u32;
                self.esc4(dshift);
                let order = if self.rng.chance(50) { 16 } else { 8 };
                self.w.put(1, u64::from(order == 16));
                self.w.put(1, self.rng.below(2));
                self.w.put(1, self.rng.below(2));
                let mut code_size = 0;
                for i in 0..order {
                    if i & 3 == 0 {
                        let v = self.rng.below(8) as u32;
                        self.w.put(3, u64::from(v));
                        code_size = 14 - v;
                    }
                    self.w.sput(code_size, self.rng.below(32) as i64 - 16);
                }
            }
            _ => {}
        }
    }

    fn frame_body(&mut self, nb: usize) {
        let channels = self.spec.channels;
        if nb < 16 {
            for _ in 0..channels * nb {
                self.w.sput(self.spec.bps, self.rng.below(512) as i64 - 256);
            }
            return;
        }
        if !self.spec.multichannel {
            for _ in 0..channels {
                self.channel(nb);
            }
            if channels == 2 {
                let two = self.rng.below(2);
                self.w.put(1, two);
                if two == 1 {
                    self.w.put(6, self.rng.below(64));
                }
                let dmode = if nb - 1 >= 256 { self.rng.below(8) } else { self.rng.below(6) } as u32;
                self.w.put(3, u64::from(dmode));
                self.dmode_params(dmode);
            }
            return;
        }
        // Multichannel: decorrelation parameters for some channels.
        if self.rng.chance(30) {
            self.w.put(1, 0);
            for _ in 0..channels {
                self.channel(nb);
            }
            return;
        }
        self.w.put(1, 1);
        // Plan: each entry's channel, and its partner (index 1 decodes the
        // partner first, so the partner never has its own entry).
        let mut queue: Vec<usize> = (0..channels).collect();
        let mut plan = Vec::new();
        let mut decoded = Vec::new();
        while let Some(nbit) = (!queue.is_empty()).then(|| queue.remove(0)) {
            let pick = self.rng.below(5);
            let entry = if pick == 1 && !queue.is_empty() {
                let partner = queue.pop().unwrap();
                decoded.push(partner);
                (nbit, Some((1usize, partner)))
            } else if pick >= 2 && !decoded.is_empty() {
                let index = [0usize, 2, 3][self.rng.below(3) as usize];
                let index = if index == 3 && nb - 1 < 256 { 0 } else { index };
                let partner = decoded[self.rng.below(decoded.len() as u64) as usize];
                (nbit, Some((index, partner)))
            } else {
                (nbit, None)
            };
            decoded.push(nbit);
            plan.push(entry);
        }
        self.w.put(4, plan.len() as u64 - 1);
        for &(nbit, mcd) in &plan {
            self.w.put(4, nbit as u64);
            match mcd {
                Some((index, partner)) => {
                    self.w.put(1, 1);
                    self.w.put(2, index as u64);
                    self.w.put(4, partner as u64);
                }
                None => self.w.put(1, 0),
            }
        }
        for &(_, mcd) in &plan {
            if let Some((1, _)) = mcd {
                self.channel(nb);
            }
            self.channel(nb);
            if let Some((index, _)) = mcd {
                self.dmode_params([1, 3, 4, 6][index]);
            }
        }
    }
}

fn streaminfo_bits(w: &mut W, spec: &Spec, total: u64) {
    w.put(6, if spec.multichannel { 4 } else { 2 });
    w.put(4, 0);
    w.put(4, u64::from(spec.frame_type));
    w.put(35, total);
    w.put(3, 0);
    w.put(18, u64::from(spec.rate - 6000));
    w.put(5, u64::from(spec.bps - 8));
    w.put(4, spec.channels as u64 - 1);
    w.put(1, 0);
}

fn tak_file(spec: &Spec) -> Vec<u8> {
    let fs = frame_samples(spec.rate, spec.frame_type) as usize;
    let mut rng = Rng(spec.seed);
    let last = 1 + rng.below(fs as u64) as usize;
    let total = (spec.frames - 1) * fs + last;
    let aligned = ((spec.rate as usize + 511) >> 9).div_ceil(4) * 4;
    let shift = match spec.rate {
        r if r < 11025 => 3,
        r if r < 22050 => 2,
        r if r < 44100 => 1,
        _ => 0,
    };
    let mut g = Gen { w: W::new(), rng, spec, uval: aligned << shift, subframe_scale: aligned << 1 };

    let mut out = b"tBaK".to_vec();
    let mut info = W::new();
    streaminfo_bits(&mut info, spec, total as u64);
    info.align();
    out.push(1);
    out.extend_from_slice(&(info.out.len() as u32 + 3).to_le_bytes()[..3]);
    out.extend_from_slice(&info.out);
    out.extend_from_slice(&[0, 0, 0]);
    out.extend_from_slice(&[0, 0, 0, 0]);

    for k in 0..spec.frames {
        let is_last = k + 1 == spec.frames;
        let has_info = k % 3 == 0;
        let mut h = W::new();
        h.put(16, 0xA0FF);
        h.put(3, u64::from(is_last) | u64::from(has_info) << 1);
        h.put(21, k as u64);
        if is_last {
            h.put(14, last as u64 - 1);
            h.put(2, 0);
        }
        if has_info {
            streaminfo_bits(&mut h, spec, total as u64);
            h.put(6, 0);
            h.align();
        }
        let crc = crc24(&h.out);
        out.extend_from_slice(&h.out);
        out.extend_from_slice(&[(crc >> 16) as u8, (crc >> 8) as u8, crc as u8]);
        g.w = W::new();
        g.frame_body(if is_last { last } else { fs });
        g.w.align();
        g.w.put(24, 0);
        out.extend_from_slice(&g.w.out);
    }
    out
}

#[test]
fn written_streams_match_ffmpeg() {
    let specs = [
        Spec { name: "s16-stereo-44k", bps: 16, channels: 2, rate: 44100, frame_type: 1, frames: 12, multichannel: false, seed: 1 },
        Spec { name: "s16-stereo-48k-big", bps: 16, channels: 2, rate: 48000, frame_type: 3, frames: 6, multichannel: false, seed: 2 },
        Spec { name: "u8-mono", bps: 8, channels: 1, rate: 8000, frame_type: 2, frames: 15, multichannel: false, seed: 3 },
        Spec { name: "s24-stereo-96k", bps: 24, channels: 2, rate: 96000, frame_type: 0, frames: 10, multichannel: false, seed: 4 },
        Spec { name: "s16-6ch-multichannel", bps: 16, channels: 6, rate: 48000, frame_type: 1, frames: 12, multichannel: true, seed: 5 },
        Spec { name: "s24-4ch-multichannel", bps: 24, channels: 4, rate: 44100, frame_type: 8, frames: 14, multichannel: true, seed: 6 },
        Spec { name: "s16-3ch-small-frames", bps: 16, channels: 3, rate: 22050, frame_type: 7, frames: 20, multichannel: true, seed: 7 },
        Spec { name: "s16-stereo-11k", bps: 16, channels: 2, rate: 11025, frame_type: 9, frames: 10, multichannel: false, seed: 8 },
        // The format's largest supported frame, well beyond a read chunk.
        Spec { name: "s24-6ch-max-frame", bps: 24, channels: 6, rate: 96000, frame_type: 6, frames: 3, multichannel: true, seed: 9 },
    ];
    for spec in &specs {
        let path = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("codec-tak-{}.tak", spec.name));
        std::fs::write(&path, tak_file(spec)).expect("write");
        let decoded = decode(&path, &[codec_tak::register], MediaType::Audio, 0);
        let format = decoded.audio_format.expect("format");
        let (bytes, f) = match spec.bps {
            8 => (1, "u8"),
            16 => (2, "s16le"),
            _ => (4, "s32le"),
        };
        let ours = interleaved(&decoded.frames, usize::from(format.channels), bytes);
        let theirs = run(
            pinned_ffmpeg(),
            &["-nostdin", "-cpuflags", "0", "-i", path.to_str().unwrap(), "-map", "0:a:0", "-f", f, "-"],
        );
        let first = ours.iter().zip(&theirs).position(|(a, b)| a != b);
        assert_eq!(first, None, "{}: first differing byte", spec.name);
        assert_eq!(ours.len(), theirs.len(), "{}: bytes", spec.name);
        assert!(!ours.is_empty(), "{}: no samples", spec.name);
        eprintln!("{}: {} samples/channel, bit-exact", spec.name, ours.len() / bytes / spec.channels);
    }
}
