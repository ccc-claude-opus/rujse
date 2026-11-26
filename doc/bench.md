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
