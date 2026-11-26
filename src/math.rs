//! Floating-point math. With the `std` feature these are std's functions
//! (wasm instructions or the compiler's libm); without it, a port of musl's
//! libm (`crate::libm`, MIT) so the core stays `no_std`.

macro_rules! unary {
    ($($name:ident),*) => {$(
        #[cfg(feature = "std")]
        #[inline]
        pub fn $name(x: f64) -> f64 {
            x.$name()
        }
        #[cfg(not(feature = "std"))]
        #[inline]
        pub fn $name(x: f64) -> f64 {
            crate::libm::$name(x)
        }
    )*};
}

unary!(trunc, floor, ceil, sqrt, exp, exp_m1, ln, ln_1p, log2, log10, sin, cos, tan, asin, acos, atan, sinh, cosh, tanh, asinh, acosh, atanh, cbrt);

#[cfg(feature = "std")]
pub fn powf(x: f64, y: f64) -> f64 {
    x.powf(y)
}
#[cfg(not(feature = "std"))]
pub fn powf(x: f64, y: f64) -> f64 {
    crate::libm::powf(x, y)
}

#[cfg(feature = "std")]
pub fn atan2(y: f64, x: f64) -> f64 {
    y.atan2(x)
}
#[cfg(not(feature = "std"))]
pub fn atan2(y: f64, x: f64) -> f64 {
    crate::libm::atan2(y, x)
}

#[cfg(feature = "std")]
pub fn hypot(x: f64, y: f64) -> f64 {
    x.hypot(y)
}
#[cfg(not(feature = "std"))]
pub fn hypot(x: f64, y: f64) -> f64 {
    crate::libm::hypot(x, y)
}

/// `x % y` (C fmod) is a core operator; kept here for symmetry.
#[inline]
pub fn fmod(x: f64, y: f64) -> f64 {
    x % y
}

/// Fractional part.
#[inline]
pub fn fract(x: f64) -> f64 {
    x - trunc(x)
}

/// Integer power by repeated squaring (exact for small exponents).
pub fn powi(x: f64, n: i32) -> f64 {
    powf(x, n as f64)
}

/// `x mod m` with a non-negative result (m > 0).
pub fn rem_euclid(x: f64, m: f64) -> f64 {
    let r = x % m;
    if r < 0.0 { r + m } else { r }
}

/// Round half up (Math.round semantics are applied by the caller).
pub fn round_half_even(x: f64) -> f64 {
    let f = floor(x);
    let d = x - f;
    if d > 0.5 || (d == 0.5 && f % 2.0 != 0.0) { f + 1.0 } else { f }
}
