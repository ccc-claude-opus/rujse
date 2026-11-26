# 除錯與物件檢視 API

rujse 提供 Rust API，rubrowser 在 `cdp.rs` 把它轉成 Chrome DevTools Protocol（`Debugger`、`Runtime`）。對應 [plan.md](plan.md) §5.3、§5.4（缺陷 6、7）。行號、欄號都從 **1** 開始（CDP 從 0 開始，轉換在 rubrowser 做）。

## 除錯器

```rust
use rujse::debug::{DebugHandler, PauseReason, Resume, ScriptInfo};

struct Devtools { /* 和前端的連線 */ }

impl DebugHandler for Devtools {
    fn script_parsed(&mut self, vm: &mut Vm, s: &ScriptInfo) {
        // CDP Debugger.scriptParsed：s.id、s.url、s.source
    }
    fn paused(&mut self, vm: &mut Vm, reason: &PauseReason) -> Resume {
        let frames = vm.debug_frames();          // 呼叫堆疊，0 = 最內層
        let scopes = vm.debug_scopes(0);         // 第 0 層的 scope 鏈
        let v = vm.debug_evaluate(0, "a + b");   // 在第 0 層求值
        // 在這裡跑 CDP 的巢狀訊息迴圈，直到前端要求 resume / step
        Resume::StepOver
    }
}

vm.enable_debugger(Box::new(Devtools { .. }));
let id = vm.set_breakpoint("https://example.com/app.js", 12, None, Some("i > 3"));
vm.set_pause_on_exceptions(PauseOnExceptions::Uncaught);
```

| API | 說明 | CDP |
|---|---|---|
| `enable_debugger(handler)`／`disable_debugger()` | 開關；關閉時移除所有修補 | `Debugger.enable`／`disable` |
| `DebugHandler::script_parsed` | 每個頂層程式碼（script、module、eval、`Function`）編譯後呼叫 | `Debugger.scriptParsed` |
| `set_breakpoint(url, line, column, condition)` | 依 URL 設定，之後才載入的 script 也會套用；回傳 id | `setBreakpointByUrl` |
| `breakpoint_locations(id)` | 實際落點 (script id, 行, 列) | `breakpointResolved` |
| `remove_breakpoint(id)` | | `removeBreakpoint` |
| `set_pause_on_exceptions(None/Uncaught/All)` | `Uncaught`：JS 堆疊上沒有 `try` 包住時才停 | `setPauseOnExceptions` |
| `debug_request_pause()` | 在下一個位置停下 | `Debugger.pause` |
| `debug_script_source(id)` | | `getScriptSource` |
| `DebugHandler::paused` 的回傳值 | `Continue`、`StepInto`、`StepOver`、`StepOut` | `resume`、`stepInto`…… |
| `debug_frames()` | 函式名、script id、URL、行列、`this`、callee | `paused.callFrames` |
| `debug_scopes(i)` | `Local`、`Closure`、`With`、`Global`，每個變數 `(名稱, Some(值))`，TDZ 中為 `None` | `callFrame.scopeChain` |
| `debug_evaluate(i, src)` | 在第 i 層求值：看得到該層的變數（副本，指派不會寫回）與 `this` | `evaluateOnCallFrame` |

停下的原因：`Breakpoint(id)`、`DebuggerStatement`（`debugger;`）、`Step`、`Exception(值)`、`Request`。`paused` 執行期間不會再停（例如在裡面 `debug_evaluate` 觸發的中斷點會被忽略）。

### 實作

* **修補**：`Code.ops` 的每個指令放在 `Cell` 裡。中斷點和單步位置把指令換成 `Op::Break(i)`，原指令存在 `Code.patches[i]`。沒有設定任何東西時直譯器完全不受影響（沒有逐指令的檢查）。
* 命中 `Break` 時先處理（條件、暫停），再把原指令暫時放回去執行，並在下一個安全點重新裝上 `Break`。回到同一個 pc 一定要經過往回跳或呼叫，兩者都是安全點。
* **位置**：編譯器記錄的原始碼位置（每個敘述開頭，以及部分運算式），和錯誤訊息的行號表是同一份。中斷點取該行上欄號最小（且 ≥ 指定欄）的位置，巢狀函式也算在內。
* **單步**：恢復時在堆疊上每個函式的位置都裝上單步修補，停下時依模式判斷：`StepInto` 任何位置都停（新進入的函式也會裝上）；`StepOver` 只在堆疊深度 ≤ 原本時停；`StepOut` 只在深度 < 原本時停。停下後全部拆除。
* **變數**：暫存器裡的變數靠編譯器產生的範圍表（`Code.scopes`：名稱、暫存器、pc 範圍）；被閉包捕捉的變數在 environment 裡，environment 一律帶著槽位名稱。一個 frame 自己推入的 environment 算 `Local`，更外層的算 `Closure`。
* **例外**：例外離開指令迴圈、尋找 handler 之前呼叫；同一個例外穿過多層原生呼叫只報一次。

## 物件檢視

```rust
let id = vm.inspect_register(value, group);        // RemoteObject.objectId
let v = vm.inspect_get(id).unwrap();
let props = vm.inspect_properties(v, /*原型鏈*/ true);
let internal = vm.internal_properties(v);           // [[PromiseState]]、[[Entries]]……
let name = vm.inspect_class_name(v);                // "Map(2)"、"Array(3)"、"Foo"
vm.inspect_release_group(group);                    // Runtime.releaseObjectGroup
```

* 登記的值在釋放前不會被 GC 回收。
* `inspect_properties` **不執行任何 JS**：accessor 回傳 `get`／`set` 函式而不呼叫；Proxy 不呼叫 trap，只在 `internal_properties` 列出 `[[Target]]`、`[[Handler]]`。
* `internal_properties`：`[[Prototype]]`、`[[PromiseState]]`／`[[PromiseResult]]`、Map／Set 的 `[[Entries]]`、`[[TargetFunction]]`／`[[BoundThis]]`／`[[BoundArgs]]`、Proxy、`[[FunctionLocation]]`（`{scriptId, lineNumber, columnNumber}`）、`[[GeneratorState]]`、`[[PrimitiveValue]]`、`[[DateValue]]`、`[[WeakRefTarget]]`、型別陣列與 DataView 的 `[[Buffer]]`、ArrayBuffer 的長度與 detached 狀態。
* `inspect_class_name` 讀原型上 `constructor.name` 的資料屬性（不觸發 getter）。

## 尚未做

* `debug_evaluate` 的指派不寫回 frame 的變數；`Closure` scope 以 environment 為單位，同一個外層函式的多個區塊會分成多個 scope。
* 沒有 `[[Scopes]]` 內部屬性（函式的閉包變數）、沒有 `Debugger.setVariableValue`。
* async 堆疊（`await` 之前的呼叫者）。
