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

#[test]
fn zero_copy_binary_data() {
    let mut vm = rujse::Vm::new();
    let data = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
    let ptr = data.as_ptr();
    let buf = vm.array_buffer_from_vec(data);
    assert_eq!(vm.array_buffer_bytes(buf).unwrap().as_ptr(), ptr, "moved in, not copied");
    vm.global_prop("buf", buf);
    let view = vm.eval_script("globalThis.v = new Uint16Array(buf, 2, 2); v[0] = 0x0a0b; new DataView(buf, 6)", "t.js").unwrap();
    assert_eq!(vm.view_bytes(view).unwrap(), &[7, 8]);
    let v = vm.eval_script("v", "t.js").unwrap();
    assert_eq!(vm.view_bytes(v).unwrap(), &[0x0b, 0x0a, 5, 6]);
    vm.array_buffer_bytes_mut(buf).unwrap()[0] = 9;
    assert_eq!(vm.eval_script("new Uint8Array(buf)[0]", "t.js").unwrap(), rujse::Value::int(9));
    let out = vm.array_buffer_take(buf).unwrap();
    assert_eq!(out.as_ptr(), ptr, "moved out, not copied");
    assert_eq!(vm.eval_script("buf.detached + ':' + buf.byteLength + ':' + v.length", "t.js").map(|v| vm.display(v)).unwrap(), "true:0:0");
    let s = vm.string_from_utf8("plain ascii");
    assert_eq!(vm.string_as_ascii(s), Some("plain ascii"));
}

#[test]
fn budget_stops_loops_inside_natives() {
    // Natives that drive JS (spread, Array.from, a callback per element)
    // stop at the budget like a JS loop, and the VM stays usable.
    let mut vm = rujse::Vm::new();
    let endless = "const it = { [Symbol.iterator]() { return { next() { return { done: false, value: 1 } } } } };";
    for src in [
        "const [...x] = it;",
        "Array.from(it);",
        "[...it];",
        "new Array(1e6).fill(0).forEach(() => { for (const _ of [1]) {} }); Array.from({ length: 1e9 }, () => 0);",
    ] {
        vm.budget = Some(5_000_000);
        let r = vm.eval_script(&format!("{endless} {src}"), "t.js");
        assert!(matches!(r, Err(rujse::Throw::Terminate)) || matches!(r, Err(rujse::Throw::Value(_))), "{src}");
    }
    vm.budget = None;
    assert_eq!(vm.eval_script("1 + 1", "t.js").unwrap(), rujse::Value::int(2));
}
