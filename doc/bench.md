# 效能量測

量測單位是 **wasm 指令數**（wasmtime 的 fuel，和 rubrowser 的 Performance 面板相同）。引擎都編成 `wasm32-unknown-unknown`（`runtime` profile：`opt-level = 3`、LTO、`panic = "abort"`），由 `tools/wasmbench` 在 wasmtime 50 上執行。

```bash
./bench/build.sh                                   # 產生 target/bench/v8-*.js
cargo build -p wasmbench --release
cargo build -p wasmbench-boa --profile runtime --target wasm32-unknown-unknown
./target/release/wasmbench target/wasm32-unknown-unknown/runtime/wasmbench_boa.wasm \
  -- bench/micro/*.js bench/prelude.js target/bench/v8-*.js
```

* `bench/micro/*.js`：含 `__N__` 的迴圈，各以 N 與 2N（N = 100,000）執行，相減得到**每圈**的指令數。
* `bench/prelude.js`：rubrowser 的 `prelude.js` 接在假的 `__rb` 上，就是每頁 JS 的啟動成本。
* `target/bench/v8-*.js`：V8 Benchmark Suite v7，每個 benchmark 固定跑 3 次（`bench/v8-v7/fixed.js`）。

## boa 0.22 基準（2026-11-26，iMac M3）

模組 6,280,513 bytes，wasmtime 編譯 914 ms，建立 `Context` 140 萬條。

| 微基準 | 每圈 wasm 指令 |
|---|---|
| `loop`（全域 `var x += k`） | 12,048 |
| `let_loop`（函式內 `let`） | 1,221 |
| `prop`（`p.x = p.x + p.y`） | 2,963 |
| `call`（呼叫 `add(s, 1)`） | 4,792 |
| `closure`（呼叫閉包 `++c`） | 4,231 |
| `method`（`p.inc()`） | 5,090 |
| `array`（`push` 後索引加總，每圈兩者各一次） | 7,912 |
| `alloc`（建立一個兩屬性物件） | 7,723 |
| `string`（`s += 'ab'`） | 302,319（隨長度變慢：字串串接是 O(n)） |

| 程式 | wasm 指令 | 時間 |
|---|---|---|
| prelude（rubrowser 每頁） | 483,918,891 | 49 ms |
| v8-richards | 1,139,871,311 | 85 ms |
| v8-deltablue | 2,216,169,502 | 179 ms |
| v8-crypto | 35,977,877,070 | 3,135 ms |
| v8-raytrace | 10,259,939,052 | 959 ms |
| v8-earley-boyer | 40,572,362,165 | 3,819 ms |
| v8-regexp | 159,792,444,912 | 10,786 ms |
| v8-splay | 11,486,250,427 | 1,109 ms |
| v8-navier-stokes | 25,404,117,505 | 1,916 ms |

## rujse 對照（2026-11-26，同一台機器）

建置：`cargo build -p wasmbench-rujse --profile runtime --target wasm32-unknown-unknown`。模組 2,096,600 bytes（boa 的 33%），wasmtime 編譯約 0.3 s（boa 0.9–1.5 s）。

| 微基準（每圈 wasm 指令） | boa | rujse | boa / rujse |
|---|---|---|---|
| `loop`（全域 `var x += k`） | 12,048 | 1,517 | 7.9× |
| `let_loop`（函式內 `let`） | 1,221 | 388 | 3.1× |
| `prop`（`p.x = p.x + p.y`） | 2,963 | 856 | 3.5× |
| `call`（呼叫 `add(s, 1)`） | 4,792 | 1,668 | 2.9× |
| `closure`（呼叫閉包 `++c`） | 4,231 | 1,500 | 2.8× |
| `method`（`p.inc()`） | 5,090 | 1,947 | 2.6× |
| `array`（`push` 後索引加總） | 7,912 | 2,556 | 3.1× |
| `alloc`（建立一個兩屬性物件） | 7,723 | 1,836 | 4.2× |
| `string`（`s += 'ab'`） | 302,319（O(n)） | 1,441（rope，O(1)） | — |

| 每頁啟動（prelude） | wasm 指令 |
|---|---|
| boa | 483,918,891 |
| rujse，原始碼 | 211,588,147 |
| rujse，bytecode cache（`prelude.rjbc`，函式本體延遲解碼） | 30,583,621 |

rubrowser 的 `js-rujse.wasm` 就是用 bytecode cache 載入 prelude（`scripts/build.sh` 產生 `src/lang/prelude.rjbc`）。

### 目前用到的技術

* **暫存器 + 累加器的 bytecode**，peephole 融合成超指令：`BinRR`（`Ldar; 運算; Star`）、`CmpJmp`（比較加條件跳躍）、`IncReg`/`DecReg`、`Mov`（`Ldar; Star`，後面的指令會覆寫累加器時）。
* **TDZ 省略**：宣告之後才讀取的 `let`/`const` 不做 TDZ 檢查。
* **inline cache**（命中前都先驗證）：
  * 全域：script 層 `let`/`const` 的 cell，或全域物件的資料屬性（dictionary 的 entry 或 shape 的 slot）；新增全域 lexical 綁定時遞增 epoch，讓可能被遮蔽的快取失效。
  * 具名屬性（每個存取點一格）：自有資料屬性（shape + slot）、直接原型上的資料屬性（接收者 shape、原型身分與原型 shape 都要相同），以及物件字面值與欄位新增屬性時的 shape 轉移。
* **數字快路徑**：int32 運算與比較就地完成；兩個運算元都是數字時走 double 路徑；慢路徑放在不內聯的函式裡。
* **字串 rope**：長度 ≥ 32 的串接建立 rope 節點，第一次讀取時以迭代方式攤平；GC 不再追蹤已攤平 rope 的子節點。
* **bytecode cache**：編好的 Code 樹序列化（字串表去重、每個函式本體以長度前綴的區塊存放），載入時只讀標頭，函式第一次執行時才解碼本體。

### 每個 op 的底線成本

switch 直譯器在 wasm 裡每個 op 的分派（取指令、解碼、`br_table`、回到迴圈頂）大約 25–30 條指令，最簡單的 op（`Ldar`、`Star`）合計約 50 條，`let_loop` 的 4 個 op 就是 388 條。要再往 boa 的 1/10（每圈約 120 條）靠近，A 階段只能繼續減少 op 數（更多超指令），根本的解法是 B 階段：把熱函式編成 wasm。
