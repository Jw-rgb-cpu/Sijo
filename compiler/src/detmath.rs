//! Deterministic float math shared by the compiler's CTFE and the C runtime (runtime/sijo_rt.h).
//!
//! The two implementations perform the same IEEE-754 operations in the same order, so a value
//! computed at compile time is bit-identical to the one the generated program computes on any
//! platform. Keep them in sync: `tests/e2e.rs` compares CTFE and runtime results.

/// bits of 2/pi: word k holds positions 32k-31 .. 32k after the binary point (word 0: zeros)
const TWO_OVER_PI: [u32; 9] = [0x00000000, 0xA2F9836E, 0x4E441529, 0xFC2757D1, 0xF534DDC0, 0xDB629599, 0x3C439041, 0xFE5163AB, 0xDEBBC561];

fn two_over_pi_bits(q: i32) -> u64 {
    let t = q + 31;
    let (k, s) = ((t >> 5) as usize, (t & 31) as u32);
    let v = if s == 0 { TWO_OVER_PI[k] } else { (TWO_OVER_PI[k] << s) | (TWO_OVER_PI[k + 1] >> (32 - s)) };
    v as u64
}

fn sin_poly(y: f64) -> f64 {
    let z = y * y;
    let p = f64::from_bits(0xBE5AE64567F544E4); // -1/11!
    let p = f64::from_bits(0x3EC71DE3A556C734) + z * p; // 1/9!
    let p = f64::from_bits(0xBF2A01A01A01A01A) + z * p; // -1/7!
    let p = f64::from_bits(0x3F81111111111111) + z * p; // 1/5!
    let p = f64::from_bits(0xBFC5555555555555) + z * p; // -1/3!
    y + y * z * p
}

fn cos_poly(y: f64) -> f64 {
    let z = y * y;
    let p = f64::from_bits(0x3E21EED8EFF8D898); // 1/12!
    let p = f64::from_bits(0xBE927E4FB7789F5C) + z * p; // -1/10!
    let p = f64::from_bits(0x3EFA01A01A01A01A) + z * p; // 1/8!
    let p = f64::from_bits(0xBF56C16C16C16C17) + z * p; // -1/6!
    let p = f64::from_bits(0x3FA5555555555555) + z * p; // 1/4!
    let p = -0.5 + z * p;
    1.0 + z * p
}

/// |x| (finite, above pi/4, as bits) -> remainder in [-pi/4, pi/4) and quadrant (exact reduction)
fn reduce(ux: u32) -> (f64, u32) {
    let m = ((ux & 0x7fffff) | 0x800000) as u64;
    let e = (ux >> 23) as i32 - 150;
    let p = e - 1;
    let (w0, w1, w2) = (two_over_pi_bits(p), two_over_pi_bits(p + 32), two_over_pi_bits(p + 64));
    let a = m * w2;
    let b = m * w1 + (a >> 32);
    let t0 = a as u32;
    let t1 = b as u32;
    let t2 = (m * w0 + (b >> 32)) as u32;
    let r = ((t2 as u64) << 32) | t1 as u64;
    let n = r.wrapping_add(1u64 << 61) >> 62;
    let f = r.wrapping_sub(n << 62) as i64;
    let y = (f as f64 + t0 as f64 * f64::from_bits(0x3DF0000000000000)) * f64::from_bits(0x3C1921FB54442D18);
    (y, (n & 3) as u32)
}

pub fn sinf(x: f32) -> f32 {
    let u = x.to_bits();
    let ux = u & 0x7fffffff;
    if ux >= 0x7f800000 {
        return x - x;
    }
    if ux <= 0x3f490fdb {
        if ux == 0 {
            return x;
        }
        return sin_poly(x as f64) as f32;
    }
    let (y, n) = reduce(ux);
    let r = match n {
        0 => sin_poly(y),
        1 => cos_poly(y),
        2 => -sin_poly(y),
        _ => -cos_poly(y),
    };
    (if u >> 31 != 0 { -r } else { r }) as f32
}

pub fn cosf(x: f32) -> f32 {
    let ux = x.to_bits() & 0x7fffffff;
    if ux >= 0x7f800000 {
        return x - x;
    }
    if ux <= 0x3f490fdb {
        return cos_poly(x as f64) as f32;
    }
    let (y, n) = reduce(ux);
    (match n {
        0 => cos_poly(y),
        1 => -sin_poly(y),
        2 => -cos_poly(y),
        _ => sin_poly(y),
    }) as f32
}

pub fn tanf(x: f32) -> f32 {
    let u = x.to_bits();
    let ux = u & 0x7fffffff;
    if ux >= 0x7f800000 {
        return x - x;
    }
    if ux <= 0x3f490fdb {
        if ux == 0 {
            return x;
        }
        let y = x as f64;
        return (sin_poly(y) / cos_poly(y)) as f32;
    }
    let (y, n) = reduce(ux);
    let r = if n & 1 != 0 { -cos_poly(y) / sin_poly(y) } else { sin_poly(y) / cos_poly(y) };
    (if u >> 31 != 0 { -r } else { r }) as f32
}

const PIO2: f64 = f64::from_bits(0x3FF921FB54442D18);
const PI: f64 = f64::from_bits(0x400921FB54442D18);
const LN2_HI: f64 = f64::from_bits(0x3FE62E42FEE00000);
const LN2_LO: f64 = f64::from_bits(0x3DEA39EF35793C76);

/// atan of t >= 0 (runtime: sijo_atan_pos): two angle halvings, then a Taylor series
fn atan_pos(t: f64) -> f64 {
    let inv = t > 1.0;
    let mut t = if inv { 1.0 / t } else { t };
    t = t / (1.0 + (1.0 + t * t).sqrt());
    t = t / (1.0 + (1.0 + t * t).sqrt());
    let z = t * t;
    let mut p = 1.0 / 23.0;
    for d in [21.0, 19.0, 17.0, 15.0, 13.0, 11.0, 9.0, 7.0, 5.0, 3.0] {
        p = 1.0 / d - z * p;
    }
    let a = 4.0 * (t - t * z * p);
    if inv {
        PIO2 - a
    } else {
        a
    }
}

pub fn atanf(x: f32) -> f32 {
    if x.is_nan() || x == 0.0 {
        return x;
    }
    let a = atan_pos((x as f64).abs());
    (if x < 0.0 { -a } else { a }) as f32
}

fn atan2_d(y: f64, x: f64) -> f64 {
    if y.is_nan() || x.is_nan() {
        return y + x;
    }
    let (sy, sx) = (y.is_sign_negative(), x.is_sign_negative());
    let (ay, ax) = (y.abs(), x.abs());
    let r = if ay == 0.0 {
        if sx {
            PI
        } else {
            0.0
        }
    } else if ax == 0.0 {
        PI / 2.0
    } else if ay.is_infinite() && ax.is_infinite() {
        if sx {
            3.0 * PI / 4.0
        } else {
            PI / 4.0
        }
    } else if ax.is_infinite() {
        if sx {
            PI
        } else {
            0.0
        }
    } else if ay.is_infinite() {
        PI / 2.0
    } else {
        let r = atan_pos(ay / ax);
        if sx {
            PI - r
        } else {
            r
        }
    };
    if sy {
        -r
    } else {
        r
    }
}

pub fn atan2f(y: f32, x: f32) -> f32 {
    atan2_d(y as f64, x as f64) as f32
}

pub fn asinf(x: f32) -> f32 {
    if x.is_nan() {
        return x;
    }
    let d = x as f64;
    if !(-1.0..=1.0).contains(&d) {
        return f32::NAN;
    }
    if x == 0.0 {
        return x;
    }
    atan2_d(d, ((1.0 - d) * (1.0 + d)).sqrt()) as f32
}

pub fn acosf(x: f32) -> f32 {
    if x.is_nan() {
        return x;
    }
    let d = x as f64;
    if !(-1.0..=1.0).contains(&d) {
        return f32::NAN;
    }
    atan2_d(((1.0 - d) * (1.0 + d)).sqrt(), d) as f32
}

fn ldexp(mut x: f64, mut e: i32) -> f64 {
    while e > 1000 {
        x *= f64::from_bits(((1000 + 1023) as u64) << 52);
        e -= 1000;
    }
    while e < -1000 {
        x *= f64::from_bits(((-1000 + 1023) as u64) << 52);
        e += 1000;
    }
    x * f64::from_bits(((e + 1023) as u64) << 52)
}

fn exp_d(x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    if x > 710.0 {
        return f64::INFINITY;
    }
    if x < -746.0 {
        return 0.0;
    }
    let nf = (x * f64::from_bits(0x3FF71547652B82FE) + 0.5).floor();
    let n = nf as i32;
    let r = (x - nf * LN2_HI) - nf * LN2_LO;
    let mut p = 1.0 / 87178291200.0;
    for d in [6227020800.0, 479001600.0, 39916800.0, 3628800.0, 362880.0, 40320.0, 5040.0, 720.0, 120.0, 24.0, 6.0] {
        p = 1.0 / d + r * p;
    }
    p = 0.5 + r * p;
    p = 1.0 + r * p;
    p = 1.0 + r * p;
    ldexp(p, n)
}

pub fn expf(x: f32) -> f32 {
    exp_d(x as f64) as f32
}

fn log_d(x: f64) -> f64 {
    let mut x = x;
    let mut u = x.to_bits();
    let mut e = ((u >> 52) & 0x7ff) as i32;
    if e == 0 {
        x *= f64::from_bits(((54 + 1023) as u64) << 52);
        u = x.to_bits();
        e = ((u >> 52) & 0x7ff) as i32 - 54;
    }
    e -= 1023;
    let mut m = f64::from_bits((u & 0x000f_ffff_ffff_ffff) | 0x3ff0_0000_0000_0000);
    if m > f64::from_bits(0x3FF6A09E667F3BCD) {
        m *= 0.5;
        e += 1;
    }
    let s = (m - 1.0) / (m + 1.0);
    let z = s * s;
    let mut p = 1.0 / 25.0;
    for d in [23.0, 21.0, 19.0, 17.0, 15.0, 13.0, 11.0, 9.0, 7.0, 5.0, 3.0] {
        p = 1.0 / d + z * p;
    }
    let lm = 2.0 * (s + s * z * p);
    (e as f64 * LN2_HI + lm) + e as f64 * LN2_LO
}

pub fn logf(x: f32) -> f32 {
    if x.is_nan() {
        return x;
    }
    if x < 0.0 {
        return f32::NAN;
    }
    if x == 0.0 {
        return f32::NEG_INFINITY;
    }
    if x.is_infinite() {
        return x;
    }
    log_d(x as f64) as f32
}

pub fn log10f(x: f32) -> f32 {
    if !(x > 0.0) || x.is_infinite() {
        return logf(x);
    }
    (log_d(x as f64) / f64::from_bits(0x40026BB1BBB55516)) as f32
}

pub fn log2f(x: f32) -> f32 {
    if !(x > 0.0) || x.is_infinite() {
        return logf(x);
    }
    (log_d(x as f64) / f64::from_bits(0x3FE62E42FEFA39EF)) as f32
}

/// IEEE / C# MathF.Pow special cases, else exp(y log |x|) in double (runtime: sijo_powf)
pub fn powf(x: f32, y: f32) -> f32 {
    if y == 0.0 || x == 1.0 {
        return 1.0;
    }
    if x.is_nan() || y.is_nan() {
        return x + y;
    }
    let ay = y.abs();
    let yint = y.floor() == y;
    let yodd = yint && ay < 16777216.0 && ay % 2.0 == 1.0;
    if y.is_infinite() {
        let ax = x.abs();
        if ax == 1.0 {
            return 1.0;
        }
        return if (ax > 1.0) == (y > 0.0) { f32::INFINITY } else { 0.0 };
    }
    if x == 0.0 {
        if y < 0.0 {
            return if yodd && x.is_sign_negative() { f32::NEG_INFINITY } else { f32::INFINITY };
        }
        return if yodd { x } else { 0.0 };
    }
    if x.is_infinite() {
        if x > 0.0 {
            return if y > 0.0 { f32::INFINITY } else { 0.0 };
        }
        if y > 0.0 {
            return if yodd { f32::NEG_INFINITY } else { f32::INFINITY };
        }
        return if yodd { -0.0 } else { 0.0 };
    }
    if x < 0.0 && !yint {
        return f32::NAN;
    }
    let r = exp_d(y as f64 * log_d((x as f64).abs()));
    (if x < 0.0 && yodd { -r } else { r }) as f32
}

/// C#'s Math.Round on floats: halves go to the even neighbour (runtime: sijo_roundf)
pub fn roundf(x: f32) -> f32 {
    let mut f = x.floor();
    let d = x - f;
    if d > 0.5 || (d == 0.5 && f % 2.0 != 0.0) {
        f += 1.0;
    }
    if f == 0.0 {
        0.0f32.copysign(x)
    } else {
        f
    }
}

/// C#'s Math.Min / Math.Max on floats: NaN if either is NaN, -0 < +0 (runtime: sijo_minf/sijo_maxf)
pub fn minf(a: f32, b: f32) -> f32 {
    if a.is_nan() {
        a
    } else if b.is_nan() {
        b
    } else if a == b {
        if a.is_sign_negative() {
            a
        } else {
            b
        }
    } else if a < b {
        a
    } else {
        b
    }
}

pub fn maxf(a: f32, b: f32) -> f32 {
    if a.is_nan() {
        a
    } else if b.is_nan() {
        b
    } else if a == b {
        if a.is_sign_negative() {
            b
        } else {
            a
        }
    } else if a > b {
        a
    } else {
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sin_cos_reference_values() {
        // bit patterns produced by the C runtime (exhaustively checked against double-precision sin)
        assert_eq!(sinf(0.0).to_bits(), 0);
        assert_eq!(sinf(-0.0).to_bits(), 0x8000_0000);
        assert_eq!(sinf(std::f32::consts::PI).to_bits(), 0xb3bbbd2e); // -0x1.777a5cp-24
        assert_eq!(cosf(std::f32::consts::PI), -1.0);
        assert_eq!(sinf(std::f32::consts::FRAC_PI_2), 1.0);
        assert_eq!(cosf(std::f32::consts::FRAC_PI_2).to_bits(), 0xb33bbd2e); // -0x1.777a5cp-25
        assert_eq!(sinf(1e6).to_bits(), 0xbeb33259); // -0x1.6664b2p-2
        assert_eq!(sinf(f32::MAX).to_bits(), 0xbf0599b3); // -0x1.0b3366p-1
        assert!(sinf(f32::INFINITY).is_nan() && cosf(f32::NAN).is_nan());
        for i in -2000..2000 {
            let x = i as f32 * 0.01;
            assert!((sinf(x) as f64 - (x as f64).sin()).abs() < 1e-7, "sin {}", x);
            assert!((cosf(x) as f64 - (x as f64).cos()).abs() < 1e-7, "cos {}", x);
        }
    }

    #[test]
    fn min_max_like_csharp() {
        assert!(minf(1.0, f32::NAN).is_nan() && minf(f32::NAN, 1.0).is_nan());
        assert!(maxf(1.0, f32::NAN).is_nan() && maxf(f32::NAN, 1.0).is_nan());
        assert_eq!(minf(0.0, -0.0).to_bits(), 0x8000_0000);
        assert_eq!(minf(-0.0, 0.0).to_bits(), 0x8000_0000);
        assert_eq!(maxf(-0.0, 0.0).to_bits(), 0);
        assert_eq!(maxf(0.0, -0.0).to_bits(), 0);
        assert_eq!(minf(1.0, 2.0), 1.0);
        assert_eq!(maxf(1.0, 2.0), 2.0);
    }
}
