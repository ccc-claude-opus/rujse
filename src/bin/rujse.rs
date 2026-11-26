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
        _ => {
            println!("rujse {}", rujse::version());
        }
    }
}
