//! Mutation fuzzer: takes JavaScript files (test262 by default), mutates
//! them and runs each through the parser, the compiler and the VM (with a
//! budget and a heap limit). Any panic is a bug: in rubrowser a panic ends
//! the whole JavaScript runtime. Inputs that panic are saved to
//! `target/fuzz-crashes/`.
//!
//! ```text
//! fuzz [--seconds N] [--seed S] [--root DIR] [FILE...]
//! ```
//!
//! With FILEs given, runs each file as it is (to reproduce a crash).

use std::alloc::{GlobalAlloc, Layout, System};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Counts live bytes and their peak, to find inputs that allocate far
/// more than the VM's heap limit allows.
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let now = LIVE.fetch_add(l.size(), Relaxed) + l.size();
            PEAK.fetch_max(now, Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        LIVE.fetch_sub(l.size(), Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            if new > l.size() {
                let now = LIVE.fetch_add(new - l.size(), Relaxed) + new - l.size();
                PEAK.fetch_max(now, Relaxed);
            } else {
                LIVE.fetch_sub(l.size() - new, Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// A run may use this much (the VM's heap limit is 64 MB).
const MEMORY_LIMIT: usize = 256 << 20;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
}

/// Fragments that make interesting syntax when spliced in.
const SNIPPETS: &[&str] = &[
    "(", ")", "{", "}", "[", "]", ";", ",", "=>", "...", "?.", "??", "?", ":", "=", "+=", "**", "++", "--",
    "async ", "await ", "yield ", "function ", "function* ", "class ", "extends ", "super", "super.x",
    "new.target", "import.meta", "import(", "export ", "let ", "const ", "var ", "of ", "in ", "for (",
    "while (", "do ", "try {", "catch (e) {", "finally {", "throw ", "return ", "break ", "continue ",
    "with (", "switch (", "case 1:", "default:", "this", "null", "undefined", "NaN", "0n", "1e309", "-0",
    "`${", "}`", "\"", "'", "/a+/g", "/(?<n>x)\\k<n>/", "#p", "static ", "get ", "set ", "delete ",
    "typeof ", "void ", "debugger;", "label: ", "\\u0061", "\n", " ", "0x7fffffff", "2147483648", "Symbol()",
    "Proxy", "Reflect", "new Proxy({}, {})", "[].concat", "Array(2**31)", "'x'.repeat(1e3)", "eval(",
];

fn mutate(rng: &mut Rng, src: &str, corpus: &[String]) -> String {
    let mut s: Vec<char> = src.chars().collect();
    for _ in 0..1 + rng.below(4) {
        let len = s.len();
        match rng.below(6) {
            // Delete a range.
            0 if len > 0 => {
                let a = rng.below(len);
                let b = (a + 1 + rng.below(16)).min(len);
                s.drain(a..b);
            }
            // Duplicate a range.
            1 if len > 0 => {
                let a = rng.below(len);
                let b = (a + 1 + rng.below(32)).min(len);
                let piece: Vec<char> = s[a..b].to_vec();
                let at = rng.below(s.len() + 1);
                s.splice(at..at, piece);
            }
            // Insert a snippet.
            2 | 3 => {
                let at = rng.below(len + 1);
                let snip = SNIPPETS[rng.below(SNIPPETS.len())];
                s.splice(at..at, snip.chars());
            }
            // Splice in a piece of another file.
            4 if !corpus.is_empty() => {
                let other: Vec<char> = corpus[rng.below(corpus.len())].chars().collect();
                if !other.is_empty() {
                    let a = rng.below(other.len());
                    let b = (a + 1 + rng.below(64)).min(other.len());
                    let at = rng.below(len + 1);
                    s.splice(at..at, other[a..b].iter().copied());
                }
            }
            // Replace a character with a random ASCII one.
            _ if len > 0 => {
                let at = rng.below(len);
                s[at] = (32 + rng.below(95) as u8) as char;
            }
            _ => {}
        }
    }
    s.into_iter().collect()
}

/// Runs a script; Err(message) if anything panicked or it used far more
/// memory than the heap limit.
fn run(src: &str) -> Result<(), String> {
    PEAK.store(LIVE.load(Relaxed), Relaxed);
    let base = LIVE.load(Relaxed);
    let r = run_inner(src);
    let peak = PEAK.load(Relaxed).saturating_sub(base);
    if r.is_ok() && peak > MEMORY_LIMIT {
        return Err(format!("memory: peak {} MB", peak >> 20));
    }
    r
}

static PARSE_ONLY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn run_inner(src: &str) -> Result<(), String> {
    let r = panic::catch_unwind(AssertUnwindSafe(|| {
        if PARSE_ONLY.load(Relaxed) {
            let _ = rujse::parser::Parser::parse_script(src, false);
            return;
        }
        let mut vm = rujse::Vm::new();
        vm.limits.max_heap_bytes = 64 << 20;
        vm.limits.max_frames = 2_000;
        vm.budget = Some(2_000_000);
        let _ = vm.eval_script(src, "fuzz.js");
        let _ = vm.run_jobs();
        // Module goal too: different grammar.
        let _ = rujse::parser::Parser::parse_module(src);
    }));
    r.map_err(|e| {
        e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()
    })
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().is_some_and(|x| x == "js") && !p.to_string_lossy().contains("_FIXTURE") {
            out.push(p);
        }
    }
}

fn main() {
    let mut seconds = 60u64;
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut root = PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("src/test262/test");
    let mut files = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--seconds" => seconds = args.next().and_then(|v| v.parse().ok()).expect("--seconds N"),
            "--seed" => seed = args.next().and_then(|v| v.parse().ok()).expect("--seed S"),
            "--root" => root = args.next().expect("--root DIR").into(),
            "--parse-only" => PARSE_ONLY.store(true, Relaxed),
            f => files.push(PathBuf::from(f)),
        }
    }
    // Quiet panics: they are reported below.
    panic::set_hook(Box::new(|_| {}));
    if !files.is_empty() {
        for f in &files {
            let src = std::fs::read_to_string(f).unwrap_or_default();
            match run(&src) {
                Ok(()) => println!("ok    {} (peak {} MB)", f.display(), PEAK.load(Relaxed) >> 20),
                Err(m) => println!("PANIC {}: {m}", f.display()),
            }
        }
        return;
    }
    let mut paths = Vec::new();
    collect(&root, &mut paths);
    paths.sort();
    let mut rng = Rng(seed | 1);
    // A small in-memory corpus for splicing.
    let corpus: Vec<String> = (0..200).filter_map(|_| std::fs::read_to_string(&paths[rng.below(paths.len())]).ok()).collect();
    let out = PathBuf::from("target/fuzz-crashes");
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let (mut runs, mut crashes) = (0u64, 0u64);
    let mut seen: Vec<String> = Vec::new();
    while Instant::now() < deadline {
        let p = &paths[rng.below(paths.len())];
        let Ok(src) = std::fs::read_to_string(p) else { continue };
        if src.len() > 20_000 {
            continue;
        }
        let m = mutate(&mut rng, &src, &corpus);
        runs += 1;
        // The input being run, for when the process dies (out of memory).
        let _ = std::fs::write("target/fuzz-current.js", &m);
        if let Err(msg) = run(&m) {
            crashes += 1;
            // One file per distinct message.
            let key: String = if msg.starts_with("memory:") { format!("memory {runs}") } else { msg.chars().take(120).collect() };
            if !seen.contains(&key) {
                seen.push(key);
                let _ = std::fs::create_dir_all(&out);
                let f = out.join(format!("crash-{}.js", seen.len()));
                let _ = std::fs::write(&f, &m);
                println!("PANIC {}: {msg}", f.display());
            }
        }
    }
    println!("{runs} runs, {crashes} panics, {} distinct", seen.len());
}
