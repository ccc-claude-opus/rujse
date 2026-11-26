# 依賴與授權

由 `tools/licenses.sh` 產生。引擎本身（crate `rujse`）沒有執行期依賴；下列是開發工具（test262 執行器、wasmbench 與其 boa 基準）用到的 crate。

| crate | 版本 | 授權 |
|---|---|---|
| addr2line | 0.26.1 | Apache-2.0 OR MIT |
| aligned-vec | 0.6.4 | MIT |
| allocator-api2 | 0.2.21 | MIT OR Apache-2.0 |
| anyhow | 1.0.105 | MIT OR Apache-2.0 |
| arbitrary | 1.5.0 | MIT OR Apache-2.0 |
| arrayvec | 0.7.8 | MIT OR Apache-2.0 |
| async-channel | 2.5.0 | Apache-2.0 OR MIT |
| async-trait | 0.1.92 | MIT OR Apache-2.0 |
| autocfg | 1.5.1 | Apache-2.0 OR MIT |
| bitflags | 2.13.2 | MIT OR Apache-2.0 |
| block-buffer | 0.10.4 | MIT OR Apache-2.0 |
| boa_ast | 0.22.0 | Unlicense OR MIT |
| boa_engine | 0.22.0 | Unlicense OR MIT |
| boa_gc | 0.22.0 | Unlicense OR MIT |
| boa_interner | 0.22.0 | Unlicense OR MIT |
| boa_macros | 0.22.0 | Unlicense OR MIT |
| boa_parser | 0.22.0 | Unlicense OR MIT |
| boa_string | 0.22.0 | Unlicense OR MIT |
| bumpalo | 3.20.4 | MIT OR Apache-2.0 |
| bytemuck | 1.25.2 | Zlib OR Apache-2.0 OR MIT |
| bytemuck_derive | 1.12.1 | Zlib OR Apache-2.0 OR MIT |
| cc | 1.11.3 | MIT OR Apache-2.0 |
| cfg-if | 1.0.5 | MIT OR Apache-2.0 |
| chacha20 | 0.10.2 | MIT OR Apache-2.0 |
| cobs | 0.3.0 | MIT OR Apache-2.0 |
| concurrent-queue | 2.5.0 | Apache-2.0 OR MIT |
| cow-utils | 0.1.3 | MIT |
| cpp_demangle | 0.5.1 | MIT OR Apache-2.0 |
| cpufeatures | 0.2.17 | MIT OR Apache-2.0 |
| cpufeatures | 0.3.1 | MIT OR Apache-2.0 |
| cranelift-assembler-x64 | 0.137.3 | Apache-2.0 WITH LLVM-exception |
| cranelift-assembler-x64-meta | 0.137.3 | Apache-2.0 WITH LLVM-exception |
| cranelift-bforest | 0.137.3 | Apache-2.0 WITH LLVM-exception |
| cranelift-bitset | 0.137.3 | Apache-2.0 WITH LLVM-exception |
| cranelift-codegen | 0.137.3 | Apache-2.0 WITH LLVM-exception |
| cranelift-codegen-meta | 0.137.3 | Apache-2.0 WITH LLVM-exception |
| cranelift-codegen-shared | 0.137.3 | Apache-2.0 WITH LLVM-exception |
| cranelift-control | 0.137.3 | Apache-2.0 WITH LLVM-exception |
| cranelift-entity | 0.137.3 | Apache-2.0 WITH LLVM-exception |
| cranelift-frontend | 0.137.3 | Apache-2.0 WITH LLVM-exception |
| cranelift-isle | 0.137.3 | Apache-2.0 WITH LLVM-exception |
| cranelift-native | 0.137.3 | Apache-2.0 WITH LLVM-exception |
| cranelift-srcgen | 0.137.3 | Apache-2.0 WITH LLVM-exception |
| crc32fast | 1.5.6 | MIT OR Apache-2.0 |
| crossbeam-deque | 0.8.8 | MIT OR Apache-2.0 |
| crossbeam-epoch | 0.9.21 | MIT OR Apache-2.0 |
| crossbeam-utils | 0.8.23 | MIT OR Apache-2.0 |
| crypto-common | 0.1.7 | MIT OR Apache-2.0 |
| dashmap | 6.2.1 | MIT |
| deranged | 0.5.9 | MIT OR Apache-2.0 |
| digest | 0.10.7 | MIT OR Apache-2.0 |
| displaydoc | 0.2.7 | MIT OR Apache-2.0 |
| dynify | 0.1.2 | MIT OR Apache-2.0 |
| dynify-macros | 0.1.2 | MIT OR Apache-2.0 |
| either | 1.19.0 | MIT OR Apache-2.0 |
| embedded-io | 0.4.0 | MIT OR Apache-2.0 |
| embedded-io | 0.6.1 | MIT OR Apache-2.0 |
| equator | 0.4.2 | MIT |
| equator-macro | 0.4.2 | MIT |
| equivalent | 1.0.3 | Apache-2.0 OR MIT |
| errno | 0.3.15 | MIT OR Apache-2.0 |
| event-listener | 5.4.2 | Apache-2.0 OR MIT |
| event-listener-strategy | 0.5.4 | Apache-2.0 OR MIT |
| fast-float2 | 0.2.4 | MIT OR Apache-2.0 |
| fastrand | 2.5.0 | Apache-2.0 OR MIT |
| find-msvc-tools | 0.1.15 | MIT OR Apache-2.0 |
| fixedbitset | 0.5.9 | MIT OR Apache-2.0 |
| fnv | 1.0.7 | Apache-2.0 / MIT |
| foldhash | 0.2.0 | Zlib |
| futures | 0.3.34 | MIT OR Apache-2.0 |
| futures-channel | 0.3.34 | MIT OR Apache-2.0 |
| futures-concurrency | 7.7.1 | MIT OR Apache-2.0 |
| futures-core | 0.3.34 | MIT OR Apache-2.0 |
| futures-io | 0.3.34 | MIT OR Apache-2.0 |
| futures-lite | 2.6.1 | Apache-2.0 OR MIT |
| futures-sink | 0.3.34 | MIT OR Apache-2.0 |
| futures-task | 0.3.34 | MIT OR Apache-2.0 |
| futures-util | 0.3.34 | MIT OR Apache-2.0 |
| generic-array | 0.14.7 | MIT |
| getrandom | 0.3.4 | MIT OR Apache-2.0 |
| getrandom | 0.4.3 | MIT OR Apache-2.0 |
| gimli | 0.33.0 | MIT OR Apache-2.0 |
| hashbrown | 0.14.5 | MIT OR Apache-2.0 |
| hashbrown | 0.16.1 | MIT OR Apache-2.0 |
| hashbrown | 0.17.2 | MIT OR Apache-2.0 |
| heck | 0.5.0 | MIT OR Apache-2.0 |
| icu_collections | 2.3.0 | Unicode-3.0 |
| icu_locale_core | 2.4.0 | Unicode-3.0 |
| icu_normalizer | 2.3.0 | Unicode-3.0 |
| icu_normalizer_data | 2.3.0 | Unicode-3.0 |
| icu_properties | 2.3.0 | Unicode-3.0 |
| icu_properties_data | 2.3.0 | Unicode-3.0 |
| icu_provider | 2.4.0 | Unicode-3.0 |
| indexmap | 2.14.2 | Apache-2.0 OR MIT |
| intrusive-collections | 0.10.3 | MIT OR Apache-2.0 |
| itertools | 0.14.0 | MIT OR Apache-2.0 |
| itertools | 0.15.0 | MIT OR Apache-2.0 |
| itoa | 1.0.19 | MIT OR Apache-2.0 |
| leb128fmt | 0.1.0 | MIT OR Apache-2.0 |
| libc | 0.2.191 | MIT OR Apache-2.0 |
| libm | 0.2.16 | MIT |
| linux-raw-sys | 0.12.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| litemap | 0.8.3 | Unicode-3.0 |
| lock_api | 0.4.14 | MIT OR Apache-2.0 |
| log | 0.4.35 | MIT OR Apache-2.0 |
| mach2 | 0.6.0 | BSD-2-Clause OR MIT OR Apache-2.0 |
| memchr | 2.8.3 | Unlicense OR MIT |
| memfd | 0.6.6 | MIT OR Apache-2.0 |
| num-bigint | 0.5.1 | MIT OR Apache-2.0 |
| num-conv | 0.2.2 | MIT OR Apache-2.0 |
| num-integer | 0.1.47 | MIT OR Apache-2.0 |
| num-traits | 0.2.19 | MIT OR Apache-2.0 |
| num_enum | 0.7.6 | BSD-3-Clause OR MIT OR Apache-2.0 |
| num_enum_derive | 0.7.6 | BSD-3-Clause OR MIT OR Apache-2.0 |
| num_threads | 0.1.8 | MIT OR Apache-2.0 |
| object | 0.40.0 | Apache-2.0 OR MIT |
| once_cell | 1.21.4 | MIT OR Apache-2.0 |
| oneshot | 0.2.1 | MIT OR Apache-2.0 |
| parking | 2.2.1 | Apache-2.0 OR MIT |
| parking_lot_core | 0.9.12 | MIT OR Apache-2.0 |
| pastey | 0.2.3 | MIT OR Apache-2.0 |
| phf | 0.14.0 | MIT |
| phf_generator | 0.14.0 | MIT |
| phf_macros | 0.14.0 | MIT |
| phf_shared | 0.14.0 | MIT |
| pin-project | 1.2.0 | Apache-2.0 OR MIT |
| pin-project-internal | 1.2.0 | Apache-2.0 OR MIT |
| pin-project-lite | 0.2.18 | Apache-2.0 OR MIT |
| portable-atomic | 1.15.0 | Apache-2.0 OR MIT |
| postcard | 1.2.0 | MIT OR Apache-2.0 |
| potential_utf | 0.1.6 | Unicode-3.0 |
| powerfmt | 0.2.1 | MIT OR Apache-2.0 |
| proc-macro-crate | 3.5.0 | MIT OR Apache-2.0 |
| proc-macro2 | 1.0.107 | MIT OR Apache-2.0 |
| pulley-interpreter | 50.0.3 | Apache-2.0 WITH LLVM-exception |
| pulley-macros | 50.0.3 | Apache-2.0 WITH LLVM-exception |
| quote | 1.0.47 | MIT OR Apache-2.0 |
| r-efi | 5.3.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later |
| r-efi | 6.0.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later |
| rand | 0.10.4 | MIT OR Apache-2.0 |
| rand_core | 0.10.1 | MIT OR Apache-2.0 |
| rayon | 1.12.0 | MIT OR Apache-2.0 |
| rayon-core | 1.13.0 | MIT OR Apache-2.0 |
| redox_syscall | 0.5.18 | MIT |
| regalloc2 | 0.15.3 | Apache-2.0 WITH LLVM-exception |
| regress | 0.12.0 | MIT OR Apache-2.0 |
| rustc-demangle | 0.1.28 | MIT/Apache-2.0 |
| rustc-hash | 2.1.3 | Apache-2.0 OR MIT |
| rustix | 1.1.6 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| rustversion | 1.0.23 | MIT OR Apache-2.0 |
| ryu-js | 1.0.4 | Apache-2.0 OR BSL-1.0 |
| scopeguard | 1.2.0 | MIT OR Apache-2.0 |
| semver | 1.0.28 | MIT OR Apache-2.0 |
| serde | 1.0.229 | MIT OR Apache-2.0 |
| serde_core | 1.0.229 | MIT OR Apache-2.0 |
| serde_derive | 1.0.229 | MIT OR Apache-2.0 |
| serde_json | 1.0.154 | MIT OR Apache-2.0 |
| sha2 | 0.10.9 | MIT OR Apache-2.0 |
| shlex | 2.0.1 | MIT OR Apache-2.0 |
| siphasher | 1.0.4 | MIT OR Apache-2.0 |
| slab | 0.4.12 | MIT |
| small_btree | 0.1.0 | Unlicense OR MIT |
| smallvec | 1.16.6 | MIT OR Apache-2.0 |
| stable_deref_trait | 1.2.1 | MIT OR Apache-2.0 |
| static_assertions | 1.1.0 | MIT OR Apache-2.0 |
| strum | 0.28.0 | MIT |
| strum_macros | 0.28.0 | MIT |
| syn | 2.0.119 | MIT OR Apache-2.0 |
| syn | 3.0.10 | MIT OR Apache-2.0 |
| synstructure | 0.14.0 | MIT |
| tag_ptr | 0.1.0 | Unlicense OR MIT |
| tap | 1.0.1 | MIT |
| target-lexicon | 0.13.6 | Apache-2.0 WITH LLVM-exception |
| termcolor | 1.4.1 | Unlicense OR MIT |
| thin-vec | 0.2.21 | MIT OR Apache-2.0 |
| thiserror | 2.0.21 | MIT OR Apache-2.0 |
| thiserror-impl | 2.0.21 | MIT OR Apache-2.0 |
| time | 0.3.55 | MIT OR Apache-2.0 |
| time-core | 0.1.9 | MIT OR Apache-2.0 |
| time-macros | 0.2.32 | MIT OR Apache-2.0 |
| tinystr | 0.8.5 | Unicode-3.0 |
| toml_datetime | 1.1.3+spec-1.1.0 | MIT OR Apache-2.0 |
| toml_edit | 0.25.21+spec-1.1.0 | MIT OR Apache-2.0 |
| toml_parser | 1.1.7+spec-1.1.0 | MIT OR Apache-2.0 |
| typenum | 1.20.1 | MIT OR Apache-2.0 |
| unicode-ident | 1.0.27 | (MIT OR Apache-2.0) AND Unicode-3.0 |
| utf16_iter | 1.1.0 | Apache-2.0 OR MIT |
| utf8_iter | 1.1.0 | Apache-2.0 OR MIT |
| utf_types | 1.0.0 | Apache-2.0 OR MIT |
| version_check | 0.9.5 | MIT/Apache-2.0 |
| wasip2 | 1.0.4+wasi-0.2.12 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| wasm-encoder | 0.261.0 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| wasmparser | 0.261.0 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| wasmprinter | 0.261.0 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| wasmtime | 50.0.3 | Apache-2.0 WITH LLVM-exception |
| wasmtime-environ | 50.0.3 | Apache-2.0 WITH LLVM-exception |
| wasmtime-internal-component-util | 50.0.3 | Apache-2.0 WITH LLVM-exception |
| wasmtime-internal-core | 50.0.3 | Apache-2.0 WITH LLVM-exception |
| wasmtime-internal-cranelift | 50.0.3 | Apache-2.0 WITH LLVM-exception |
| wasmtime-internal-fiber | 50.0.3 | Apache-2.0 WITH LLVM-exception |
| wasmtime-internal-jit-debug | 50.0.3 | Apache-2.0 WITH LLVM-exception |
| wasmtime-internal-jit-icache-coherence | 50.0.3 | Apache-2.0 WITH LLVM-exception |
| wasmtime-internal-unwinder | 50.0.3 | Apache-2.0 WITH LLVM-exception |
| wasmtime-internal-versioned-export-macros | 50.0.3 | Apache-2.0 WITH LLVM-exception |
| winapi-util | 0.1.11 | Unlicense OR MIT |
| windows-link | 0.2.1 | MIT OR Apache-2.0 |
| windows-sys | 0.61.2 | MIT OR Apache-2.0 |
| winnow | 1.0.5 | MIT |
| wit-bindgen | 0.57.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| write16 | 1.0.0 | Apache-2.0 OR MIT |
| writeable | 0.6.4 | Unicode-3.0 |
| yoke | 0.8.4 | Unicode-3.0 |
| yoke-derive | 0.8.4 | Unicode-3.0 |
| zerofrom | 0.1.8 | Unicode-3.0 |
| zerofrom-derive | 0.1.8 | Unicode-3.0 |
| zerotrie | 0.2.5 | Unicode-3.0 |
| zerovec | 0.11.9 | Unicode-3.0 |
| zerovec-derive | 0.11.7 | Unicode-3.0 |
| zmij | 1.0.23 | MIT |

## 外部資料

| 資料 | 來源 | 授權 | 用途 |
|---|---|---|---|
| test262 | github.com/tc39/test262（commit 見 `tools/test262/COMMIT`） | BSD-3-Clause | 符合度測試，不提交 |
| V8 Benchmark Suite v7 | github.com/mozilla/arewefastyet `benchmarks/v8-v7` | BSD-3-Clause（檔頭） | `bench/v8-v7/` |
| QuickJS | github.com/bellard/quickjs | MIT | 原生效能比較，只在本機建置 |
| boa 0.22 | crates.io `boa_engine`、`boa_cli` | MIT OR Unlicense | 效能基準 |
