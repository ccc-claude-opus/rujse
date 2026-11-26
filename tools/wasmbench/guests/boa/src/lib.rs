//! boa 0.22 as a wasmbench guest: the baseline rujse is measured against.
//! Built the way rubrowser builds its `js.wasm` (same features, a host
//! clock, the getrandom "custom" backend).

use std::cell::RefCell;
use std::rc::Rc;

use boa_engine::context::time::JsInstant;
use boa_engine::context::Clock;
use boa_engine::property::Attribute;
use boa_engine::vm::RuntimeLimits;
use boa_engine::{js_string, Context, JsValue, NativeFunction, Source};

mod host {
    #[link(wasm_import_module = "bench")]
    unsafe extern "C" {
        pub fn now() -> f64;
        pub fn print(ptr: *const u8, len: usize);
    }
}

thread_local! {
    static CTX: RefCell<Option<Context>> = const { RefCell::new(None) };
}

struct HostClock;

impl Clock for HostClock {
    fn now(&self) -> JsInstant {
        let ms = unsafe { host::now() }.max(0.0);
        let secs = (ms / 1000.0) as u64;
        JsInstant::new(secs, ((ms - secs as f64 * 1000.0) * 1e6) as u32)
    }
    fn system_time_millis(&self) -> i64 {
        unsafe { host::now() as i64 }
    }
}

#[unsafe(no_mangle)]
unsafe extern "Rust" fn __getrandom_v03_custom(dest: *mut u8, len: usize) -> Result<(), getrandom03::Error> {
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for i in 0..len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        unsafe { *dest.add(i) = (x >> 24) as u8 };
    }
    Ok(())
}

#[unsafe(no_mangle)]
pub extern "C" fn bench_alloc(len: usize) -> *mut u8 {
    Box::leak(vec![0u8; len].into_boxed_slice()).as_mut_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn bench_init() {
    let mut ctx = Context::builder().clock(Rc::new(HostClock)).build().expect("context");
    let mut limits = RuntimeLimits::default();
    limits.set_recursion_limit(10_000);
    limits.set_stack_size_limit(1 << 20);
    ctx.set_runtime_limits(limits);
    let print = NativeFunction::from_fn_ptr(|_, args, ctx| {
        let s = args.first().cloned().unwrap_or_default().to_string(ctx)?.to_std_string_escaped();
        unsafe { host::print(s.as_ptr(), s.len()) };
        Ok(JsValue::undefined())
    });
    let f = boa_engine::object::FunctionObjectBuilder::new(ctx.realm(), print).name(js_string!("print")).length(1).build();
    ctx.register_global_property(js_string!("print"), f, Attribute::all()).expect("print");
    CTX.with(|c| *c.borrow_mut() = Some(ctx));
}

/// Runs a script; 0 = completed, 1 = threw (the error is printed).
#[unsafe(no_mangle)]
pub extern "C" fn bench_eval(ptr: *const u8, len: usize) -> i32 {
    let src = unsafe { std::slice::from_raw_parts(ptr, len) };
    CTX.with(|c| {
        let mut c = c.borrow_mut();
        let ctx = c.as_mut().expect("bench_init first");
        let r = ctx.eval(Source::from_bytes(src));
        let r = r.and_then(|v| {
            ctx.run_jobs()?;
            Ok(v)
        });
        match r {
            Ok(_) => 0,
            Err(e) => {
                let s = format!("Uncaught {e}");
                unsafe { host::print(s.as_ptr(), s.len()) };
                1
            }
        }
    })
}
