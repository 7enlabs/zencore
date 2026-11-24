#!/bin/bash
# Regression test for the bench harness (issue #332).
#
# `bash -n` alone does NOT catch a truncation like the one at
# bench.sh:40: the dangling `${` swallows the following lines up to the
# next `}` and still parses, failing only at run time with a bad
# substitution. So this test executes the harness through its built-in
# self-test (BENCH_SELFTEST=1), which needs no model, no network and no
# zen binary - only git, python3 and node. It covers:
#   1. every shell file the harness sources still parses;
#   2. the default task set (no positional args -> BENCH_SET base) runs,
#      proving TASKS falls back to ALL_TASKS;
#   3. explicit task args still win over the default set.
# The r8-hard set is NOT run here: its rs/go tasks need cargo and go,
# which the CI check job does not install.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

fail=0

for f in tools/bench.sh tools/bench_tasks_r8.sh tools/bench_tasks_r8hard.sh tools/r8hard/*.sh; do
  bash -n "$f" || fail=1
done

BENCH_SELFTEST=1 bash tools/bench.sh /tmp/bench-selftest.json || fail=1
BENCH_SELFTEST=1 bash tools/bench.sh /tmp/bench-selftest.json fix_failing_test || fail=1

exit $fail
