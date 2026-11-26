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
            let r = vm.eval_script(&src, &name).and_then(|_| vm.run_jobs());
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
