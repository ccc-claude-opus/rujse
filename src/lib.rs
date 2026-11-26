//! rujse: a JavaScript engine, built first for rubrowser (as wasm).
//!
//! The core is `no_std` + `alloc`; the `std` feature (on by default) adds
//! the host clock and random numbers for the CLI.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(any(feature = "std", test))]
extern crate std;

pub mod ast;
pub mod lexer;
pub mod parser;
pub mod unicode;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
