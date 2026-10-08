use crate::{rvoice::BUFSIZE, sfont::SoundFont, smf, synth::Synth};
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
    let info = list(b"INFO", &[chunk(b"ifil", &[2, 0, 4, 0])]);
    let sdta = list(b"sdta", &[chunk(b"smpl", &pcm)]);
    let pdta = list(
        b"pdta",
        &[
            chunk(b"phdr", &phdr),
            chunk(b"pbag", &[0, 0, 0, 0, 1, 0, 0, 0]),
            chunk(b"pmod", &[0; 10]),
            chunk(b"pgen", &[41, 0, 0, 0, 0, 0, 0, 0]),
            chunk(b"inst", &inst),
            chunk(b"ibag", &[0, 0, 0, 0, 2, 0, 0, 0]),
            chunk(b"imod", &[0; 10]),
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
