//! rujse command line: run a script, or inspect tokens / AST / bytecode.
//!
//! ```text
//! rujse FILE.js              run a script
//! rujse -e 'code'            run code
//! rujse --tokens FILE.js     print tokens
//! rujse --ast FILE.js        print the syntax tree
//! ```

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(|s| s.as_str()).unwrap_or("");
    let read = |i: usize| -> String {
        match args.get(i).map(|s| s.as_str()) {
            Some("-e") => args.get(i + 1).cloned().unwrap_or_default(),
            Some(p) => std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{p}: {e}")),
            None => panic!("missing file"),
        }
    };
    match mode {
        "--tokens" => {
            let src = read(1);
            let mut lx = rujse::lexer::Lexer::new(&src);
            loop {
                match lx.next() {
                    Ok(t) => {
                        println!("{}:{} {:?}{}{}", t.line, t.col, t.tok, if t.nl_before { " nl" } else { "" }, if t.escaped { " esc" } else { "" });
                        if t.tok == rujse::lexer::Tok::Eof {
                            break;
                        }
                    }
                    Err(e) => {
                        println!("error {}:{} {}", e.line, e.col, e.message);
                        break;
                    }
                }
            }
        }
        "--bytecode" => {
            let src = read(1);
            let mut vm = rujse::Vm::new();
            let prog = rujse::parser::Parser::parse_script(&src, false).expect("syntax error");
            let source = vm.add_source("dump", src.as_str().into());
            let code = rujse::compiler::Compiler::new(&mut vm, source).script(&prog, false).expect("compile error");
            fn dump(c: &rujse::bytecode::Code, depth: usize) {
                let pad = "  ".repeat(depth);
                println!("{pad}== code nregs={} nparams={}", c.nregs, c.nparams);
                for (i, op) in c.ops.iter().enumerate() {
                    println!("{pad}{i:4} {op:?}");
                }
                for ch in &c.children {
                    dump(ch, depth + 1);
                }
            }
            dump(&code, 0);
        }
        "--phases" => {
            let src = read(1);
            let t0 = std::time::Instant::now();
            let prog = rujse::parser::Parser::parse_script(&src, false).expect("syntax error");
            let t1 = std::time::Instant::now();
            let mut vm = rujse::Vm::new();
            let t2 = std::time::Instant::now();
            let source = vm.add_source("x", src.as_str().into());
            let code = rujse::compiler::Compiler::new(&mut vm, source).script(&prog, false).expect("compile");
            let t3 = std::time::Instant::now();
            let this = rujse::Value::object(vm.global);
            let r = vm.run_code(code, None, this, rujse::Value::UNDEFINED, rujse::Value::UNDEFINED);
            let t4 = std::time::Instant::now();
            println!("parse {:?} vm {:?} compile {:?} run {:?} ok={}", t1 - t0, t2 - t1, t3 - t2, t4 - t3, r.is_ok());
        }
        "--ast" => {
            let src = read(1);
            match rujse::parser::Parser::parse_script(&src, false) {
                Ok(p) => println!("{p:#?}"),
                Err(e) => println!("SyntaxError {}:{} {}", e.line, e.col, e.message),
            }
        }
        "" | "--version" => {
            println!("rujse {}", rujse::version());
        }
        _ => {
            let (src, name) = if mode == "-e" { (read(0), String::from("<eval>")) } else { (read(0), args[0].clone()) };
            let mut vm = rujse::Vm::new();
            vm.host = Some(Box::new(CliHost));
            install_print(&mut vm);
            let is_module = name.ends_with(".mjs") || args.iter().any(|a| a == "--module");
            let r = if is_module {
                let path = std::fs::canonicalize(&name).map(|p| p.to_string_lossy().into_owned()).unwrap_or(name.clone());
                let p = vm.import_module(&path, None);
                vm.run_jobs().and_then(|_| match rujse::builtins::promise::state_of(&vm, p) {
                    Some((rujse::builtins::promise::State::Rejected, e)) => Err(rujse::Throw::Value(e)),
                    _ => Ok(()),
                })
            } else {
                vm.eval_script(&src, &name).and_then(|_| vm.run_jobs())
            };
            if let Err(rujse::Throw::Value(e)) = r {
                let s = vm.error_to_string(e);
                eprintln!("Uncaught {s}");
                std::process::exit(1);
            }
        }
    }
}

struct CliHost;

impl rujse::Host for CliHost {
    fn now(&mut self) -> f64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0)
    }
    fn uncaught_error(&mut self, text: &str) {
        eprintln!("Uncaught {text}");
    }
    fn print(&mut self, text: &str) {
        println!("{text}");
    }
    fn resolve_module(&mut self, referrer: Option<&str>, specifier: &str) -> Result<String, String> {
        resolve_path(referrer, specifier)
    }
    fn fetch_module(&mut self, url: &str) -> rujse::module::ModuleSource {
        match std::fs::read_to_string(url) {
            Ok(s) => rujse::module::ModuleSource::Ready(s),
            Err(e) => rujse::module::ModuleSource::Error(format!("cannot load module {url}: {e}")),
        }
    }
}

/// File-path module resolution: relative to the importing file.
pub fn resolve_path(referrer: Option<&str>, specifier: &str) -> Result<String, String> {
    let base = match referrer {
        Some(r) => std::path::Path::new(r).parent().map(|p| p.to_path_buf()).unwrap_or_default(),
        None => std::env::current_dir().unwrap_or_default(),
    };
    let p = if specifier.starts_with('/') { std::path::PathBuf::from(specifier) } else { base.join(specifier) };
    // Normalize . and .. without touching the file system.
    let mut out = std::path::PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    Ok(out.to_string_lossy().into_owned())
}

fn install_print(vm: &mut rujse::Vm) {
    let f = vm.native_fn("print", 1, |vm, _this, args, _nt| {
        let mut out = String::new();
        for (i, &a) in args.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            let s = vm.to_string(a)?;
            out.push_str(&vm.str(s).to_rust_lossy());
        }
        println!("{out}");
        Ok(rujse::Value::UNDEFINED)
    });
    vm.global_prop("print", rujse::Value::object(f));
}
