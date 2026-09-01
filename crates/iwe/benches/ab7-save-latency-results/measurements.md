# AB7 per-save latency — independent verification (T8, M2)

Verifies the regression-budget half of AB7:

> a per-save baseline is measured on the current single store before
> migration, and after migration per-save overhead adds no more than
> 50ms over that baseline.

(The second half of AB7 — the materialize-under-5s tolerance — is M4's
concern and out of scope here.)

This document and the script that produced it (`../ab7_save_latency.py`)
were built directly from the AB7 requirement text handed down by
Task-scoping, without reading the M2 write-path implementation
(`crates/liwe/src/transaction.rs` and friends were not opened). The
measurement treats `iwe` as a black box: two release binaries, invoked
as subprocesses, timed end-to-end.

## Binaries under test

- **baseline** — pre-M2, commit `bfbca7f655a08f86654c70b04407c1e970c33ed7`
  ("Reconcile with upstream 0.23.0: fork operators join the query
  schema"). This is `git merge-base knowledge-compositor-m2-t8-dev
  master`, and also local `master`'s own tip — i.e. the point M2 branched
  from, containing no M2 work.
- **m2** — `07c6ba4` (`knowledge-compositor-m2-t8-dev`, the T8 Developer
  branch tip this verification worktree was cut from), built at this
  worktree's own tip.

Both built with `cargo build --release -p iwe`.

## Methodology

**Operation measured.** One `iwe update -k <key> --set
ab7_bench_probe=<value>` per trial — a complete, real save: process
start, `.iwe/config.toml` + graph load, apply a frontmatter mutation,
write the file to disk, process exit. This is the CLI's canonical
single-document write path and goes through the same commit-time schema
validation / write-permission machinery any other write path in M2 does.

**Why whole-invocation wall time isolates the write-path delta.** AB7
budgets *per-save overhead*, not per-save total time. M2 only extends
write paths (permission checks / transaction routing) — it doesn't touch
document loading. Load cost, process-start cost, and argument-parsing
cost are therefore common to both binaries on the same corpus and cancel
out in the paired difference. What's left in `m2_i - baseline_i` is
(to first order) the write-path delta AB7 budgets.

**Corpus.** The real IWE knowledge-graph testbed at
`/home/yurii/projects/iwe-memory-testbed` (1,066 real markdown documents,
the effort's own graph — schema-validated frontmatter, inclusion/reference
links, varied section/paragraph counts), not synthetic content. Copied
once into a scratch working directory; the *original* testbed is never
written to — every trial resets its target file by re-reading it fresh
from the read-only original before running, so writes never accumulate
and the original corpus is left untouched throughout.

**Sampling.** 300 distinct document keys, sampled without replacement
(seeded for reproducibility) out of the 1,066. Each key gets a *paired*
trial: one baseline run, one m2 run, in randomized order (guards against
first-run/second-run bias e.g. OS page-cache warmth). Before each
individual trial (not just each pair) the target file is reset to
pristine content, so every single save — baseline or m2, first or second
in its pair — starts from byte-identical input.

**Interleaving.** Pairs run back-to-back in their own randomized
baseline/m2 order across the whole 300-pair sequence — not as two
sequential blocks (run all baseline, then all m2) — so a transient spike
in shared-machine load lands on both arms rather than skewing whichever
block happened to run during it. `uptime` was checked before each run:
load average was 4.3–12.7 (16-core box, shared), i.e. materially loaded
but not idle, which is exactly the condition the interleaved design
guards against.

**Warm-up.** 10 untimed saves per binary against a throwaway key before
the timed run, so binary-load / cache-warming costs aren't concentrated
in the first few timed trials of either arm.

**Statistics.** p50/p95/p99, mean, stdev per arm, plus the *paired*
per-key delta (`m2_i - baseline_i`) — the more direct estimate of
per-save overhead added, since it removes any drift between arms visited
at different points in the run.

The full harness: [`../ab7_save_latency.py`](../ab7_save_latency.py).
Re-run with:

```bash
python3 crates/iwe/benches/ab7_save_latency.py \
  --baseline-bin /path/to/pre-m2/target/release/iwe \
  --m2-bin /path/to/m2/target/release/iwe \
  --corpus /home/yurii/projects/iwe-memory-testbed \
  --work-dir /path/to/scratch/copy/of/corpus \
  --pairs 300 --seed 42
```

## Results

Two independent runs (different seeds, different sample of 300 keys
each) to check the figure is stable rather than a sampling artifact.

### Run 1 (seed 42) — raw samples: [`seed42.csv`](seed42.csv)

Machine load at start: `load average: 12.67, 8.83, 7.98` (16 cores).

| | n | mean | stdev | p50 | p95 | p99 | max |
|---|---|---|---|---|---|---|---|
| baseline | 300 | 40.14ms | 4.95ms | 39.29ms | 45.24ms | 51.51ms | 107.86ms |
| m2 | 300 | 59.41ms | 4.78ms | 59.17ms | 65.19ms | 69.34ms | 76.45ms |

Paired delta (`m2_i - baseline_i`, matched per key): mean **+19.27ms**,
p50 **+20.07ms**, p95 **+24.49ms**, p99 **+26.99ms**, min −31.41ms, max
+31.66ms (the negative min is a single trial where baseline itself hit a
load spike — the paired design's tails still hold well clear of budget).

### Run 2 (seed 7) — raw samples: [`seed7.csv`](seed7.csv)

Machine load at start: `load average: 10.35, 8.80, 8.01`.

| | n | mean | stdev | p50 | p95 | p99 | max |
|---|---|---|---|---|---|---|---|
| baseline | 300 | 40.15ms | 3.43ms | 39.38ms | 46.17ms | 50.68ms | 68.75ms |
| m2 | 300 | 60.28ms | 6.95ms | 59.37ms | 66.99ms | 73.75ms | 142.44ms |

Paired delta: mean **+20.13ms**, p50 **+20.18ms**, p95 **+25.17ms**, p99
**+27.52ms**, min −3.91ms, max +92.26ms (again a single outlier trial
under load; p99 stays at +27.52ms).

Both runs: 300/300 trials succeeded on both binaries (no non-zero exit
codes) in both runs — 1,200 `iwe update` invocations total, zero
failures.

## Verdict

| Metric (paired delta) | Run 1 (seed 42) | Run 2 (seed 7) | Budget | Verdict |
|---|---|---|---|---|
| p50 | +20.07ms | +20.18ms | 50ms | **PASS** |
| mean | +19.27ms | +20.13ms | 50ms | **PASS** |
| p95 | +24.49ms | +25.17ms | 50ms | **PASS** |
| p99 | +26.99ms | +27.52ms | 50ms | **PASS** |

Per-save overhead added by M2 is consistently **~19–20ms at the median**,
reproducible across two independently sampled 300-pair runs on a loaded
shared machine, with the tail (p99) still at roughly half the 50ms
budget and comfortable headroom (~23–30ms) even against the worst
commonly observed case.

**AB7 (regression-budget half) — PASS.** M2's per-save overhead
(measured end-to-end via `iwe update`, the CLI's canonical single-document
write path) stays well within the 50ms ceiling, on the real project
graph, under realistic (non-idle) machine load.

## What this does not cover

- The materialize-under-5s tolerance (AB7's second criterion) — M4's
  concern, ratified against the first real materialize, out of scope for
  M2.
- Save paths other than `iwe update` (e.g. `create`, `delete`, `extract`,
  `inline`, or the LSP/MCP incremental-edit paths in `iwes`/`iwec`) —
  `update` was chosen as a representative, realistic single-document
  write that exercises the same commit-time validation and write-path
  machinery those paths share; a full census of every write path was out
  of scope for one task's budget-verification pass.
