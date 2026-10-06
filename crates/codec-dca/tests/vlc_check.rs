use codec_dca::bitreader::BitReader;
use codec_dca::vlc::Vlc;

#[test]
fn transition_mode_book_decodes_canonically() {
    // dcahuff.c transition_mode[0]: {0,1},{1,2},{2,3},{3,3}
    let src: [[u8; 2]; 4] = [[0, 1], [1, 2], [2, 3], [3, 3]];
    let vlc = codec_dca::vlc::init(&src, 3, 0, false);
    // bits: '0' -> 0, '10' -> 1, '110' -> 2, '111' -> 3
    let bits: Vec<u8> = vec![0b0101_1011, 0b1000_0000];
    let mut gb = BitReader::new(&bits);
    assert_eq!(vlc.get(&mut gb, 1), 0);
    assert_eq!(vlc.get(&mut gb, 2), 1);
    assert_eq!(vlc.get(&mut gb, 2), 2);
    assert_eq!(vlc.get(&mut gb, 2), 3);
}

#[test]
fn scale_factor_book_decodes_negative_symbols() {
    // scale_factor[0] head: {64,4},{66,4},{60,5},{68,5},{56,6},{72,6}
    // offset -64: symbols 0, 2, -4, 4, -8, 8. Fill the tree canonically.
    let mut src = [[0u8; 2]; 129];
    let head: [[u8; 2]; 6] =
        [[64, 4], [66, 4], [60, 5], [68, 5], [56, 6], [72, 6]];
    for (i, e) in head.iter().enumerate() {
        src[i] = *e;
    }
    // Walk the canonical code pointer and fill the rest with 9-bit codes
    // until the 9-bit table space is exhausted (a complete tree).
    let mut code: u64 = 0;
    for e in head.iter() {
        code += 1u64 << (32 - e[1]);
    }
    let mut idx = 6usize;
    while code < (1u64 << 32) && idx < 129 {
        src[idx] = [9, 9];
        code += 1u64 << (32 - 9);
        idx += 1;
    }
    // Trim the tail: the last entries may overshoot 2^32; that mirrors
    // FFmpeg's overlong-book handling which ignores the excess.
    let vlc = codec_dca::vlc::init(&src, 9, -64, false);

    // Encode the six head symbols canonically.
    let mut bytes = [0u8; 8];
    let mut bitpos = 0usize;
    let mut code: u64 = 0;
    for e in head.iter() {
        for b in 0..e[1] {
            let bit = (code >> (31 - b as u32)) & 1;
            if bit == 1 {
                bytes[bitpos / 8] |= 1 << (7 - (bitpos % 8));
            }
            bitpos += 1;
        }
        code += 1u64 << (32 - e[1]);
    }
    let mut gb = BitReader::new(&bytes);
    assert_eq!(vlc.get(&mut gb, 2), 0);
    assert_eq!(vlc.get(&mut gb, 2), 2);
    assert_eq!(vlc.get(&mut gb, 2), -4);
    assert_eq!(vlc.get(&mut gb, 2), 4);
    assert_eq!(vlc.get(&mut gb, 2), -8);
    assert_eq!(vlc.get(&mut gb, 2), 8);
}
