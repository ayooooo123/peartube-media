use crate::{generator, modulator::Mod, rvoice::BUFSIZE, sfont::SoundFont, smf, synth::Synth};
use std::{io::Cursor, sync::Arc};

fn chunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = id.to_vec();
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    if data.len() % 2 != 0 {
        out.push(0);
    }
    out
}
fn list(id: &[u8; 4], chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut data = id.to_vec();
    for c in chunks {
        data.extend_from_slice(c);
    }
    chunk(b"LIST", &data)
}
fn named_record(size: usize, name: &[u8]) -> Vec<u8> {
    let mut out = vec![0; size];
    out[..name.len()].copy_from_slice(name);
    out
}

// Small, generated test bank: one looping triangle sample, one instrument,
// one preset. No external bank or waveform is embedded in the player.
fn bank() -> Vec<u8> {
    bank_with_modulators(&[], &[], None)
}

fn mod_records(destinations: &[u16]) -> Vec<u8> {
    let mut records = Vec::new();
    for &dest in destinations {
        // Active MIDI CC 74 source, positive attenuation/filter amount,
        // no secondary source and a linear transform.
        for word in [0x80 | 74, dest, 240, 0, 0] {
            records.extend_from_slice(&word.to_le_bytes());
        }
    }
    records.extend_from_slice(&[0; 10]); // terminal record
    records
}

fn bank_with_modulators(preset: &[u16], instrument: &[u16], defaults: Option<&[u16]>) -> Vec<u8> {
    let mut pcm = Vec::new();
    for i in 0..346i16 {
        let value = if i < 300 {
            ((i % 64 - 32).abs() - 16) * 1000
        } else {
            0
        };
        pcm.extend_from_slice(&value.to_le_bytes());
    }
    let mut phdr = named_record(38, b"test");
    let mut terminal = named_record(38, b"EOP");
    terminal[24..26].copy_from_slice(&1u16.to_le_bytes());
    phdr.extend(terminal);
    let mut inst = named_record(22, b"test");
    let mut terminal = named_record(22, b"EOI");
    terminal[20..22].copy_from_slice(&1u16.to_le_bytes());
    inst.extend(terminal);
    let mut shdr = named_record(46, b"test");
    for (offset, value) in [(20, 0u32), (24, 300), (28, 32), (32, 288), (36, 44100)] {
        shdr[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    shdr[40] = 60;
    shdr[44] = 1;
    shdr.extend(named_record(46, b"EOS"));
    let mut info = vec![chunk(b"ifil", &[2, 0, 4, 0])];
    if let Some(defaults) = defaults {
        info.push(chunk(b"DMOD", &mod_records(defaults)));
    }
    let info = list(b"INFO", &info);
    let mut pbag = [0, 0, 0, 0, 1, 0, 0, 0];
    pbag[6..8].copy_from_slice(&u16::try_from(preset.len()).unwrap().to_le_bytes());
    let mut ibag = [0, 0, 0, 0, 2, 0, 0, 0];
    ibag[6..8].copy_from_slice(&u16::try_from(instrument.len()).unwrap().to_le_bytes());
    let sdta = list(b"sdta", &[chunk(b"smpl", &pcm)]);
    let pdta = list(
        b"pdta",
        &[
            chunk(b"phdr", &phdr),
            chunk(b"pbag", &pbag),
            chunk(b"pmod", &mod_records(preset)),
            chunk(b"pgen", &[41, 0, 0, 0, 0, 0, 0, 0]),
            chunk(b"inst", &inst),
            chunk(b"ibag", &ibag),
            chunk(b"imod", &mod_records(instrument)),
            chunk(b"igen", &[54, 0, 1, 0, 53, 0, 0, 0, 0, 0, 0, 0]),
            chunk(b"shdr", &shdr),
        ],
    );
    let mut body = b"sfbk".to_vec();
    for part in [info, sdta, pdta] {
        body.extend(part);
    }
    chunk(b"RIFF", &body)
}
fn random(seed: &mut u32) -> usize {
    *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
    *seed as usize
}
fn mutate(data: &mut Vec<u8>, index: usize, seed: &mut u32) {
    if index % 4 == 0 {
        data.truncate(random(seed) % data.len());
    } else {
        for _ in 0..1 + index % 4 {
            let at = random(seed) % data.len();
            data[at] ^= 1 << (random(seed) % 8);
        }
    }
}
fn render_blocks(synth: &mut Synth) -> f64 {
    let mut energy = 0.0;
    let mut output = [0.0; BUFSIZE * 2];
    for _ in 0..16 {
        synth.begin_block();
        synth.render_block(&mut output);
        for value in output {
            assert!(value.is_finite(), "non-finite synthesis output");
            energy += f64::from(value).powi(2);
        }
    }
    energy
}

#[test]
fn damaged_soundfonts_are_bounded_and_never_panic() {
    let original = bank();
    let valid = Arc::new(SoundFont::load(&mut Cursor::new(&original)).unwrap());
    let note = smf::Kind::NoteOn {
        chan: 0,
        key: 60,
        vel: 100,
    };
    let mut synth = Synth::new(valid, 44100.0);
    synth.event(&note);
    assert!(
        render_blocks(&mut synth) > 1e-4,
        "fixture must reach audible synthesis"
    );
    let mut seed = 0x5346_3201;
    for i in 0..2000 {
        let mut input = original.clone();
        mutate(&mut input, i, &mut seed);
        if let Ok(font) = SoundFont::load(&mut Cursor::new(input)) {
            let mut synth = Synth::new(Arc::new(font), 44100.0);
            synth.event(&note);
            render_blocks(&mut synth);
        }
    }
}

#[test]
fn damaged_midi_events_are_bounded_and_never_panic() {
    let font = Arc::new(SoundFont::load(&mut Cursor::new(bank())).unwrap());
    let events = [
        0, 0xc0, 0, 0, 0xb0, 64, 127, 0, 0x90, 60, 100, 1, 0xb0, 1, 100, 1, 0xe0, 0, 70, 1, 0xa0,
        60, 80, 1, 0x80, 60, 0, 1, 0xb0, 64, 0, 1, 0xff, 0x2f, 0,
    ];
    let mut original = b"MThd\0\0\0\x06\0\0\0\x01\x01\xe0MTrk".to_vec();
    original.extend_from_slice(&(events.len() as u32).to_be_bytes());
    original.extend_from_slice(&events);
    let mut seed = 0x4d49_4449;
    for i in 0..2000 {
        let mut input = original.clone();
        mutate(&mut input, i, &mut seed);
        if let Ok(song) = smf::parse(&input) {
            if smf::duration_us(&song) > 86_400_000_000 {
                continue;
            }
            let mut synth = Synth::new(font.clone(), 44100.0);
            let mut player = crate::player::Player::new(song);
            let mut output = [0.0; BUFSIZE * 2];
            for _ in 0..32 {
                synth.begin_block();
                if player.callback(&mut synth).is_err() {
                    break;
                }
                synth.render_block(&mut output);
                assert!(
                    output.iter().all(|v| v.is_finite()),
                    "mutation {i} emitted non-finite PCM"
                );
                if synth.failed || player.done {
                    break;
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum ModLocation {
    Preset,
    Instrument,
    Default,
}

impl ModLocation {
    fn load(self, destinations: &[u16]) -> SoundFont {
        let data = match self {
            Self::Preset => bank_with_modulators(destinations, &[], None),
            Self::Instrument => bank_with_modulators(&[], destinations, None),
            Self::Default => bank_with_modulators(&[], &[], Some(destinations)),
        };
        SoundFont::load(&mut Cursor::new(data)).unwrap()
    }

    fn records(self, font: &SoundFont) -> &[Mod] {
        match self {
            Self::Preset => &font.presets[0].zones[0].mods,
            Self::Instrument => &font.insts[0].zones[0].mods,
            Self::Default => font.default_mods.as_deref().unwrap(),
        }
    }
}

fn modulated_pcm(font: SoundFont) -> Vec<f32> {
    let mut synth = Synth::new(Arc::new(font), 44100.0);
    synth.event(&smf::Kind::Control {
        chan: 0,
        num: 74,
        value: 0,
    });
    synth.event(&smf::Kind::NoteOn {
        chan: 0,
        key: 60,
        vel: 100,
    });
    let mut pcm = Vec::new();
    for block in 0..32 {
        synth.begin_block();
        if block == 8 {
            synth.event(&smf::Kind::Control {
                chan: 0,
                num: 74,
                value: 127,
            });
        }
        if block == 24 {
            synth.event(&smf::Kind::NoteOff {
                chan: 0,
                key: 60,
                vel: 0,
            });
        }
        let mut output = [0.0; BUFSIZE * 2];
        synth.render_block(&mut output);
        assert!(!synth.failed);
        assert!(output.iter().all(|v| v.is_finite()));
        pcm.extend(output);
    }
    pcm
}

fn check_modulator_destinations(location: ModLocation) {
    let valid = [
        generator::ATTENUATION as u16,
        (generator::LAST - 1) as u16,
        generator::FILTERFC as u16,
    ];
    let invalid = [
        generator::LAST as u16,
        64,
        255,
        256,
        256 + valid[0],
        u16::MAX,
    ];
    // The wide value with ATTENUATION's low byte follows the real record:
    // rejecting it must not replace or suppress that earlier valid record.
    let mut mixed = valid[..2].to_vec();
    mixed.extend(invalid);
    mixed.push(valid[2]);
    let font = location.load(&mixed);
    let mut expected = valid.to_vec();
    if matches!(location, ModLocation::Default) {
        expected.reverse(); // FluidSynth prepends DMOD records.
    }
    let admitted: Vec<_> = location
        .records(&font)
        .iter()
        .map(|m| u16::from(m.dest))
        .collect();
    // Fail at admission, before giving any unchecked record to a voice.
    assert_eq!(
        admitted, expected,
        "{location:?}: full-width destination admission"
    );
    let filtered_pcm = modulated_pcm(font);
    let valid_pcm = modulated_pcm(location.load(&valid));
    assert_eq!(
        filtered_pcm, valid_pcm,
        "{location:?}: ignored records changed valid modulation"
    );
    assert_ne!(
        valid_pcm,
        modulated_pcm(bank_without_modulators(location)),
        "{location:?}: fixture did not exercise active modulation"
    );
    let rejected = location.load(&invalid);
    assert!(
        location.records(&rejected).is_empty(),
        "{location:?}: invalid-only records survived"
    );
}

fn bank_without_modulators(location: ModLocation) -> SoundFont {
    match location {
        ModLocation::Default => {
            let mut font = location.load(&[generator::ATTENUATION as u16]);
            font.default_mods.as_mut().unwrap().clear();
            font
        }
        _ => location.load(&[]),
    }
}

#[test]
fn preset_modulator_destinations_are_checked_before_narrowing() {
    check_modulator_destinations(ModLocation::Preset);
}

#[test]
fn instrument_modulator_destinations_are_checked_before_narrowing() {
    check_modulator_destinations(ModLocation::Instrument);
}

#[test]
fn default_modulator_destinations_are_checked_before_narrowing() {
    check_modulator_destinations(ModLocation::Default);
}
