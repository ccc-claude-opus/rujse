//! Math.

use crate::vm::Vm;

pub fn init(_vm: &mut Vm) {}

/// Number::exponentiate for the non-trivial cases.
pub fn pow(x: f64, y: f64) -> f64 {
    crate::math::powf(x, y)
}
