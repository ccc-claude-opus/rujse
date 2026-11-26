//! rujse as a wasmbench guest (the same protocol as the boa guest).

use std::cell::RefCell;

mod host {
    #[link(wasm_import_module = "bench")]
    unsafe extern "C" {
        pub fn now() -> f64;
        pub fn print(ptr: *const u8, len: usize);
    }
}

struct Host;

impl rujse::Host for Host {
    fn now(&mut self) -> f64 {
        unsafe { host::now() }
    }
    fn print(&mut self, t: &str) {
        unsafe { host::print(t.as_ptr(), t.len()) };
    }
}

thread_local! {
    static VM: RefCell<Option<Box<rujse::Vm>>> = const { RefCell::new(None) };
}

#[unsafe(no_mangle)]
pub extern "C" fn bench_alloc(len: usize) -> *mut u8 {
    Box::leak(vec![0u8; len].into_boxed_slice()).as_mut_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn bench_init() {
    let mut vm = rujse::Vm::new();
    vm.host = Some(Box::new(Host));
    let print = vm.native_fn("print", 1, |vm, _, a, _| {
        let v = a.first().copied().unwrap_or(rujse::Value::UNDEFINED);
        let s = vm.to_string(v)?;
        let t = vm.str(s).to_rust_lossy();
        unsafe { host::print(t.as_ptr(), t.len()) };
        Ok(rujse::Value::UNDEFINED)
    });
    vm.global_prop("print", rujse::Value::object(print));
    VM.with(|v| *v.borrow_mut() = Some(vm));
}

/// Runs a script; 0 = completed, 1 = threw (the error is printed).
#[unsafe(no_mangle)]
pub extern "C" fn bench_eval(ptr: *const u8, len: usize) -> i32 {
    let src = unsafe { std::slice::from_raw_parts(ptr, len) };
    let src = String::from_utf8_lossy(src).into_owned();
    VM.with(|v| {
        let mut b = v.borrow_mut();
        let vm = b.as_mut().expect("bench_init first");
        match vm.eval_script(&src, "bench.js").and_then(|_| vm.run_jobs()) {
            Ok(()) => 0,
            Err(rujse::Throw::Value(e)) => {
                let s = format!("Uncaught {}", vm.error_to_string(e));
                unsafe { host::print(s.as_ptr(), s.len()) };
                1
            }
            Err(rujse::Throw::Terminate) => 1,
        }
    })
}
