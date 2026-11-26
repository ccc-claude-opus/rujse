#![no_std]
//! rujse: a JavaScript engine.
extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
