# 進度（2026-11-26）

對照 [plan.md](plan.md) 的缺陷表（§1）與里程碑（§10）。數字的量法見 [bench.md](bench.md)。

## rubrowser 的 11 項缺陷

| # | 缺陷 | 狀態 |
|---|---|---|
| 1 | 速度 | A 階段（直譯器）：每圈 wasm 指令為 boa 的 1/2.6～1/7.9（全域迴圈 12,048 → 1,517）。目標 1/10 要靠 B 階段（JIT 成 wasm），見下方「每個 op 的底線」 |
| 2 | 啟動成本 | 模組 2.1 MB（boa 6.3 MB），wasmtime 編譯 0.3 s（boa 0.9 s 以上）；prelude 4.84 億 → 3,060 萬條（bytecode cache + 延遲解碼）。heap snapshot（目標 ≤ 1,000 萬）未做 |
| 3 | 深遞迴的 RangeError 不能捕捉 | 完成：JS 呼叫不佔 Rust 堆疊，超過 1 萬層丟出可捕捉的 `RangeError` |
| 4 | 不能乾淨中斷 | 完成：預算、`interrupt` 旗標、`Host::interrupt_requested`（安全點輪詢），中斷後 VM 仍可用；宿主中途放棄的呼叫用 `Vm::recover` |
| 5 | Intl 資料 | 未做（M10）；rubrowser 的 prelude 自己提供 Intl，rujse 補上了 `String.prototype.normalize` |
| 6 | 除錯 API | rujse 端完成（[debugger.md](debugger.md)）：中斷點、單步、例外暫停、呼叫堆疊、scope 變數、在 frame 求值。rubrowser 的 CDP 對接未做 |
| 7 | 物件檢視 API | rujse 端完成：不執行 JS 的屬性列表、內部屬性、物件群組。rubrowser 的 CDP 對接未做 |
| 8 | 零拷貝 ArrayBuffer | 完成：`array_buffer_from_vec`、`array_buffer_bytes(_mut)`、`view_bytes`、`array_buffer_take`、`uint8_array_from_vec` |
| 9 | 字串轉換成本 | Latin-1 儲存、ASCII 直接 `&str`（`string_as_ascii`）；外部字串未做 |
| 10 | 非同步模組載入 | 完成：`Host::fetch_module` 可回 `Pending`，之後 `Vm::finish_loading` |
| 11 | 可重入的 Context | 完成：`Vm` 由宿主直接擁有，原生函式拿到 `&mut Vm` 可再呼叫 JS |

## 里程碑

| | 狀態 |
|---|---|
| M0 骨架 | 完成 |
| M1 parser | 完成 |
| M2 核心 VM | 完成 |
| M3 完整內建 | 完成。test262 97.24%（41,443 過、1,175 失敗、6,302 略過：Intl、Temporal、decorators 等提案） |
| M4 rubrowser 導入 | 完成：rubrowser 的 `js-runtime-rujse` feature 與 `--features rujse`，242 個函式庫測試（含全部 JS 測試）通過；prelude 以 bytecode cache 載入。未做：100 MB 上傳只有一份資料的驗證 |
| M5 直譯器效能 | 大部分完成：inline cache（全域、具名屬性、shape 轉移）、int32 與 double 快路徑、超指令、rope、bytecode cache。未做：heap snapshot |
| M6 除錯與檢視 | rujse 的 API 完成；rubrowser 的 CDP 對接未做 |
| M7 符合度 | test262 97.25%；`tools/fuzz`（突變 test262 檔案，抓 panic、超量配置與洩漏）已跑約 300 萬次無錯，沿途修掉的問題見下方；24 小時連續 fuzz 與 node 差分測試未做 |
| M8–M10 | 未開始 |

## 資源上限（fuzz 找到並修正的）

在瀏覽器裡 panic 或吃光記憶體都等於整個 JS runtime 掛掉，所以這些都當成錯誤處理：

* heap 上限原本只在安全點檢查：原生函式裡的迴圈（`fill`、`repeat`、`join`、`replace` 的 `$1`、`reduceRight` 走過 2⁵³ 個索引、對無窮迭代器做 `[...x]`）可以一次配置數 GB。現在字串長度受 `Vm::max_string_len()`（2²⁹ 或 heap 上限能容納的長度）限制；元素、Map/Set 條目、atom 都計入配置量；Rust 呼叫 JS、迭代器每一步、泛用的屬性存取都會計 tick 並檢查預算、中斷與配置量（不做 GC，原生函式持有未 root 的值）。
* BigInt 上限 2²⁰ 位元（與 SpiderMonkey 相同），`asIntN/asUintN` 不再配置整個遮罩，`toString` 快約 9 倍。
* AST：`Stmt` 由 224 降到 112 bytes，Scanner 不快取空的區域（`eval` 200 萬個 `{}` 由 460 MB 降到 232 MB）。

## 每個 op 的底線

switch 直譯器在 wasm 裡每個 op 的分派加最簡單的處理約 50 條指令，`let_loop`（4 個 op）388 條、boa 1,221 條。要到 boa 的 1/10（約 120 條）只能讓每圈的 op 再少（更多超指令）或交給 B 階段：把熱函式編成 wasm，由 wasmtime 編成機器碼。

## 下一步

1. heap snapshot：每頁啟動 ≤ 1,000 萬條指令（#2）。
2. rubrowser 的 CDP `Debugger` 與 `Runtime.getProperties`（M6）。
3. fuzz（parser、RegExp、VM）與剩下的 test262 失敗（M7）。
4. B 階段 JIT 的設計比較（M8）。
