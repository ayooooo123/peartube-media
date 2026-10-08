//! Unit conversions: FluidSynth 2.6.1 `utils/fluid_conv.c` and the tables
//! `gentables/*.cpp` builds. FluidSynth computes the tables with gcem's
//! constexpr math; libm agrees with it to the last bit or two, far below
//! anything audible or measurable at the output.

use std::sync::LazyLock;

pub const PEAK_ATTENUATION: f64 = 960.0;
pub const VEL_CB_SIZE: usize = 128;
const CENTS_HZ_SIZE: usize = 1200;
const CB_AMP_SIZE: usize = 1441;
const PAN_SIZE: usize = 1002;
/// Rows of the 4th-order interpolation table (`FLUID_INTERP_MAX`).
pub const INTERP_MAX: usize = 256;

struct Tables {
    ct2hz: [f64; CENTS_HZ_SIZE],
    cb2amp: [f64; CB_AMP_SIZE],
    concave: [f64; VEL_CB_SIZE],
    convex: [f64; VEL_CB_SIZE],
    pan: [f64; PAN_SIZE],
    interp: [[f64; 4]; INTERP_MAX],
}

static TABLES: LazyLock<Tables> = LazyLock::new(|| {
    let mut t = Tables {
        ct2hz: [0.0; CENTS_HZ_SIZE],
        cb2amp: [0.0; CB_AMP_SIZE],
        concave: [0.0; VEL_CB_SIZE],
        convex: [0.0; VEL_CB_SIZE],
        pan: [0.0; PAN_SIZE],
        interp: [[0.0; 4]; INTERP_MAX],
    };
    for (i, v) in t.ct2hz.iter_mut().enumerate() {
        *v = 6.875 * 2.0f64.powf(i as f64 / 1200.0);
    }
    for (i, v) in t.cb2amp.iter_mut().enumerate() {
        *v = 10.0f64.powf(i as f64 / -200.0);
    }
    let last = VEL_CB_SIZE - 1;
    let scale = -200.0 * 2.0 / PEAK_ATTENUATION;
    for i in 0..VEL_CB_SIZE {
        t.concave[i] = match i {
            0 => 0.0,
            _ if i == last => 1.0,
            _ => scale * ((last - i) as f64 / last as f64).ln() / std::f64::consts::LN_10,
        };
        t.convex[i] = match i {
            0 => 0.0,
            _ if i == last => 1.0,
            _ => 1.0 - (scale * (i as f64 / last as f64).ln() / std::f64::consts::LN_10),
        };
    }
    for (i, v) in t.pan.iter_mut().enumerate() {
        *v = (i as f64 * (std::f64::consts::FRAC_PI_2 / (PAN_SIZE as f64 - 1.0))).sin();
    }
    for (i, row) in t.interp.iter_mut().enumerate() {
        let x = i as f64 / INTERP_MAX as f64;
        row[0] = x * (-0.5 + x * (1.0 - 0.5 * x));
        row[1] = 1.0 + x * x * (1.5 * x - 2.5);
        row[2] = x * (0.5 + x * (2.0 - 1.5 * x));
        row[3] = 0.5 * x * x * (x - 1.0);
    }
    t
});

/// The 4th-order interpolation coefficients for table row `row`.
pub fn interp_coeff(row: usize) -> &'static [f64; 4] {
    &TABLES.interp[row]
}

/// `fluid_ct2hz_real`: absolute cents to Hz. Only the whole cents count.
pub fn ct2hz_real(cents: f64) -> f64 {
    let icents = (cents as i32).wrapping_add(300);
    let mut fac = icents / 1200;
    let mut rem = if icents < 0 {
        -((icents.unsigned_abs() % 1200) as i32)
    } else {
        (icents as u32 % 1200) as i32
    };
    if rem < 0 {
        rem += 1200;
        fac -= 1;
    }
    let tab = TABLES.ct2hz[rem as usize];
    if fac >= 0 {
        f64::from(1u32 << (fac & 31)) * tab
    } else {
        tab / f64::from(1u32 << ((-fac) & 31))
    }
}

/// `fluid_ct2hz`: cents clamped to 1500..13500 (20 Hz..20 kHz).
pub fn ct2hz(cents: f64) -> f64 {
    let cents = if cents >= 13500.0 {
        13500.0
    } else if cents < 1500.0 {
        1500.0
    } else {
        cents
    };
    ct2hz_real(cents)
}

/// `fluid_cb2amp`: centibels of attenuation to linear amplitude.
pub fn cb2amp(cb: f64) -> f64 {
    if cb < 0.0 {
        return 10.0f64.powf(cb / -200.0);
    }
    if cb >= CB_AMP_SIZE as f64 {
        return 0.0;
    }
    TABLES.cb2amp[cb as usize]
}

/// `fluid_tc2sec`: timecents to seconds.
pub fn tc2sec(tc: f64) -> f64 {
    2.0f64.powf(tc / 1200.0)
}

/// `fluid_tc2sec_delay`.
pub fn tc2sec_delay(tc: f64) -> f64 {
    if tc <= -32768.0 {
        return 0.0;
    }
    tc2sec(tc.clamp(-12000.0, 5000.0))
}

/// `fluid_tc2sec_attack` (and `fluid_tc2sec_release`, the same function).
pub fn tc2sec_attack(tc: f64) -> f64 {
    if tc <= -32768.0 {
        return 0.0;
    }
    tc2sec(tc.clamp(-12000.0, 8000.0))
}

/// `fluid_hz2ct`.
pub fn hz2ct(f: f64) -> f64 {
    6900.0 + (1200.0 / std::f64::consts::LN_2) * (f / 440.0).ln()
}

/// `fluid_pan`: the gain of one side for a pan of `c` (-500..500).
pub fn pan(c: f64, left: bool) -> f64 {
    let c = if left { -c } else { c };
    if c <= -500.0 {
        0.0
    } else if c >= 500.0 {
        1.0
    } else {
        TABLES.pan[(c as i32 + 500) as usize]
    }
}

/// `fluid_balance`: the gain of one side for a balance of `balance` cB.
pub fn balance(balance: f64, left: bool) -> f64 {
    if balance == 0.0 {
        return 1.0;
    }
    if (left && balance < 0.0) || (!left && balance > 0.0) {
        return 1.0;
    }
    cb2amp(balance.abs())
}

fn curve(tab: &[f64; VEL_CB_SIZE], val: f64) -> f64 {
    let ival = val as i32;
    if val < 0.0 {
        return 0.0;
    }
    if ival >= VEL_CB_SIZE as i32 - 1 {
        return tab[VEL_CB_SIZE - 1];
    }
    let i = ival as usize;
    tab[i] + (tab[i + 1] - tab[i]) * (val - f64::from(ival))
}

/// `fluid_concave`, interpolated between table steps.
pub fn concave(val: f64) -> f64 {
    curve(&TABLES.concave, val)
}

/// `fluid_convex`, interpolated between table steps.
pub fn convex(val: f64) -> f64 {
    curve(&TABLES.convex, val)
}
