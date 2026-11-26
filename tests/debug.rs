//! Debugger and inspector API.

use std::cell::RefCell;
use std::rc::Rc;

use rujse::debug::{DebugHandler, PauseOnExceptions, PauseReason, Resume, ScopeType, ScriptInfo};
use rujse::{Value, Vm};

/// Records each pause as "reason line func vars…" and answers from a
/// script of resume actions.
struct Rec {
    log: Rc<RefCell<Vec<String>>>,
    actions: Vec<Resume>,
    eval: Option<&'static str>,
}

impl DebugHandler for Rec {
    fn paused(&mut self, vm: &mut Vm, reason: &PauseReason) -> Resume {
        let frames = vm.debug_frames();
        let top = &frames[0];
        let scopes = vm.debug_scopes(0);
        let mut vars: Vec<String> = Vec::new();
        for s in &scopes {
            if s.kind == ScopeType::Global {
                continue;
            }
            for (n, v) in &s.variables {
                let shown = match v {
                    Some(v) => vm.display(*v),
                    None => "<tdz>".into(),
                };
                vars.push(format!("{:?}:{n}={shown}", s.kind));
            }
        }
        let r = match reason {
            PauseReason::Breakpoint(_) => "bp".to_string(),
            PauseReason::DebuggerStatement => "debugger".into(),
            PauseReason::Step => "step".into(),
            PauseReason::Exception(v) => format!("exception({})", vm.display(*v)),
            PauseReason::Request => "request".into(),
        };
        let mut line = format!("{r} L{} {} [{}]", top.line, top.function_name, vars.join(" "));
        if let Some(src) = self.eval {
            let v = vm.debug_evaluate(0, src).map(|v| vm.display(v)).unwrap_or_else(|_| "error".into());
            line.push_str(&format!(" eval={v}"));
        }
        self.log.borrow_mut().push(line);
        if self.actions.is_empty() { Resume::Continue } else { self.actions.remove(0) }
    }

    fn script_parsed(&mut self, _vm: &mut Vm, s: &ScriptInfo) {
        self.log.borrow_mut().push(format!("parsed {}", s.url));
    }
}

fn setup(actions: Vec<Resume>, eval: Option<&'static str>) -> (Box<Vm>, Rc<RefCell<Vec<String>>>) {
    let mut vm = Vm::new();
    let log = Rc::new(RefCell::new(Vec::new()));
    vm.enable_debugger(Box::new(Rec { log: log.clone(), actions, eval }));
    (vm, log)
}

const SRC: &str = "function add(a, b) {
  let s = a + b;
  return s;
}
function f(n) {
  let t = 0;
  for (let i = 0; i < n; i++) {
    t = add(t, i);
  }
  return t;
}
f(3);
";

#[test]
fn breakpoint_frames_scopes_and_eval() {
    let (mut vm, log) = setup(vec![], Some("a * 10 + b"));
    vm.set_breakpoint("page.js", 3, None, None).unwrap();
    vm.eval_script(SRC, "page.js").unwrap();
    let log = log.borrow();
    assert_eq!(log[0], "parsed page.js");
    // Hit three times (i = 0, 1, 2), with locals and an evaluation.
    assert_eq!(log.len(), 4, "{log:?}");
    assert_eq!(log[1], "bp L3 add [Local:a=0 Local:b=0 Local:s=0] eval=0");
    assert_eq!(log[3], "bp L3 add [Local:a=1 Local:b=2 Local:s=3] eval=12");
}

#[test]
fn breakpoint_before_script_and_condition_and_removal() {
    let (mut vm, log) = setup(vec![], None);
    let id = vm.set_breakpoint("page.js", 8, None, Some("i === 2")).unwrap();
    vm.eval_script(SRC, "page.js").unwrap();
    assert_eq!(log.borrow().len(), 2, "{:?}", log.borrow());
    let hit = log.borrow()[1].clone();
    assert!(hit.starts_with("bp L8 f [") && hit.contains("Local:i=2") && hit.contains("Local:t=1"), "{hit}");
    assert_eq!(vm.breakpoint_locations(id).len(), 1);
    vm.remove_breakpoint(id);
    vm.eval_script("f(3)", "other.js").unwrap();
    assert_eq!(log.borrow().len(), 3); // only "parsed other.js"
}

#[test]
fn stepping() {
    // Over from line 6, then step over until line 8, into add, out again.
    let mut acts = vec![Resume::StepOver; 4];
    acts.extend([Resume::StepInto, Resume::StepInto, Resume::StepOut, Resume::Continue]);
    let (mut vm, log) = setup(acts, None);
    vm.set_breakpoint("page.js", 6, None, None).unwrap();
    vm.eval_script(SRC, "page.js").unwrap();
    let lines: Vec<String> = log.borrow()[1..].iter().map(|l| l.split(" [").next().unwrap().to_string()).collect();
    assert_eq!(lines[0], "bp L6 f");
    assert_eq!(lines[1], "step L7 f", "{lines:?}");
    assert!(lines.iter().any(|l| l == "step L8 f"), "{lines:?}");
    let into = lines.iter().position(|l| l.ends_with(" add")).expect("stepped into add");
    // Stepping out of add lands back in f.
    assert!(lines[into + 1..].iter().any(|l| l.ends_with(" f")), "{lines:?}");
    // Stepping over never enters add.
    assert!(lines[..5].iter().all(|l| !l.ends_with(" add")), "{lines:?}");
}

#[test]
fn step_into_call() {
    let (mut vm, log) = setup(vec![Resume::StepInto, Resume::StepInto, Resume::StepInto, Resume::Continue], None);
    vm.set_breakpoint("page.js", 8, None, None).unwrap();
    vm.eval_script(SRC, "page.js").unwrap();
    let l = log.borrow();
    assert!(l[1].starts_with("bp L8 f"), "{l:?}");
    assert!(l.iter().any(|x| x.starts_with("step L2 add [Local:a=0 Local:b=0 Local:s=<tdz>]")), "{l:?}");
}

#[test]
fn debugger_statement_closure_scope_and_exceptions() {
    let (mut vm, log) = setup(vec![], None);
    vm.eval_script("function outer() { let cap = 7; return function inner(x) { debugger; return cap + x; }; } outer()(1);", "d.js").unwrap();
    assert_eq!(log.borrow()[1], "debugger L1 inner [Local:x=1 Closure:cap=7]");

    vm.set_pause_on_exceptions(PauseOnExceptions::Uncaught);
    let _ = vm.eval_script("try { throw 1 } catch (e) {}", "e.js");
    assert_eq!(log.borrow().len(), 3); // caught: no pause
    let r = vm.eval_script("throw new TypeError('boom')", "e2.js");
    assert!(r.is_err());
    assert!(log.borrow().last().unwrap().starts_with("exception("), "{:?}", log.borrow());
}

#[test]
fn inspector() {
    let mut vm = Vm::new();
    let v = vm
        .eval_script("globalThis.o = { a: 1, get g() { throw new Error('ran getter') } }; o", "i.js")
        .unwrap();
    let id = vm.inspect_register(v, 1);
    let o = vm.inspect_get(id).unwrap();
    let props = vm.inspect_properties(o, false);
    let names: Vec<String> = props.iter().map(|p| vm.display(p.name)).collect();
    assert_eq!(names, ["a", "g"]);
    assert!(props[1].get.is_some() && props[1].value.is_none());
    let with_chain = vm.inspect_properties(o, true);
    assert!(with_chain.iter().any(|p| !p.own && vm.display(p.name) == "hasOwnProperty"));
    assert_eq!(vm.inspect_class_name(o), "Object");

    let p = vm.eval_script("Promise.resolve(5)", "i.js").unwrap();
    let ip = vm.internal_properties(p);
    let state = ip.iter().find(|(n, _)| *n == "[[PromiseState]]").unwrap().1;
    assert_eq!(vm.display(state), "fulfilled");
    let m = vm.eval_script("class Foo {}; new Map([[1, new Foo()]])", "i.js").unwrap();
    assert_eq!(vm.inspect_class_name(m), "Map(1)");
    let entries = vm.internal_properties(m).into_iter().find(|(n, _)| *n == "[[Entries]]").unwrap().1;
    let first = vm.inspect_properties(entries, false)[0].value.unwrap();
    let foo = vm.inspect_properties(first, false)[1].value.unwrap();
    assert_eq!(vm.inspect_class_name(foo), "Foo");
    let f = vm.eval_script("(function named() {})", "i.js").unwrap();
    let loc = vm.internal_properties(f).into_iter().find(|(n, _)| *n == "[[FunctionLocation]]").unwrap().1;
    assert!(!vm.inspect_properties(loc, false).is_empty());

    vm.inspect_release_group(1);
    assert!(vm.inspect_get(id).is_none());
    let _ = Value::UNDEFINED;
}
