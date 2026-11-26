//! End-to-end checks: evaluate a script, compare the result as a string.

fn eval(src: &str) -> String {
    let mut vm = rujse::Vm::new();
    match vm.eval_script(src, "test.js") {
        Ok(v) => {
            let s = vm.to_string(v).unwrap();
            vm.str(s).to_rust_lossy()
        }
        Err(rujse::Throw::Value(e)) => format!("threw {}", vm.error_to_string(e)),
        Err(rujse::Throw::Terminate) => "terminated".into(),
    }
}

#[test]
fn operator_precedence() {
    assert_eq!(eval("var r = 83; r*77 + r*150"), "18841");
    assert_eq!(eval("2 ** 3 ** 2"), "512");
    assert_eq!(eval("1 - 2 - 3"), "-4");
    assert_eq!(eval("8 / 2 / 2"), "2");
    assert_eq!(eval("1 + 2 * 3 - 4"), "3");
    assert_eq!(eval("1 < 2 == true"), "true");
    assert_eq!(eval("1 | 2 & 3 ^ 4"), "7");
    assert_eq!(eval("null ?? 1 + 1"), "2");
}

#[test]
fn language_basics() {
    assert_eq!(eval("let s = 0; for (let i = 0; i < 10; i++) s += i; s"), "45");
    assert_eq!(eval("function f(a, b = 2, ...c) { return a + b + c.length } f(1, undefined, 3, 4)"), "5");
    assert_eq!(eval("const {a, b: [c] = [7]} = {a: 1}; a + c"), "8");
    assert_eq!(eval("class A { #x = 2; get x() { return this.#x } } new A().x"), "2");
    assert_eq!(eval("[...'ab'].join('-')"), "a-b");
    assert_eq!(eval("try { null.x } catch (e) { e instanceof TypeError }"), "true");
    assert_eq!(eval("(() => { try { return 1 } finally { globalThis.z = 2 } })() + z"), "3");
}

#[test]
fn budget_and_recursion() {
    let mut vm = rujse::Vm::new();
    vm.budget = Some(1_000_000);
    assert!(matches!(vm.eval_script("for (;;) {}", "t.js"), Err(rujse::Throw::Terminate)));
    // The VM is still usable after a termination.
    vm.budget = None;
    let v = vm.eval_script("1 + 1", "t.js").unwrap();
    assert_eq!(v.as_int(), Some(2));
    assert_eq!(eval("function f(n) { return f(n + 1) } try { f(0) } catch (e) { e instanceof RangeError }"), "true");
}
