//! Bit-exactness against FFmpeg's simple_idct (8-bit / int16 instantiation),
//! checked over 200 pseudorandom coefficient blocks (seed 42, range ±2048).
//! `blocks_expected.txt` holds the outputs of the unmodified C template
//! (`libavcodec/simple_idct_template.c` 8-bit path) compiled standalone.

use codec_mpeg4_idct::simple_idct_put_8bit;

#[test]
fn matches_c_reference_on_random_blocks() {
    let coeffs: Vec<Vec<i16>> = gen_blocks();
    let expected: Vec<Vec<i32>> = expected_blocks();
    assert_eq!(coeffs.len(), expected.len(), "block count");
    for (n, (coef, exp)) in coeffs.iter().zip(expected.iter()).enumerate() {
        assert_eq!(coef.len(), 64, "block {n} size");
        assert_eq!(exp.len(), 64, "block {n} expected size");
        let mut block = [0i16; 64];
        block.copy_from_slice(coef);
        let out = simple_idct_put_8bit(&mut block);
        // The expected file lists, per output line, one *column* of 8 rows:
        // value index = col*8 + row.
        for c in 0..8 {
            for r in 0..8 {
                assert_eq!(
                    out[c][r] as i32, exp[c * 8 + r],
                    "block {n} pixel (col {c}, row {r})"
                );
            }
        }
    }
}

fn parse_blocks(raw: &str) -> Vec<Vec<i32>> {
    raw.trim_matches(|c| c == '[' || c == ']')
        .split("], [")
        .map(|row| row.split(',').map(|v| v.trim().parse().unwrap()).collect())
        .collect()
}

fn gen_blocks() -> Vec<Vec<i16>> {
    parse_blocks(include_str!("blocks_coeff.txt"))
        .into_iter()
        .map(|b| b.into_iter().map(|v| v as i16).collect())
        .collect()
}

fn expected_blocks() -> Vec<Vec<i32>> {
    parse_blocks(include_str!("blocks_expected.txt"))
}
