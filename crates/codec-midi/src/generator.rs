//! SoundFont generators: FluidSynth 2.6.1 `synth/fluid_gen.c`.

pub const STARTADDROFS: usize = 0;
pub const ENDADDROFS: usize = 1;
pub const STARTLOOPADDROFS: usize = 2;
pub const ENDLOOPADDROFS: usize = 3;
pub const STARTADDRCOARSEOFS: usize = 4;
pub const MODLFOTOPITCH: usize = 5;
pub const VIBLFOTOPITCH: usize = 6;
pub const MODENVTOPITCH: usize = 7;
pub const FILTERFC: usize = 8;
pub const FILTERQ: usize = 9;
pub const MODLFOTOFILTERFC: usize = 10;
pub const MODENVTOFILTERFC: usize = 11;
pub const ENDADDRCOARSEOFS: usize = 12;
pub const MODLFOTOVOL: usize = 13;
pub const UNUSED1: usize = 14;
pub const CHORUSSEND: usize = 15;
pub const REVERBSEND: usize = 16;
pub const PAN: usize = 17;
pub const UNUSED2: usize = 18;
pub const UNUSED3: usize = 19;
pub const UNUSED4: usize = 20;
pub const MODLFODELAY: usize = 21;
pub const MODLFOFREQ: usize = 22;
pub const VIBLFODELAY: usize = 23;
pub const VIBLFOFREQ: usize = 24;
pub const MODENVDELAY: usize = 25;
pub const MODENVATTACK: usize = 26;
pub const MODENVHOLD: usize = 27;
pub const MODENVDECAY: usize = 28;
pub const MODENVSUSTAIN: usize = 29;
pub const MODENVRELEASE: usize = 30;
pub const KEYTOMODENVHOLD: usize = 31;
pub const KEYTOMODENVDECAY: usize = 32;
pub const VOLENVDELAY: usize = 33;
pub const VOLENVATTACK: usize = 34;
pub const VOLENVHOLD: usize = 35;
pub const VOLENVDECAY: usize = 36;
pub const VOLENVSUSTAIN: usize = 37;
pub const VOLENVRELEASE: usize = 38;
pub const KEYTOVOLENVHOLD: usize = 39;
pub const KEYTOVOLENVDECAY: usize = 40;
pub const INSTRUMENT: usize = 41;
pub const RESERVED1: usize = 42;
pub const KEYRANGE: usize = 43;
pub const VELRANGE: usize = 44;
pub const STARTLOOPADDRCOARSEOFS: usize = 45;
pub const KEYNUM: usize = 46;
pub const VELOCITY: usize = 47;
pub const ATTENUATION: usize = 48;
pub const RESERVED2: usize = 49;
pub const ENDLOOPADDRCOARSEOFS: usize = 50;
pub const COARSETUNE: usize = 51;
pub const FINETUNE: usize = 52;
pub const SAMPLEID: usize = 53;
pub const SAMPLEMODE: usize = 54;
pub const RESERVED3: usize = 55;
pub const SCALETUNE: usize = 56;
pub const EXCLUSIVECLASS: usize = 57;
pub const OVERRIDEROOTKEY: usize = 58;
pub const PITCH: usize = 59;
pub const CUSTOM_BALANCE: usize = 60;
pub const LAST: usize = 63;

/// `fluid_gen_info[]`: (NRPN scale, default). The table's float defaults,
/// as FluidSynth stores them.
const INFO: [(i32, f32); LAST] = [
    (1, 0.0),      // STARTADDROFS
    (1, 0.0),      // ENDADDROFS
    (1, 0.0),      // STARTLOOPADDROFS
    (1, 0.0),      // ENDLOOPADDROFS
    (1, 0.0),      // STARTADDRCOARSEOFS
    (2, 0.0),      // MODLFOTOPITCH
    (2, 0.0),      // VIBLFOTOPITCH
    (2, 0.0),      // MODENVTOPITCH
    (2, 13500.0),  // FILTERFC
    (1, 0.0),      // FILTERQ
    (2, 0.0),      // MODLFOTOFILTERFC
    (2, 0.0),      // MODENVTOFILTERFC
    (1, 0.0),      // ENDADDRCOARSEOFS
    (1, 0.0),      // MODLFOTOVOL
    (0, 0.0),      // UNUSED1
    (1, 0.0),      // CHORUSSEND
    (1, 0.0),      // REVERBSEND
    (1, 0.0),      // PAN
    (0, 0.0),      // UNUSED2
    (0, 0.0),      // UNUSED3
    (0, 0.0),      // UNUSED4
    (2, -12000.0), // MODLFODELAY
    (4, 0.0),      // MODLFOFREQ
    (2, -12000.0), // VIBLFODELAY
    (4, 0.0),      // VIBLFOFREQ
    (2, -12000.0), // MODENVDELAY
    (2, -12000.0), // MODENVATTACK
    (2, -12000.0), // MODENVHOLD
    (2, -12000.0), // MODENVDECAY
    (1, 0.0),      // MODENVSUSTAIN
    (2, -12000.0), // MODENVRELEASE
    (1, 0.0),      // KEYTOMODENVHOLD
    (1, 0.0),      // KEYTOMODENVDECAY
    (2, -12000.0), // VOLENVDELAY
    (2, -12000.0), // VOLENVATTACK
    (2, -12000.0), // VOLENVHOLD
    (2, -12000.0), // VOLENVDECAY
    (1, 0.0),      // VOLENVSUSTAIN
    (2, -12000.0), // VOLENVRELEASE
    (1, 0.0),      // KEYTOVOLENVHOLD
    (1, 0.0),      // KEYTOVOLENVDECAY
    (0, 0.0),      // INSTRUMENT
    (0, 0.0),      // RESERVED1
    (0, 0.0),      // KEYRANGE
    (0, 0.0),      // VELRANGE
    (1, 0.0),      // STARTLOOPADDRCOARSEOFS
    (0, -1.0),     // KEYNUM
    (1, -1.0),     // VELOCITY
    (1, 0.0),      // ATTENUATION
    (0, 0.0),      // RESERVED2
    (1, 0.0),      // ENDLOOPADDRCOARSEOFS
    (1, 0.0),      // COARSETUNE
    (1, 0.0),      // FINETUNE
    (0, 0.0),      // SAMPLEID
    (0, 0.0),      // SAMPLEMODE
    (0, 0.0),      // RESERVED3
    (1, 100.0),    // SCALETUNE
    (0, 0.0),      // EXCLUSIVECLASS
    (0, -1.0),     // OVERRIDEROOTKEY
    (0, 0.0),      // PITCH
    (0, 0.0),      // CUSTOM_BALANCE
    (2, 0.0),      // CUSTOM_FILTERFC
    (1, 0.0),      // CUSTOM_FILTERQ
];

/// `fluid_gen_t`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Gen {
    pub set: bool,
    pub val: f64,
    pub modv: f64,
    pub nrpn: f64,
}

/// `fluid_gen_init`: defaults, nothing set, NRPN offsets from the channel.
pub fn init(nrpn: Option<&[f64; LAST]>) -> [Gen; LAST] {
    std::array::from_fn(|i| Gen {
        set: false,
        val: f64::from(INFO[i].1),
        modv: 0.0,
        nrpn: nrpn.map_or(0.0, |n| n[i]),
    })
}

/// `fluid_gen_scale_nrpn`: an NRPN data value to a generator offset.
pub fn scale_nrpn(id: usize, data: i32) -> f64 {
    let data = (data - 8192).clamp(-8192, 8192);
    f64::from(data * INFO[id].0)
}
