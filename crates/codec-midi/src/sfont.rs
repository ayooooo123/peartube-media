//! SoundFont 2 loading: FluidSynth 2.6.1 `sfloader/fluid_sffile.c` (the
//! RIFF parser and its structural checks) and `sfloader/fluid_defsfont.c`
//! (presets, instruments, zones, samples and modulators as the synth plays
//! them). The whole sample chunk is loaded, as FluidSynth does without
//! dynamic sample loading.

use std::io::{Read, Seek, SeekFrom};

use crate::generator;
use crate::modulator::{self, Mod};

pub const SAMPLETYPE_MONO: i32 = 1;
const SAMPLETYPE_RIGHT: i32 = 2;
const SAMPLETYPE_LEFT: i32 = 4;
const SAMPLETYPE_LINKED: i32 = 8;
pub const SAMPLETYPE_OGG_VORBIS: i32 = 0x10;
const SAMPLETYPE_ROM: i32 = 0x8000;

/// `EMU_ATTENUATION_FACTOR`: SoundFont attenuation is scaled as on EMU
/// hardware.
const EMU_ATTENUATION_FACTOR: f32 = 0.4;

/// `FLUID_NUM_MOD`: the modulators a zone or voice holds at most.
pub const NUM_MOD: usize = 64;

/// A loading failure, worded for the user.
#[derive(Debug)]
pub struct LoadError(pub String);

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<std::io::Error> for LoadError {
    fn from(e: std::io::Error) -> Self {
        LoadError(format!("SoundFont read error: {e}"))
    }
}

fn bad(msg: &str) -> LoadError {
    LoadError(format!("not a usable SoundFont: {msg}"))
}

type Result<T, E = LoadError> = std::result::Result<T, E>;

/// `fluid_sample_t`, its data in [`SoundFont::data`].
#[derive(Clone, Debug)]
pub struct Sample {
    pub start: u32,
    /// The last sample point (not one past it, unlike the file).
    pub end: u32,
    pub loopstart: u32,
    /// The first point after the loop.
    pub loopend: u32,
    pub samplerate: u32,
    pub origpitch: i32,
    pub pitchadj: i32,
    pub sampletype: i32,
    /// `amplitude_that_reaches_noise_floor`, when the loop is not silent.
    pub noise_floor_amplitude: Option<f64>,
}

/// `fluid_zone_range_t`.
#[derive(Clone, Copy, Debug)]
pub struct Range {
    pub keylo: i32,
    pub keyhi: i32,
    pub vello: i32,
    pub velhi: i32,
}

impl Range {
    const ALL: Range = Range {
        keylo: 0,
        keyhi: 128,
        vello: 0,
        velhi: 128,
    };

    /// `fluid_zone_inside_range`.
    pub fn inside(&self, key: i32, vel: i32) -> bool {
        self.keylo <= key && self.keyhi >= key && self.vello <= vel && self.velhi >= vel
    }
}

/// A zone's generators: (set, value) per generator.
pub type ZoneGens = [(bool, f64); generator::LAST];

#[derive(Clone, Debug)]
pub struct InstZone {
    pub range: Range,
    pub gens: ZoneGens,
    pub mods: Vec<Mod>,
    /// Index into [`SoundFont::samples`].
    pub sample: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct Inst {
    pub global: Option<InstZone>,
    /// In FluidSynth's order: last file zone first.
    pub zones: Vec<InstZone>,
}

/// An instrument zone of a preset zone that can start a voice, with the
/// intersection of both ranges.
#[derive(Clone, Copy, Debug)]
pub struct VoiceZone {
    pub inst_zone: usize,
    pub range: Range,
}

#[derive(Clone, Debug)]
pub struct PresetZone {
    pub range: Range,
    pub gens: ZoneGens,
    pub mods: Vec<Mod>,
    /// Index into [`SoundFont::insts`].
    pub inst: Option<usize>,
    pub voice_zones: Vec<VoiceZone>,
}

#[derive(Clone, Debug)]
pub struct Preset {
    pub bank: i32,
    pub num: i32,
    pub global: Option<PresetZone>,
    /// In FluidSynth's order: last file zone first.
    pub zones: Vec<PresetZone>,
}

/// A loaded SoundFont: `fluid_defsfont_t`.
pub struct SoundFont {
    /// Sorted by bank, then program, as `fluid_list_sort` leaves them.
    pub presets: Vec<Preset>,
    pub insts: Vec<Inst>,
    pub samples: Vec<Sample>,
    /// The `smpl` chunk.
    pub data: Vec<i16>,
    /// The `sm24` chunk, when it matches `smpl`.
    pub data24: Option<Vec<u8>>,
    /// The `DMOD` chunk's modulators, replacing the synth's defaults.
    pub default_mods: Option<Vec<Mod>>,
}

impl SoundFont {
    /// `fluid_defsfont_get_preset`: the first preset of `bank`, `num`.
    pub fn preset(&self, bank: i32, num: i32) -> Option<usize> {
        self.presets
            .iter()
            .position(|p| p.bank == bank && p.num == num)
    }

    /// Loads a SoundFont 2 file, as `fluid_defsfont_load` does.
    pub fn load(file: &mut (impl Read + Seek)) -> Result<SoundFont> {
        let raw = RawFont::parse(file)?;
        import(raw, file)
    }
}

// ───────────────────────── fluid_sffile.c ─────────────────────────

#[derive(Clone, Copy, Debug)]
enum Amount {
    Range(u8, u8),
    Word(u16),
}

impl Amount {
    fn sword(self) -> i16 {
        match self {
            Amount::Word(w) => w as i16,
            Amount::Range(lo, hi) => i16::from_le_bytes([lo, hi]),
        }
    }
    fn uword(self) -> u16 {
        self.sword() as u16
    }
}

#[derive(Clone, Copy, Debug)]
struct SfGen {
    id: u16,
    amount: Amount,
}

#[derive(Clone, Copy, Debug)]
struct SfMod {
    src: u16,
    dest: u16,
    amount: i16,
    amtsrc: u16,
    trans: u16,
}

#[derive(Clone, Debug, Default)]
struct SfZone {
    gens: Vec<SfGen>,
    mods: Vec<SfMod>,
}

#[derive(Clone, Debug)]
struct SfPreset {
    prenum: u16,
    bank: u16,
    zones: Vec<SfZone>,
}

#[derive(Clone, Copy, Debug)]
struct SfSample {
    start: u32,
    end: u32,
    loopstart: u32,
    loopend: u32,
    samplerate: u32,
    origpitch: u8,
    pitchadj: i8,
    sampletype: u16,
}

struct RawFont {
    major: u16,
    default_mods: Vec<SfMod>,
    samplepos: u64,
    samplesize: u32,
    sample24pos: u64,
    sample24size: u32,
    presets: Vec<SfPreset>,
    insts: Vec<Vec<SfZone>>,
    samples: Vec<SfSample>,
}

const GEN_SIZE: u32 = 4;
const MOD_SIZE: u32 = 10;
const BAG_SIZE: u32 = 4;
const PHDR_SIZE: u32 = 38;
const IHDR_SIZE: u32 = 22;
const SHDR_SIZE: u32 = 46;

struct Reader<'a, R: Read + Seek> {
    r: &'a mut R,
    end: u64,
}

impl<R: Read + Seek> Reader<'_, R> {
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut b = [0u8; N];
        self.r
            .read_exact(&mut b)
            .map_err(|_| bad("file ends early"))?;
        Ok(b)
    }
    fn id(&mut self) -> Result<[u8; 4]> {
        self.bytes::<4>()
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.bytes::<4>()?))
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.bytes::<2>()?))
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes::<1>()?[0])
    }
    fn skip(&mut self, n: u64) -> Result<()> {
        let next = self
            .r
            .stream_position()?
            .checked_add(n)
            .filter(|&p| p <= self.end)
            .ok_or_else(|| bad("chunk exceeds the file"))?;
        self.r.seek(SeekFrom::Start(next))?;
        Ok(())
    }
    fn pos(&mut self) -> Result<u64> {
        Ok(self.r.stream_position()?)
    }
    fn chunk(&mut self) -> Result<([u8; 4], u32)> {
        let (id, size) = (self.id()?, self.u32()?);
        if u64::from(size) > self.end.saturating_sub(self.pos()?) {
            return Err(bad("chunk exceeds the file"));
        }
        Ok((id, size))
    }
    /// `read_listchunk`: a LIST chunk and its id; the size left after it.
    fn list(&mut self) -> Result<([u8; 4], u32)> {
        let (id, size) = self.chunk()?;
        if &id != b"LIST" {
            return Err(bad("expected a LIST chunk"));
        }
        if size < 4 {
            return Err(bad("expected a LIST chunk"));
        }
        let id = self.id()?;
        Ok((id, size - 4))
    }
    fn mod_record(&mut self) -> Result<SfMod> {
        Ok(SfMod {
            src: self.u16()?,
            dest: self.u16()?,
            amount: self.u16()? as i16,
            amtsrc: self.u16()?,
            trans: self.u16()?,
        })
    }
}

impl RawFont {
    fn parse<R: Read + Seek>(file: &mut R) -> Result<RawFont> {
        let filesize = file.seek(SeekFrom::End(0))?;
        if !(12..=256 * 1024 * 1024).contains(&filesize) {
            return Err(bad("SoundFont must be at most 256 MiB"));
        }
        file.seek(SeekFrom::Start(0))?;
        let mut r = Reader {
            r: file,
            end: filesize,
        };
        // load_header
        let (id, size) = r.chunk()?;
        if &id != b"RIFF" {
            return Err(bad("not a RIFF file"));
        }
        if &r.id()? != b"sfbk" {
            return Err(bad("not a SoundFont file"));
        }
        if u64::from(size) != filesize.wrapping_sub(8) {
            return Err(bad("file size mismatch"));
        }
        let mut font = RawFont {
            major: 0,
            default_mods: Vec::new(),
            samplepos: 0,
            samplesize: 0,
            sample24pos: 0,
            sample24size: 0,
            presets: Vec::new(),
            insts: Vec::new(),
            samples: Vec::new(),
        };
        let (id, size) = r.list()?;
        if &id != b"INFO" {
            return Err(bad("expected the INFO chunk"));
        }
        let minor = font.process_info(&mut r, size)?;
        let (id, size) = r.list()?;
        if &id != b"sdta" {
            return Err(bad("expected the sample chunk"));
        }
        font.process_sdta(&mut r, size, minor)?;
        let (id, size) = r.list()?;
        if &id != b"pdta" {
            return Err(bad("expected the preset chunk"));
        }
        font.process_pdta(&mut r, size as i64)?;
        // load_body: presets sorted by bank, then number (a stable sort).
        font.presets
            .sort_by_key(|p| (u32::from(p.bank) << 16) | u32::from(p.prenum));
        Ok(font)
    }

    /// `process_info`; returns the minor version.
    fn process_info<R: Read + Seek>(&mut self, r: &mut Reader<R>, size: u32) -> Result<u16> {
        let mut size = i64::from(size);
        let mut minor = 0;
        while size > 0 {
            let (id, csize) = r.chunk()?;
            size -= 8;
            if i64::from(csize) > size {
                return Err(bad("INFO sub-chunk exceeds its parent"));
            }
            match &id {
                b"ifil" => {
                    if csize != 4 {
                        return Err(bad("version chunk has an invalid size"));
                    }
                    self.major = r.u16()?;
                    minor = r.u16()?;
                    if self.major < 2 {
                        return Err(LoadError(format!(
                            "SoundFont version {}.{} is not supported, convert it to 2.0x",
                            self.major, minor
                        )));
                    }
                    if self.major == 3 {
                        return Err(LoadError(
                            "SF3 SoundFonts (Ogg Vorbis samples) are not supported; use an SF2"
                                .into(),
                        ));
                    }
                    if self.major > 2 {
                        return Err(LoadError(format!(
                            "SoundFont version {}.{} is too new",
                            self.major, minor
                        )));
                    }
                }
                b"iver" => {
                    if csize != 4 {
                        return Err(bad("ROM version chunk has an invalid size"));
                    }
                    r.skip(4)?;
                }
                b"DMOD" => {
                    if csize < MOD_SIZE || csize % MOD_SIZE != 0 || size == 0 {
                        return Err(bad("DMOD chunk has an invalid size"));
                    }
                    let count = csize / MOD_SIZE - 1;
                    if count > 256 {
                        return Err(bad("too many default modulators"));
                    }
                    // fluid_list_prepend: the list holds them last first.
                    let mut mods = Vec::with_capacity(count as usize);
                    for _ in 0..count {
                        mods.push(r.mod_record()?);
                    }
                    mods.reverse();
                    self.default_mods = mods;
                    r.skip(u64::from(MOD_SIZE))?;
                }
                _ => {
                    if csize % 2 != 0 {
                        return Err(bad("INFO sub-chunk has an odd size"));
                    }
                    r.skip(u64::from(csize))?;
                }
            }
            size -= i64::from(csize);
        }
        if size < 0 {
            return Err(bad("INFO chunk size mismatch"));
        }
        Ok(minor)
    }

    fn process_sdta<R: Read + Seek>(
        &mut self,
        r: &mut Reader<R>,
        size: u32,
        minor: u16,
    ) -> Result<()> {
        if size == 0 {
            return Ok(());
        }
        let mut size = size;
        let (id, csize) = r.chunk()?;
        size = size
            .checked_sub(8)
            .ok_or_else(|| bad("sample chunk is too small"))?;
        if &id != b"smpl" {
            return Err(bad("expected the smpl chunk"));
        }
        if csize > size {
            return Err(bad("sample chunk size mismatch"));
        }
        self.samplepos = r.pos()?;
        self.samplesize = csize;
        r.skip(u64::from(csize))?;
        size -= csize;
        if self.major >= 2 && minor >= 4 && size > 8 {
            let (id, csize) = r.chunk()?;
            size -= 8;
            if &id == b"sm24" && csize <= size {
                let mut half = self.samplesize / 2;
                half += half % 2;
                if half == csize {
                    self.sample24pos = r.pos()?;
                    self.sample24size = csize;
                }
            }
        }
        r.skip(u64::from(size))?;
        Ok(())
    }

    fn pdta_chunk<R: Read + Seek>(
        r: &mut Reader<R>,
        expect: &[u8; 4],
        reclen: u32,
        size: &mut i64,
    ) -> Result<u32> {
        let (id, csize) = r.chunk()?;
        if &id != expect {
            return Err(bad("unexpected preset sub-chunk"));
        }
        if csize % reclen != 0 || csize / reclen > 65_536 {
            return Err(bad("preset sub-chunk has an invalid size"));
        }
        *size -= i64::from(csize) + 8;
        if *size < 0 {
            return Err(bad("preset sub-chunk exceeds the preset chunk"));
        }
        Ok(csize)
    }

    fn process_pdta<R: Read + Seek>(&mut self, r: &mut Reader<R>, mut size: i64) -> Result<()> {
        if size > 8 * 1024 * 1024 {
            return Err(bad("preset metadata exceeds 8 MiB"));
        }
        let n = Self::pdta_chunk(r, b"phdr", PHDR_SIZE, &mut size)?;
        let preset_bags = self.load_phdr(r, n)?;
        let n = Self::pdta_chunk(r, b"pbag", BAG_SIZE, &mut size)?;
        let (pgen_counts, pmod_counts) = load_bags(r, n, &preset_bags)?;
        let n = Self::pdta_chunk(r, b"pmod", MOD_SIZE, &mut size)?;
        let pmods = load_mods(r, n, &pmod_counts)?;
        let n = Self::pdta_chunk(r, b"pgen", GEN_SIZE, &mut size)?;
        let pzones = load_gens(
            r,
            n,
            &pgen_counts,
            pmods,
            generator::INSTRUMENT as u16,
            valid_preset_genid,
        )?;
        for (p, zones) in self
            .presets
            .iter_mut()
            .zip(group_zones(pzones, &preset_bags))
        {
            p.zones = zones;
        }

        let n = Self::pdta_chunk(r, b"inst", IHDR_SIZE, &mut size)?;
        let inst_bags = load_ihdr(r, n)?;
        let n = Self::pdta_chunk(r, b"ibag", BAG_SIZE, &mut size)?;
        let (igen_counts, imod_counts) = load_bags(r, n, &inst_bags)?;
        let n = Self::pdta_chunk(r, b"imod", MOD_SIZE, &mut size)?;
        let imods = load_mods(r, n, &imod_counts)?;
        let n = Self::pdta_chunk(r, b"igen", GEN_SIZE, &mut size)?;
        let izones = load_gens(
            r,
            n,
            &igen_counts,
            imods,
            generator::SAMPLEID as u16,
            valid_inst_genid,
        )?;
        self.insts = group_zones(izones, &inst_bags);

        let n = Self::pdta_chunk(r, b"shdr", SHDR_SIZE, &mut size)?;
        self.load_shdr(r, n)?;
        if size != 0 {
            return Err(bad("preset chunk size mismatch"));
        }
        Ok(())
    }

    /// `load_phdr`: the presets and their zone counts. Zones before the
    /// first preset's are not referenced; the bag chunk then has more
    /// records than the presets use, and `load_bags` rejects it.
    fn load_phdr<R: Read + Seek>(&mut self, r: &mut Reader<R>, size: u32) -> Result<Vec<usize>> {
        if size == 0 {
            return Err(bad("preset header chunk size is invalid"));
        }
        let count = size / PHDR_SIZE - 1;
        let mut bags = Vec::new();
        if count == 0 {
            r.skip(u64::from(PHDR_SIZE))?;
            return Ok(bags);
        }
        let mut prev_idx: Option<u16> = None;
        for _ in 0..count {
            r.skip(20)?;
            let prenum = r.u16()?;
            let bank = r.u16()?;
            let idx = r.u16()?;
            r.skip(12)?;
            if let Some(prev) = prev_idx {
                if idx < prev {
                    return Err(bad("preset header indices not monotonic"));
                }
                bags.push(usize::from(idx - prev));
            }
            self.presets.push(SfPreset {
                prenum,
                bank,
                zones: Vec::new(),
            });
            prev_idx = Some(idx);
        }
        r.skip(24)?;
        let idx = r.u16()?;
        r.skip(12)?;
        let prev = prev_idx.unwrap_or(0);
        if idx < prev {
            return Err(bad("preset header indices not monotonic"));
        }
        bags.push(usize::from(idx - prev));
        Ok(bags)
    }

    /// `load_shdr`.
    fn load_shdr<R: Read + Seek>(&mut self, r: &mut Reader<R>, size: u32) -> Result<()> {
        if size == 0 {
            return Err(bad("sample header has an invalid size"));
        }
        let count = size / SHDR_SIZE - 1;
        for _ in 0..count {
            r.skip(20)?;
            let start = r.u32()?;
            let end = r.u32()?;
            let loopstart = r.u32()?;
            let loopend = r.u32()?;
            let samplerate = r.u32()?;
            let origpitch = r.u8()?;
            let pitchadj = r.u8()? as i8;
            r.skip(2)?;
            let sampletype = r.u16()?;
            if sampletype & SAMPLETYPE_OGG_VORBIS as u16 != 0 {
                return Err(bad("compressed SF3 samples are not supported; use an SF2"));
            }
            self.samples.push(SfSample {
                start,
                end,
                loopstart,
                loopend,
                samplerate,
                origpitch,
                pitchadj,
                sampletype,
            });
        }
        r.skip(u64::from(SHDR_SIZE))?;
        Ok(())
    }
}

/// `load_ihdr`: the instruments' zone counts.
fn load_ihdr<R: Read + Seek>(r: &mut Reader<R>, size: u32) -> Result<Vec<usize>> {
    if size == 0 {
        return Err(bad("instrument header has an invalid size"));
    }
    let count = size / IHDR_SIZE - 1;
    let mut bags = Vec::new();
    if count == 0 {
        r.skip(u64::from(IHDR_SIZE))?;
        return Ok(bags);
    }
    let mut prev: Option<u16> = None;
    for _ in 0..count {
        r.skip(20)?;
        let idx = r.u16()?;
        if let Some(p) = prev {
            if idx < p {
                return Err(bad("instrument header indices not monotonic"));
            }
            bags.push(usize::from(idx - p));
        }
        prev = Some(idx);
    }
    r.skip(20)?;
    let idx = r.u16()?;
    let p = prev.unwrap_or(0);
    if idx < p {
        return Err(bad("instrument header indices not monotonic"));
    }
    bags.push(usize::from(idx - p));
    Ok(bags)
}

/// `load_pbag` / `load_ibag`: the generator and modulator counts of every
/// zone, in file order (`bags` zones per preset or instrument).
fn load_bags<R: Read + Seek>(
    r: &mut Reader<R>,
    size: u32,
    bags: &[usize],
) -> Result<(Vec<usize>, Vec<usize>)> {
    if size == 0 {
        return Err(bad("bag chunk size is invalid"));
    }
    let mut size = i64::from(size);
    let zones: usize = bags.iter().sum();
    let (mut gens, mut mods) = (Vec::with_capacity(zones), Vec::with_capacity(zones));
    let mut prev: Option<(u16, u16)> = None;
    for _ in 0..zones {
        size -= i64::from(BAG_SIZE);
        if size < 0 {
            return Err(bad("bag chunk size mismatch"));
        }
        let genndx = r.u16()?;
        let modndx = r.u16()?;
        if let Some((pg, pm)) = prev {
            if genndx < pg || modndx < pm {
                return Err(bad("bag indices not monotonic"));
            }
            gens.push(usize::from(genndx - pg));
            mods.push(usize::from(modndx - pm));
        }
        prev = Some((genndx, modndx));
    }
    size -= i64::from(BAG_SIZE);
    if size != 0 {
        return Err(bad("bag chunk size mismatch"));
    }
    let genndx = r.u16()?;
    let modndx = r.u16()?;
    if let Some((pg, pm)) = prev {
        if genndx < pg || modndx < pm {
            return Err(bad("bag indices not monotonic"));
        }
        gens.push(usize::from(genndx - pg));
        mods.push(usize::from(modndx - pm));
    }
    Ok((gens, mods))
}

/// `load_pmod` / `load_imod`.
fn load_mods<R: Read + Seek>(
    r: &mut Reader<R>,
    size: u32,
    counts: &[usize],
) -> Result<Vec<Vec<SfMod>>> {
    let mut size = i64::from(size);
    let mut out = Vec::with_capacity(counts.len());
    for &count in counts {
        if count > 256 {
            return Err(bad("too many modulators in a zone"));
        }
        let mut mods = Vec::with_capacity(count);
        for _ in 0..count {
            size -= i64::from(MOD_SIZE);
            if size < 0 {
                return Err(bad("modulator chunk size mismatch"));
            }
            mods.push(r.mod_record()?);
        }
        out.push(mods);
    }
    if size == 0 {
        return Ok(out);
    }
    size -= i64::from(MOD_SIZE);
    if size != 0 {
        return Err(bad("modulator chunk size mismatch"));
    }
    r.skip(u64::from(MOD_SIZE))?;
    Ok(out)
}

fn valid_inst_genid(id: u16) -> bool {
    let id = usize::from(id);
    id <= generator::OVERRIDEROOTKEY
        && ![
            generator::UNUSED1,
            generator::UNUSED2,
            generator::UNUSED3,
            generator::UNUSED4,
            generator::RESERVED1,
            generator::RESERVED2,
            generator::RESERVED3,
            generator::INSTRUMENT,
        ]
        .contains(&id)
}

fn valid_preset_genid(id: u16) -> bool {
    valid_inst_genid(id)
        && ![
            generator::STARTADDROFS,
            generator::ENDADDROFS,
            generator::STARTLOOPADDROFS,
            generator::ENDLOOPADDROFS,
            generator::STARTADDRCOARSEOFS,
            generator::ENDADDRCOARSEOFS,
            generator::STARTLOOPADDRCOARSEOFS,
            generator::KEYNUM,
            generator::VELOCITY,
            generator::ENDLOOPADDRCOARSEOFS,
            generator::SAMPLEMODE,
            generator::EXCLUSIVECLASS,
            generator::OVERRIDEROOTKEY,
            generator::SAMPLEID,
        ]
        .contains(&usize::from(id))
}

/// `load_pgen` / `load_igen`: every zone's generators, checked as
/// FluidSynth checks them, and whether the zone reached its terminal
/// generator (`terminal`: the instrument or the sample).
fn load_gens<R: Read + Seek>(
    r: &mut Reader<R>,
    size: u32,
    counts: &[usize],
    mods: Vec<Vec<SfMod>>,
    terminal: u16,
    valid: fn(u16) -> bool,
) -> Result<Vec<(SfZone, bool)>> {
    let mut size = i64::from(size);
    let mut out = Vec::with_capacity(counts.len());
    for (&count, zone_mods) in counts.iter().zip(mods) {
        let mut gens: Vec<SfGen> = Vec::with_capacity(count);
        let mut level = 0;
        let mut consumed = 0;
        while consumed < count {
            consumed += 1;
            size -= i64::from(GEN_SIZE);
            if size < 0 {
                return Err(bad("generator chunk size mismatch"));
            }
            let id = r.u16()?;
            let lo = r.u8()?;
            let hi = r.u8()?;
            let amount = Amount::Range(lo, hi);
            if usize::from(id) == generator::KEYRANGE {
                // Only as the first generator (SF2.01 8.1.2).
                if level == 0 {
                    level = 1;
                    gens.push(SfGen { id, amount });
                }
            } else if usize::from(id) == generator::VELRANGE {
                // Only first, or after the key range.
                if level <= 1 {
                    level = 2;
                    gens.push(SfGen { id, amount });
                }
            } else if id == terminal {
                level = 3;
                gens.push(SfGen {
                    id,
                    amount: Amount::Word(amount.uword()),
                });
                break;
            } else {
                level = 2;
                if valid(id) {
                    let amount = Amount::Word(amount.uword());
                    // A duplicate keeps its place and takes the last value.
                    if let Some(g) = gens.iter_mut().find(|g| g.id == id) {
                        g.amount = amount;
                    } else {
                        gens.push(SfGen { id, amount });
                    }
                }
            }
        }
        // The generators after the terminal one are discarded.
        while consumed < count {
            consumed += 1;
            size -= i64::from(GEN_SIZE);
            if size < 0 {
                return Err(bad("generator chunk size mismatch"));
            }
            r.skip(u64::from(GEN_SIZE))?;
        }
        out.push((
            SfZone {
                gens,
                mods: zone_mods,
            },
            level == 3,
        ));
    }
    if size != 0 {
        size -= i64::from(GEN_SIZE);
        if size != 0 {
            return Err(bad("generator chunk size mismatch"));
        }
        r.skip(u64::from(GEN_SIZE))?;
    }
    Ok(out)
}

/// Splits the zones among their owners (`counts` zones each). An owner's
/// first zone may lack the terminal generator: it is the global zone. A
/// later zone without it is discarded (SF2.01 7.3 and 7.7).
fn group_zones(zones: Vec<(SfZone, bool)>, counts: &[usize]) -> Vec<Vec<SfZone>> {
    let mut zones = zones.into_iter();
    counts
        .iter()
        .map(|&count| {
            zones
                .by_ref()
                .take(count)
                .enumerate()
                .filter(|(i, (_, terminal))| *i == 0 || *terminal)
                .map(|(_, (zone, _))| zone)
                .collect()
        })
        .collect()
}

// ───────────────────────── fluid_defsfont.c ─────────────────────────

/// `fluid_zone_mod_source_import_sfont`: source, flags, and whether the
/// curve type is known.
fn mod_source(sf_source: u16) -> (u8, u8, bool) {
    let src = (sf_source & 127) as u8;
    let mut flags = 0u8;
    if sf_source & (1 << 7) != 0 {
        flags |= modulator::CC;
    }
    if sf_source & (1 << 8) != 0 {
        flags |= modulator::NEGATIVE;
    }
    if sf_source & (1 << 9) != 0 {
        flags |= modulator::BIPOLAR;
    }
    let ok = match (sf_source >> 10) & 63 {
        0 => true,
        1 => {
            flags |= modulator::CONCAVE;
            true
        }
        2 => {
            flags |= modulator::CONVEX;
            true
        }
        3 => {
            flags |= modulator::SWITCH;
            true
        }
        _ => false,
    };
    (src, flags, ok)
}

/// `fluid_mod_import_sfont`.
fn import_mods(sfmods: &[SfMod]) -> Vec<Mod> {
    sfmods
        .iter()
        // Check the on-disk u16 before narrowing or duplicate admission.
        .filter(|m| usize::from(m.dest) < generator::LAST)
        .map(|m| {
            let mut amount = f64::from(m.amount);
            let (src1, flags1, ok1) = mod_source(m.src);
            let (src2, flags2, ok2) = mod_source(m.amtsrc);
            if !ok1 || !ok2 {
                amount = 0.0;
            }
            let trans = if m.trans != u16::from(modulator::TRANSFORM_LINEAR)
                && m.trans != u16::from(modulator::TRANSFORM_ABS)
            {
                amount = 0.0;
                modulator::TRANSFORM_LINEAR
            } else {
                m.trans as u8
            };
            Mod {
                dest: m.dest as u8,
                src1,
                flags1,
                src2,
                flags2,
                trans,
                amount,
            }
        })
        .collect()
}

/// `fluid_zone_check_mod`: invalid sources and modulators identical to a
/// later one are dropped, then the list is cut at [`NUM_MOD`].
fn check_mods(mods: Vec<Mod>) -> Vec<Mod> {
    let mut kept: Vec<Mod> = Vec::with_capacity(mods.len());
    for (i, m) in mods.iter().enumerate() {
        if !m.sources_valid() || mods[i + 1..].iter().any(|n| m.same_as(n)) {
            continue;
        }
        kept.push(*m);
    }
    kept.truncate(NUM_MOD);
    kept
}

/// `fluid_zone_gen_import_sfont`.
fn import_gens(sfzone: &SfZone, global_range: Option<Range>) -> (Range, ZoneGens) {
    let mut gens: ZoneGens = [(false, 0.0); generator::LAST];
    for (i, g) in generator::init(None).iter().enumerate() {
        gens[i] = (false, g.val);
    }
    let mut range = global_range.unwrap_or(Range::ALL);
    for g in &sfzone.gens {
        let id = usize::from(g.id);
        match id {
            generator::KEYRANGE | generator::VELRANGE => {
                if let Amount::Range(lo, hi) = g.amount {
                    if id == generator::KEYRANGE {
                        (range.keylo, range.keyhi) = (i32::from(lo), i32::from(hi));
                    } else {
                        (range.vello, range.velhi) = (i32::from(lo), i32::from(hi));
                    }
                }
            }
            generator::ATTENUATION => {
                gens[id] = (
                    true,
                    f64::from(g.amount.sword()) * f64::from(EMU_ATTENUATION_FACTOR),
                )
            }
            generator::INSTRUMENT | generator::SAMPLEID => {
                gens[id] = (true, f64::from(g.amount.uword()))
            }
            _ if id < generator::LAST => gens[id] = (true, f64::from(g.amount.sword())),
            _ => {}
        }
    }
    (range, gens)
}

/// `fluid_sample_validate`: `None` for a sample FluidSynth ignores.
fn validate_sample(s: &SfSample, samplesize: u32) -> Option<Sample> {
    const EXCLUSIVE: i32 = SAMPLETYPE_MONO | SAMPLETYPE_RIGHT | SAMPLETYPE_LEFT;
    const SUPPORTED: i32 = EXCLUSIVE | SAMPLETYPE_LINKED | SAMPLETYPE_OGG_VORBIS | SAMPLETYPE_ROM;
    let mut sampletype = i32::from(s.sampletype);
    let end = if s.end > 0 { s.end - 1 } else { 0 };
    if sampletype & SAMPLETYPE_ROM != 0 || sampletype & !SUPPORTED != 0 {
        return None;
    }
    if sampletype & EXCLUSIVE == 0 {
        sampletype = SAMPLETYPE_MONO;
    }
    if samplesize % 2 != 0 {
        return None;
    }
    if end >= samplesize / 2 || s.start >= end || s.samplerate == 0 {
        return None;
    }
    Some(Sample {
        start: s.start,
        end,
        loopstart: s.loopstart,
        loopend: s.loopend,
        samplerate: s.samplerate,
        origpitch: i32::from(s.origpitch),
        pitchadj: i32::from(s.pitchadj),
        sampletype,
        noise_floor_amplitude: None,
    })
}

/// `fluid_sample_sanitize_loop`.
fn sanitize_loop(s: &mut Sample, buffer_size: u32) {
    let max_end = buffer_size / 2;
    let sample_end = s.end + 1;
    if s.loopstart != s.loopend && s.loopstart > s.loopend {
        std::mem::swap(&mut s.loopstart, &mut s.loopend);
    }
    if s.loopstart < s.start || s.loopstart > max_end {
        s.loopstart = s.start;
    }
    if s.loopend < s.start || s.loopend > max_end {
        s.loopend = sample_end;
    }
}

/// `fluid_voice_optimize_sample`: the loop's peak, for the voice to know
/// when a looped release has decayed below the noise floor.
fn optimize_sample(s: &mut Sample, data: &[i16], data24: Option<&[u8]>) {
    const NOISE_FLOOR: f64 = 2.0e-7;
    const INT24_MAX: f64 = (1 << 23) as f64;
    if s.start == s.end {
        return;
    }
    let (mut peak_max, mut peak_min) = (0i32, 0i32);
    for i in s.loopstart..s.loopend {
        let val = sample_at(data, data24, i as usize);
        if val > peak_max {
            peak_max = val;
        } else if val < peak_min {
            peak_min = val;
        }
    }
    let peak = if peak_max > -peak_min {
        peak_max
    } else {
        -peak_min
    };
    let peak = if peak == 0 { 1 } else { peak };
    let normalized = f64::from(peak) / (INT24_MAX * 1.0);
    s.noise_floor_amplitude = Some(NOISE_FLOOR / normalized);
}

/// `fluid_rvoice_get_sample`: a 24-bit sample point.
pub fn sample_at(data: &[i16], data24: Option<&[u8]>, idx: usize) -> i32 {
    let msb = (i32::from(data[idx]) as u32) << 8;
    match data24 {
        Some(lsb) => (msb | u32::from(lsb[idx])) as i32,
        None => msb as i32,
    }
}

fn import<R: Read + Seek>(raw: RawFont, file: &mut R) -> Result<SoundFont> {
    let RawFont {
        default_mods,
        samplepos,
        samplesize,
        sample24pos,
        sample24size,
        presets,
        insts,
        samples,
        ..
    } = raw;
    let default_mods = if default_mods.is_empty() {
        None
    } else {
        Some(import_mods(&default_mods))
    };

    // Samples (fluid_sample_import_sfont); FluidSynth references them by
    // header index.
    let sample_slots: Vec<Option<Sample>> = samples
        .iter()
        .map(|s| validate_sample(s, samplesize))
        .collect();

    // fluid_defsfont_load_all_sampledata
    let num = (samplesize / 2) as usize;
    let mut data = vec![0i16; num];
    file.seek(SeekFrom::Start(samplepos))?;
    let mut chunk = vec![0u8; 1 << 16];
    let mut filled = 0;
    while filled < num {
        let n = ((num - filled) * 2).min(chunk.len());
        file.read_exact(&mut chunk[..n])
            .map_err(|_| bad("sample data ends early"))?;
        for (d, b) in data[filled..filled + n / 2]
            .iter_mut()
            .zip(chunk[..n].chunks_exact(2))
        {
            *d = i16::from_le_bytes([b[0], b[1]]);
        }
        filled += n / 2;
    }
    let data24 = if sample24pos != 0 && num > 0 && (num as u32 - 1) <= sample24size {
        let mut lsb = vec![0u8; num];
        file.seek(SeekFrom::Start(sample24pos))?;
        file.read_exact(&mut lsb).ok().map(|_| lsb)
    } else {
        None
    };

    let mut out_samples = Vec::new();
    let mut sample_index: Vec<Option<usize>> = Vec::with_capacity(sample_slots.len());
    let mut scan_points = 0u64;
    for slot in sample_slots {
        let index = if let Some(mut s) = slot {
            sanitize_loop(&mut s, samplesize);
            scan_points += u64::from(s.loopend.saturating_sub(s.loopstart));
            if scan_points > 128 * 1024 * 1024 {
                return Err(bad("sample loop scan exceeds 128 million points"));
            }
            optimize_sample(&mut s, &data, data24.as_deref());
            out_samples.push(s);
            Some(out_samples.len() - 1)
        } else {
            None
        };
        sample_index.push(index);
    }

    // Presets, importing instruments as they are referenced.
    let mut font = SoundFont {
        presets: Vec::new(),
        insts: Vec::new(),
        samples: out_samples,
        data,
        data24,
        default_mods,
    };
    let mut inst_index: Vec<Option<usize>> = vec![None; insts.len()];
    let mut voice_zone_count = 0usize;
    for sfpreset in presets {
        let mut preset = Preset {
            bank: i32::from(sfpreset.bank),
            num: i32::from(sfpreset.prenum),
            global: None,
            zones: Vec::new(),
        };
        for (count, sfzone) in sfpreset.zones.into_iter().enumerate() {
            let global_range = preset.global.as_ref().map(|z| z.range);
            let (range, gens) = import_gens(&sfzone, global_range);
            let mut zone = PresetZone {
                range,
                gens,
                mods: Vec::new(),
                inst: None,
                voice_zones: Vec::new(),
            };
            if zone.gens[generator::INSTRUMENT].0 {
                let idx = zone.gens[generator::INSTRUMENT].1 as usize;
                let inst = match inst_index.get(idx).copied() {
                    Some(Some(i)) => i,
                    Some(None) => {
                        let inst = import_inst(&insts[idx], &sample_index)?;
                        font.insts.push(inst);
                        inst_index[idx] = Some(font.insts.len() - 1);
                        font.insts.len() - 1
                    }
                    None => return Err(bad("a preset zone references a missing instrument")),
                };
                zone.inst = Some(inst);
                voice_zone_count += font.insts[inst].zones.len();
                if voice_zone_count > 262_144 {
                    return Err(bad("too many expanded voice zones"));
                }
                zone.voice_zones = voice_zones(&zone.range, &font.insts[inst], &font.samples);
                zone.gens[generator::INSTRUMENT].0 = false;
            }
            zone.mods = check_mods(import_mods(&sfzone.mods));
            if count == 0 && zone.inst.is_none() {
                preset.global = Some(zone);
            } else {
                // fluid_defpreset_add_zone prepends.
                preset.zones.push(zone);
            }
        }
        preset.zones.reverse();
        font.presets.push(preset);
    }
    if font.samples.is_empty()
        || !font
            .presets
            .iter()
            .any(|p| p.zones.iter().any(|z| !z.voice_zones.is_empty()))
    {
        return Err(bad("no playable sample zones"));
    }
    Ok(font)
}

/// `fluid_inst_import_sfont`.
fn import_inst(sfzones: &[SfZone], sample_index: &[Option<usize>]) -> Result<Inst> {
    let mut inst = Inst {
        global: None,
        zones: Vec::new(),
    };
    for (count, sfzone) in sfzones.iter().enumerate() {
        let global_range = inst.global.as_ref().map(|z| z.range);
        let (range, mut gens) = import_gens(sfzone, global_range);
        let mut sample = None;
        if gens[generator::SAMPLEID].0 {
            let idx = gens[generator::SAMPLEID].1 as usize;
            match sample_index.get(idx) {
                Some(s) => sample = *s,
                None => return Err(bad("an instrument zone references a missing sample")),
            }
            gens[generator::SAMPLEID].0 = false;
        }
        let zone = InstZone {
            range,
            gens,
            mods: check_mods(import_mods(&sfzone.mods)),
            sample,
        };
        if count == 0 && zone.sample.is_none() {
            inst.global = Some(zone);
        } else {
            // fluid_inst_add_zone prepends.
            inst.zones.push(zone);
        }
    }
    inst.zones.reverse();
    Ok(inst)
}

/// `fluid_preset_zone_create_voice_zones`.
fn voice_zones(prange: &Range, inst: &Inst, samples: &[Sample]) -> Vec<VoiceZone> {
    inst.zones
        .iter()
        .enumerate()
        .filter(|(_, z)| {
            z.sample
                .is_some_and(|s| samples[s].sampletype & SAMPLETYPE_ROM == 0)
        })
        .map(|(i, z)| {
            let irange = &z.range;
            VoiceZone {
                inst_zone: i,
                range: Range {
                    keylo: prange.keylo.max(irange.keylo),
                    keyhi: prange.keyhi.min(irange.keyhi),
                    vello: prange.vello.max(irange.vello),
                    velhi: prange.velhi.min(irange.velhi),
                },
            }
        })
        .collect()
}
