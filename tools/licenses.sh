#!/bin/sh
# Lists every crate in the workspace's dependency graph with its license
# (doc/licenses.md). The engine crate itself has no runtime dependencies.
set -e
cd "$(dirname "$0")/.."
{
  echo "# 依賴與授權"
  echo
  echo "由 \`tools/licenses.sh\` 產生。引擎本身（crate \`rujse\`）沒有執行期依賴；下列是開發工具（test262 執行器、wasmbench 與其 boa 基準）用到的 crate。"
  echo
  echo "| crate | 版本 | 授權 |"
  echo "|---|---|---|"
  cargo metadata --format-version 1 --locked 2>/dev/null | python3 -c '
import json, sys
m = json.load(sys.stdin)
ids = {n["id"] for n in m["resolve"]["nodes"]}
ws = set(m["workspace_members"])
for p in sorted(m["packages"], key=lambda p: (p["name"], p["version"])):
    if p["id"] in ids and p["id"] not in ws:
        print("| %s | %s | %s |" % (p["name"], p["version"], p["license"] or "?"))
'
  echo
  echo "## 外部資料"
  echo
  echo "| 資料 | 來源 | 授權 | 用途 |"
  echo "|---|---|---|---|"
  echo "| test262 | github.com/tc39/test262（commit 見 \`tools/test262/COMMIT\`） | BSD-3-Clause | 符合度測試，不提交 |"
  echo "| V8 Benchmark Suite v7 | github.com/mozilla/arewefastyet \`benchmarks/v8-v7\` | BSD-3-Clause（檔頭） | \`bench/v8-v7/\` |"
  echo "| QuickJS | github.com/bellard/quickjs | MIT | 原生效能比較，只在本機建置 |"
  echo "| boa 0.22 | crates.io \`boa_engine\`、\`boa_cli\` | MIT OR Unlicense | 效能基準 |"
} > doc/licenses.md
grep -c '^| ' doc/licenses.md
