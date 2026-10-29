// Numeric helpers that reproduce Nix's observable behaviour: the three ways
// it prints a float, and checked 64-bit integer arithmetic.
//
// Nix prints floats with three different C++ facilities:
//   toString / string interpolation   std::to_string(double)   == printf "%f"
//   the REPL, nix-instantiate         operator<< on ostream    == printf "%g"
//   toJSON                            nlohmann::json's dump    shortest round-trip
// Rust's formatter rounds the exact binary value, half to even, as glibc
// does, so `{:.6}` and `{:.5e}` give printf's digits.

use crate::value::{NixError, Result};

fn non_finite(x: f64) -> String {
    if x.is_nan() {
        "nan".into()
    } else if x < 0.0 {
        "-inf".into()
    } else {
        "inf".into()
    }
}

// std::to_string(double), which is printf("%f"): six fixed decimals.
pub fn format_float_fixed(x: f64) -> String {
    if !x.is_finite() {
        return non_finite(x);
    }
    format!("{x:.6}")
}

// Remove trailing zeros of the fraction, and the point itself if nothing
// remains, as the %g conversion does.
fn strip_trailing_zeros(s: &str) -> &str {
    if !s.contains('.') {
        return s;
    }
    s.trim_end_matches('0').trim_end_matches('.')
}

// The mantissa and decimal exponent of Rust's `{:e}`-style output.
fn split_exp(s: &str) -> (&str, i32) {
    let (m, e) = s.split_once('e').expect("exponent form");
    (m, e.parse().expect("exponent"))
}

fn format_exponent(e: i32) -> String {
    let sign = if e < 0 { '-' } else { '+' };
    format!("{sign}{:02}", e.unsigned_abs())
}

// printf("%g"): six significant digits, trailing zeros dropped, scientific
// notation when the decimal exponent is below -4 or at least 6.
pub fn format_float_g(x: f64) -> String {
    if !x.is_finite() {
        return non_finite(x);
    }
    if x == 0.0 {
        return if x.is_sign_negative() {
            "-0".into()
        } else {
            "0".into()
        };
    }
    let sci = format!("{x:.5e}");
    let (mantissa, exp) = split_exp(&sci);
    if (-4..6).contains(&exp) {
        let decimals = (5 - exp) as usize;
        let fixed = format!("{x:.decimals$}");
        return strip_trailing_zeros(&fixed).to_string();
    }
    format!(
        "{}e{}",
        strip_trailing_zeros(mantissa),
        format_exponent(exp)
    )
}

// nlohmann::json's dump of a double: the shortest digits that round-trip,
// laid out like printf("%.17g") but with ".0" appended to integral values.
pub fn format_float_json(x: f64) -> String {
    if !x.is_finite() {
        return "null".into();
    }
    let sign = if x.is_sign_negative() { "-" } else { "" };
    if x == 0.0 {
        return format!("{sign}0.0");
    }
    // `{:e}` with no precision yields the shortest round-trip digits.
    let sci = format!("{:e}", x.abs());
    let (mantissa, exp) = split_exp(&sci);
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    // n is the position of the decimal point relative to the start of digits.
    let n = exp + 1;
    if k <= n && n <= 15 {
        return format!("{sign}{digits}{}.0", "0".repeat((n - k) as usize));
    }
    if 0 < n && n <= 15 {
        let (a, b) = digits.split_at(n as usize);
        return format!("{sign}{a}.{b}");
    }
    if -4 < n && n <= 0 {
        return format!("{sign}0.{}{digits}", "0".repeat((-n) as usize));
    }
    let m = if k == 1 {
        digits
    } else {
        format!("{}.{}", &digits[..1], &digits[1..])
    };
    format!("{sign}{m}e{}", format_exponent(n - 1))
}

pub fn add_int(a: i64, b: i64) -> Result<i64> {
    a.checked_add(b)
        .ok_or_else(|| NixError::boxed(format!("integer overflow in adding {a} + {b}")))
}

pub fn sub_int(a: i64, b: i64) -> Result<i64> {
    a.checked_sub(b)
        .ok_or_else(|| NixError::boxed(format!("integer overflow in subtracting {a} - {b}")))
}

pub fn mul_int(a: i64, b: i64) -> Result<i64> {
    a.checked_mul(b)
        .ok_or_else(|| NixError::boxed(format!("integer overflow in multiplying {a} * {b}")))
}

// Integer division truncates toward zero.
pub fn div_int(a: i64, b: i64) -> Result<i64> {
    if b == 0 {
        return Err(NixError::boxed("division by zero"));
    }
    a.checked_div(b)
        .ok_or_else(|| NixError::boxed(format!("integer overflow in dividing {a} / {b}")))
}

// Convert an already-integral float to an int64, rejecting NaN and anything
// outside the range. 2^63 is a power of two, so the bound compares exactly.
fn integral_float_to_int(rounded: f64, original: f64) -> Result<i64> {
    const LIMIT: f64 = 9223372036854775808.0;
    if !(-LIMIT..LIMIT).contains(&rounded) {
        return Err(NixError::boxed(format!(
            "Cannot convert {} to an integer",
            format_float_g(original)
        )));
    }
    Ok(rounded as i64)
}

pub fn float_to_int_floor(x: f64) -> Result<i64> {
    integral_float_to_int(x.floor(), x)
}

pub fn float_to_int_ceil(x: f64) -> Result<i64> {
    integral_float_to_int(x.ceil(), x)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g_format() {
        assert_eq!(format_float_g(2.5), "2.5");
        assert_eq!(format_float_g(1e10), "1e+10");
        assert_eq!(format_float_g(0.5), "0.5");
        assert_eq!(format_float_g(5.0), "5");
        assert_eq!(format_float_g(123456.0), "123456");
        assert_eq!(format_float_g(1234567.0), "1.23457e+06");
        assert_eq!(format_float_g(0.0001), "0.0001");
        assert_eq!(format_float_g(0.00001), "1e-05");
        assert_eq!(format_float_g(999999.5), "1e+06");
    }

    #[test]
    fn fixed_format() {
        assert_eq!(format_float_fixed(1.5), "1.500000");
        assert_eq!(format_float_fixed(-0.1), "-0.100000");
    }

    #[test]
    fn json_format() {
        assert_eq!(format_float_json(2.5), "2.5");
        assert_eq!(format_float_json(2.0), "2.0");
        assert_eq!(format_float_json(1e20), "1e+20");
        assert_eq!(format_float_json(0.001), "0.001");
        assert_eq!(format_float_json(1e-5), "1e-05");
        assert_eq!(format_float_json(0.1), "0.1");
    }
}
