//! test262 runner for rujse.
//!
//! ```text
//! test262 [--root DIR] [--parse-only] [--filter SUBSTR]... [-j N]
//!         [--check FILE] [--write FILE] [--failures FILE] [-v]
//! ```
//!
//! Runs every test under `DIR/test` (except `intl402/` and `staging/`),
//! in strict and sloppy mode as the metadata asks. `--check FILE` compares
//! the failures with a list of known failures and exits 1 on a new one.

mod meta;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use meta::Meta;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Outcome {
    Pass,
    Fail,
    Skip,
}

struct Opts {
    root: PathBuf,
    parse_only: bool,
    filters: Vec<String>,
    jobs: usize,
    check: Option<PathBuf>,
    write: Option<PathBuf>,
    failures: Option<PathBuf>,
    verbose: bool,
}

fn parse_args() -> Opts {
    let mut o = Opts {
        root: std::env::var_os("TEST262")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("src/test262")),
        parse_only: false,
        filters: Vec::new(),
        jobs: 4,
        check: None,
        write: None,
        failures: None,
        verbose: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--root" => o.root = args.next().expect("--root DIR").into(),
            "--parse-only" => o.parse_only = true,
            "--filter" => o.filters.push(args.next().expect("--filter SUBSTR")),
            "-j" => o.jobs = args.next().expect("-j N").parse().expect("-j N"),
            "--check" => o.check = Some(args.next().expect("--check FILE").into()),
            "--write" => o.write = Some(args.next().expect("--write FILE").into()),
            "--failures" => o.failures = Some(args.next().expect("--failures FILE").into()),
            "-v" => o.verbose = true,
            other if !other.starts_with('-') => o.filters.push(other.to_string()),
            other => panic!("unknown option {other}"),
        }
    }
    o
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().is_some_and(|e| e == "js") && !p.to_string_lossy().contains("_FIXTURE") {
            out.push(p);
        }
    }
}

/// Features rujse does not implement (yet): their tests are skipped.
const UNSUPPORTED_FEATURES: &[&str] = &[
    "decorators",
    "source-phase-imports",
    "source-phase-imports-module-source",
    "import-defer",
    "Intl",
    "IsHTMLDDA",
    "cross-realm",
    "tail-call-optimization",
    "explicit-resource-management",
    "Atomics.waitAsync",
    "ShadowRealm",
];

fn run_parse_only(path: &Path, src: &str, meta: &Meta) -> Result<(), String> {
    let negative_parse = meta.negative.as_ref().is_some_and(|n| n.phase == "parse");
    let results: Vec<Result<(), String>> = if meta.flags.module {
        vec![rujse::parser::Parser::parse_module(src).map(|_| ()).map_err(|e| e.message)]
    } else {
        let mut v = Vec::new();
        if !meta.flags.only_strict {
            v.push(rujse::parser::Parser::parse_script(src, false).map(|_| ()).map_err(|e| e.message));
        }
        if !meta.flags.no_strict && !meta.flags.raw {
            v.push(rujse::parser::Parser::parse_script(src, true).map(|_| ()).map_err(|e| e.message));
        }
        v
    };
    let _ = path;
    for r in results {
        match (negative_parse, r) {
            (true, Ok(())) => return Err("expected a SyntaxError, parsed fine".into()),
            (false, Err(e)) => return Err(format!("parse error: {e}")),
            _ => {}
        }
    }
    Ok(())
}

fn main() {
    let o = parse_args();
    let test_dir = o.root.join("test");
    let mut files = Vec::new();
    collect(&test_dir, &mut files);
    let files: Vec<PathBuf> = files
        .into_iter()
        .filter(|p| {
            let rel = p.strip_prefix(&test_dir).unwrap().to_string_lossy().into_owned();
            !rel.starts_with("intl402/") && !rel.starts_with("staging/")
                && (o.filters.is_empty() || o.filters.iter().any(|f| rel.contains(f.as_str())))
        })
        .collect();
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<(String, Outcome, String)>> = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..o.jobs {
            std::thread::Builder::new().stack_size(256 << 20).spawn_scoped(s, || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(path) = files.get(i) else { break };
                let rel = path.strip_prefix(&test_dir).unwrap().to_string_lossy().into_owned();
                let src = std::fs::read_to_string(path).unwrap_or_default();
                let meta = Meta::parse(&src);
                let (outcome, msg) = if meta.features.iter().any(|f| UNSUPPORTED_FEATURES.contains(&f.as_str())) {
                    (Outcome::Skip, String::new())
                } else {
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if o.parse_only { run_parse_only(path, &src, &meta) } else { runner_full(&o, path, &src, &meta) }
                    }));
                    match r {
                        Ok(Ok(())) => (Outcome::Pass, String::new()),
                        Ok(Err(m)) => (Outcome::Fail, m),
                        Err(p) => {
                            let m = p
                                .downcast_ref::<String>()
                                .cloned()
                                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                                .unwrap_or_default();
                            (Outcome::Fail, format!("panic: {m}"))
                        }
                    }
                };
                results.lock().unwrap().push((rel, outcome, msg));
            }).expect("spawn test thread");
        }
    });
    let mut results = results.into_inner().unwrap();
    results.sort_by(|a, b| a.0.cmp(&b.0));
    report(&o, &results);
}

fn harness(o: &Opts, name: &str) -> String {
    use std::collections::HashMap;
    use std::sync::OnceLock;
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let c = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(s) = c.lock().unwrap().get(name) {
        return s.clone();
    }
    let s = std::fs::read_to_string(o.root.join("harness").join(name)).unwrap_or_default();
    c.lock().unwrap().insert(name.to_string(), s.clone());
    s
}

fn install_host(vm: &mut rujse::Vm, out: std::rc::Rc<std::cell::RefCell<Vec<String>>>) {
    use rujse::Value;
    struct H(std::rc::Rc<std::cell::RefCell<Vec<String>>>);
    impl rujse::Host for H {
        fn print(&mut self, t: &str) {
            self.0.borrow_mut().push(t.to_string());
        }
    }
    vm.host = Some(Box::new(H(out)));
    let print = vm.native_fn("print", 1, |vm, _, a, _| {
        let v = a.first().copied().unwrap_or(Value::UNDEFINED);
        let s = vm.to_string(v)?;
        let t = vm.str(s).to_rust_lossy();
        if let Some(h) = vm.host.as_mut() {
            h.print(&t);
        }
        Ok(Value::UNDEFINED)
    });
    vm.global_prop("print", Value::object(print));
    let d = vm.new_object();
    vm.global_prop("$262", Value::object(d));
    let g = vm.global;
    vm.prop(d, "global", Value::object(g), 7);
    vm.method(d, "evalScript", 1, |vm, _, a, _| {
        let v = a.first().copied().unwrap_or(Value::UNDEFINED);
        let s = vm.to_string(v)?;
        let src = vm.str(s).to_rust_lossy();
        vm.eval_script(&src, "evalScript")
    });
    vm.method(d, "gc", 0, |vm, _, _, _| {
        vm.collect_garbage();
        Ok(Value::UNDEFINED)
    });
    vm.method(d, "detachArrayBuffer", 1, |vm, _, a, _| {
        let v = a.first().copied().unwrap_or(Value::UNDEFINED);
        rujse::builtins::buffer::detach(vm, v)?;
        Ok(Value::NULL)
    });
    vm.method(d, "createRealm", 0, |vm, _, _, _| Err(vm.type_error("createRealm is not supported")));
}

/// Name of the error constructor of a thrown value.
fn error_name(vm: &mut rujse::Vm, e: rujse::Value) -> String {
    let Some(o) = e.as_object() else { return vm.display(e) };
    let kc = vm.key("constructor");
    let c = match vm.get(o, kc) {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    let Some(co) = c.as_object() else { return String::new() };
    let kn = vm.key("name");
    match vm.get(co, kn) {
        Ok(n) => vm.display(n),
        Err(_) => String::new(),
    }
}

fn run_once(o: &Opts, src: &str, meta: &Meta, strict: bool) -> Result<(), String> {
    let mut code = String::new();
    if strict {
        code.push_str("'use strict';\n");
    }
    if !meta.flags.raw {
        code.push_str(&harness(o, "assert.js"));
        code.push('\n');
        code.push_str(&harness(o, "sta.js"));
        code.push('\n');
        if meta.flags.is_async {
            code.push_str(&harness(o, "doneprintHandle.js"));
            code.push('\n');
        }
        for inc in &meta.includes {
            code.push_str(&harness(o, inc));
            code.push('\n');
        }
    }
    code.push_str(src);
    let out = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let mut vm = rujse::Vm::new();
    vm.limits.max_heap_bytes = 256 << 20;
    vm.budget = Some(400_000_000);
    install_host(&mut vm, out.clone());
    let r = vm.eval_script(&code, "test.js").and_then(|_| vm.run_jobs());
    let negative = meta.negative.as_ref();
    match r {
        Ok(()) => {
            if let Some(n) = negative {
                return Err(format!("expected {} ({}), completed normally", n.kind, n.phase));
            }
            if meta.flags.is_async {
                let lines = out.borrow();
                if lines.iter().any(|l| l == "Test262:AsyncTestComplete") {
                    return Ok(());
                }
                return Err(format!("async test did not complete: {}", lines.join(" | ")));
            }
            Ok(())
        }
        Err(rujse::Throw::Terminate) => Err("timeout (budget exhausted)".into()),
        Err(rujse::Throw::Value(e)) => {
            let name = error_name(&mut vm, e);
            if let Some(n) = negative {
                if name == n.kind {
                    return Ok(());
                }
                return Err(format!("expected {}, got {}", n.kind, vm.error_to_string(e)));
            }
            Err(format!("threw {}", vm.error_to_string(e).lines().next().unwrap_or("")))
        }
    }
}

fn runner_full(o: &Opts, path: &Path, src: &str, meta: &Meta) -> Result<(), String> {
    let _ = path;
    if meta.flags.module {
        return Err("modules not supported yet".into());
    }
    if meta.flags.only_strict {
        return run_once(o, src, meta, true);
    }
    if meta.flags.no_strict || meta.flags.raw {
        return run_once(o, src, meta, false);
    }
    run_once(o, src, meta, false)?;
    run_once(o, src, meta, true)
}

fn report(o: &Opts, results: &[(String, Outcome, String)]) {
    let mut by_dir: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
    let (mut pass, mut fail, mut skip) = (0, 0, 0);
    for (rel, out, msg) in results {
        let parts: Vec<&str> = rel.split('/').collect();
        let dir = parts[..parts.len().min(2)].join("/");
        let e = by_dir.entry(dir).or_default();
        match out {
            Outcome::Pass => {
                pass += 1;
                e.0 += 1
            }
            Outcome::Fail => {
                fail += 1;
                e.1 += 1;
                if o.verbose {
                    println!("FAIL {rel}: {msg}");
                }
            }
            Outcome::Skip => {
                skip += 1;
                e.2 += 1
            }
        }
    }
    println!("{:<40} {:>7} {:>7} {:>6} {:>7}", "directory", "pass", "fail", "skip", "rate");
    for (d, (p, f, s)) in &by_dir {
        let rate = if p + f > 0 { 100.0 * *p as f64 / (*p + *f) as f64 } else { 100.0 };
        println!("{d:<40} {p:>7} {f:>7} {s:>6} {rate:>6.1}%");
    }
    let rate = 100.0 * pass as f64 / ((pass + fail).max(1)) as f64;
    println!("{:<40} {pass:>7} {fail:>7} {skip:>6} {rate:>6.2}%", "TOTAL");

    let failing: BTreeSet<&str> = results.iter().filter(|r| r.1 == Outcome::Fail).map(|r| r.0.as_str()).collect();
    if let Some(f) = &o.failures {
        let text: String = results
            .iter()
            .filter(|r| r.1 == Outcome::Fail)
            .map(|r| format!("{}\t{}\n", r.0, r.2.replace('\n', " ")))
            .collect();
        std::fs::write(f, text).expect("write failures");
    }
    if let Some(w) = &o.write {
        let text: String = failing.iter().map(|s| format!("{s}\n")).collect();
        std::fs::write(w, text).expect("write expectations");
        println!("wrote {} known failures to {}", failing.len(), w.display());
    }
    if let Some(c) = &o.check {
        let known: BTreeSet<String> = std::fs::read_to_string(c).unwrap_or_default().lines().map(|s| s.to_string()).collect();
        let ran: BTreeSet<&str> = results.iter().map(|r| r.0.as_str()).collect();
        let new: Vec<&&str> = failing.iter().filter(|f| !known.contains(**f)).collect();
        let fixed: Vec<&String> = known.iter().filter(|k| ran.contains(k.as_str()) && !failing.contains(k.as_str())).collect();
        if !fixed.is_empty() {
            println!("{} known failures now pass (update with --write)", fixed.len());
        }
        if !new.is_empty() {
            println!("{} NEW FAILURES:", new.len());
            for n in new.iter().take(50) {
                println!("  {n}");
            }
            std::process::exit(1);
        }
    }
}
