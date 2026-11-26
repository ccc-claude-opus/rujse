#!/bin/sh
# Builds the benchmark scripts into target/bench/:
#   v8-NAME.js  base.js + NAME.js + fixed.js (fixed work, for wasmbench)
#   micro/*.js and prelude.js are used as they are.
set -e
cd "$(dirname "$0")/.."
mkdir -p target/bench
for n in richards deltablue crypto raytrace earley-boyer regexp splay navier-stokes; do
  cat bench/v8-v7/base.js bench/v8-v7/$n.js bench/v8-v7/fixed.js > target/bench/v8-$n.js
done
