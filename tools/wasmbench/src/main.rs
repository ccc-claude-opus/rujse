//! wasmbench: counts the wasm instructions (wasmtime fuel, the unit of
//! rubrowser's Performance panel) that JS engines built as wasm spend on
//! the same scripts.
//!
//! ```text
//! wasmbench [--json] ENGINE.wasm... -- BENCH.js...
//! ```
//!
//! A guest exports `bench_alloc(len) -> ptr`, `bench_init()` and
//! `bench_eval(ptr, len) -> status` and imports `bench.now() -> f64` and
//! `bench.print(ptr, len)`. A script containing `__N__` is a loop
//! benchmark: it runs with N and 2N and the difference gives the cost of
//! one iteration, separate from the fixed cost.

use std::time::Instant;
use wasmtime::{Config, Engine, Instance, Linker, Module, Store};

const FUEL: u64 = u64::MAX / 2;

struct Guest {
    store: Store<Vec<String>>,
    instance: Instance,
}

impl Guest {
    fn new(engine: &Engine, module: &Module) -> Result<(Guest, u64), String> {
        let mut store = Store::new(engine, Vec::new());
        store.set_fuel(FUEL).map_err(|e| e.to_string())?;
        let mut linker = Linker::new(engine);
        let t0 = Instant::now();
        linker
            .func_wrap("bench", "now", move || t0.elapsed().as_secs_f64() * 1000.0)
            .map_err(|e| e.to_string())?;
        linker
            .func_wrap("bench", "print", |mut caller: wasmtime::Caller<'_, Vec<String>>, ptr: i32, len: i32| {
                let mem = caller.get_export("memory").and_then(|e| e.into_memory()).expect("memory");
                let mut buf = vec![0u8; len as usize];
                mem.read(&caller, ptr as u32 as usize, &mut buf).expect("print range");
                caller.data_mut().push(String::from_utf8_lossy(&buf).into_owned());
            })
            .map_err(|e| e.to_string())?;
        let instance = linker.instantiate(&mut store, module).map_err(|e| e.to_string())?;
        let mut g = Guest { store, instance };
        let before = g.store.get_fuel().unwrap();
        let init = g.instance.get_typed_func::<(), ()>(&mut g.store, "bench_init").map_err(|e| e.to_string())?;
        init.call(&mut g.store, ()).map_err(|e| e.to_string())?;
        let used = before - g.store.get_fuel().unwrap();
        Ok((g, used))
    }

    /// Runs one script; returns (fuel, nanoseconds, ok).
    fn eval(&mut self, src: &str) -> Result<(u64, u128, bool), String> {
        let alloc = self.instance.get_typed_func::<i32, i32>(&mut self.store, "bench_alloc").map_err(|e| e.to_string())?;
        let eval = self.instance.get_typed_func::<(i32, i32), i32>(&mut self.store, "bench_eval").map_err(|e| e.to_string())?;
        let ptr = alloc.call(&mut self.store, src.len() as i32).map_err(|e| e.to_string())?;
        let mem = self.instance.get_memory(&mut self.store, "memory").ok_or("no memory export")?;
        mem.write(&mut self.store, ptr as u32 as usize, src.as_bytes()).map_err(|e| e.to_string())?;
        let before = self.store.get_fuel().unwrap();
        let t = Instant::now();
        let status = eval.call(&mut self.store, (ptr, src.len() as i32)).map_err(|e| e.to_string())?;
        let ns = t.elapsed().as_nanos();
        Ok((before - self.store.get_fuel().unwrap(), ns, status == 0))
    }
}

fn engine_name(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    base.trim_end_matches(".wasm").trim_start_matches("wasmbench_").to_string()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let json = args.iter().any(|a| a == "--json");
    let sep = args.iter().position(|a| a == "--").unwrap_or_else(|| {
        eprintln!("usage: wasmbench [--json] ENGINE.wasm... -- BENCH.js...");
        std::process::exit(2)
    });
    let engines: Vec<&String> = args[..sep].iter().filter(|a| !a.starts_with("--")).collect();
    let benches = &args[sep + 1..];

    let mut config = Config::new();
    config.consume_fuel(true);
    let engine = Engine::new(&config).expect("wasmtime engine");
    let mut rows = Vec::new();
    for path in &engines {
        let name = engine_name(path);
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let t = Instant::now();
        let module = Module::from_file(&engine, path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let compile_ms = t.elapsed().as_secs_f64() * 1000.0;
        let (_, init_fuel) = Guest::new(&engine, &module).expect("instantiate");
        rows.push(format!("{name}\t(module)\tsize={size}\tcompile_ms={compile_ms:.0}\tinit_fuel={init_fuel}"));
        for b in benches {
            let src = std::fs::read_to_string(b).unwrap_or_else(|e| panic!("{b}: {e}"));
            let bname = b.rsplit('/').next().unwrap().trim_end_matches(".js");
            if let Some(n) = loop_count(&src) {
                let (mut g1, _) = Guest::new(&engine, &module).unwrap();
                let (f1, _, ok1) = g1.eval(&src.replace("__N__", &n.to_string())).unwrap();
                let (mut g2, _) = Guest::new(&engine, &module).unwrap();
                let (f2, ns2, ok2) = g2.eval(&src.replace("__N__", &(2 * n).to_string())).unwrap();
                let per = (f2 as f64 - f1 as f64) / n as f64;
                let ok = ok1 && ok2;
                rows.push(format!(
                    "{name}\t{bname}\tper_iter_fuel={per:.1}\tfixed_fuel={}\tms_2n={:.1}\tok={ok}",
                    (f1 as f64 - per * n as f64).max(0.0) as u64,
                    ns2 as f64 / 1e6
                ));
                report_output(&g1, &g2, ok);
            } else {
                let (mut g, _) = Guest::new(&engine, &module).unwrap();
                match g.eval(&src) {
                    Ok((f, ns, ok)) => {
                        rows.push(format!("{name}\t{bname}\tfuel={f}\tms={:.1}\tok={ok}", ns as f64 / 1e6));
                        if !ok || !g.store.data().is_empty() {
                            for l in g.store.data() {
                                eprintln!("  [{name} {bname}] {l}");
                            }
                        }
                    }
                    Err(e) => rows.push(format!("{name}\t{bname}\ttrap={e}")),
                }
            }
        }
    }
    for r in &rows {
        if json {
            let fields: Vec<String> = r
                .split('\t')
                .enumerate()
                .map(|(i, f)| match i {
                    0 => format!("\"engine\":\"{f}\""),
                    1 => format!("\"bench\":\"{f}\""),
                    _ => {
                        let (k, v) = f.split_once('=').unwrap_or((f, ""));
                        if v.parse::<f64>().is_ok() || v == "true" || v == "false" {
                            format!("\"{k}\":{v}")
                        } else {
                            format!("\"{k}\":\"{}\"", v.replace('"', "'"))
                        }
                    }
                })
                .collect();
            println!("{{{}}}", fields.join(","));
        } else {
            println!("{r}");
        }
    }
}

fn report_output(g1: &Guest, g2: &Guest, ok: bool) {
    if !ok {
        for l in g1.store.data().iter().chain(g2.store.data()) {
            eprintln!("  {l}");
        }
    }
}

/// `// wasmbench: N=100000` sets the loop count (default 100000).
fn loop_count(src: &str) -> Option<u64> {
    if !src.contains("__N__") {
        return None;
    }
    let n = src
        .lines()
        .find_map(|l| l.trim().strip_prefix("// wasmbench: N="))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(100_000);
    Some(n)
}
