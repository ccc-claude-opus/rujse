//! rujse: a JavaScript engine, built first for rubrowser (as wasm).
//!
//! The core is `no_std` + `alloc`; the `std` feature (on by default) adds
//! the host clock, random numbers and std's float math.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(any(feature = "std", test))]
extern crate std;

pub mod ast;
pub mod builtins;
pub mod bytecode;
pub mod compiler;
pub mod debug;
pub mod heap;
pub mod lexer;
#[cfg(not(feature = "std"))]
pub mod libm;
pub mod math;
pub mod module;
pub mod object;
pub mod parser;
pub mod regex;
pub mod regex_props;
pub mod string;
pub mod unicode;
pub mod value;
pub mod vm;

pub use value::Value;
pub use vm::{JsResult, Throw, Vm};

/// What the embedder provides to the engine.
pub trait Host {
    /// Milliseconds since the epoch (Date.now).
    fn now(&mut self) -> f64 {
        0.0
    }
    /// Offset of local time from UTC in minutes at a UTC time (ms).
    fn local_tz_offset(&mut self, _utc_ms: f64) -> f64 {
        0.0
    }
    /// An error nobody caught (a job or a callback).
    fn uncaught_error(&mut self, _text: &str) {}
    /// A promise was rejected with no handler (`handled` = a handler was
    /// added later).
    fn promise_rejection(&mut self, _promise: Value, _handled: bool) {}
    /// Output of `print` / console.
    fn print(&mut self, _text: &str) {}
    /// Resolves a module specifier against the importing module/script
    /// URL; the result identifies the module (its URL).
    fn resolve_module(&mut self, _referrer: Option<&str>, _specifier: &str) -> Result<alloc::string::String, alloc::string::String> {
        Err("module loading is not supported by this host".into())
    }
    /// Fetches a module's source. May answer `Pending` and later call
    /// `Vm::finish_loading(url, …)`.
    fn fetch_module(&mut self, _url: &str) -> module::ModuleSource {
        module::ModuleSource::Error("module loading is not supported by this host".into())
    }
}

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
