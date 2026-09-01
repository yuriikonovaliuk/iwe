#!/usr/bin/env python3
"""AB7 per-save latency verification (T8).

Independently measures whether M2's changes keep per-save latency overhead
within AB7's 50ms regression budget:

    "a per-save baseline is measured on the current single store before
    migration, and after migration per-save overhead adds no more than
    50ms over that baseline."

This is a black-box A/B comparison between two *binaries* built from two
different commits (pre-M2 baseline vs. M2-extended), not an in-process
criterion bench — criterion benches one checkout at a time, and what AB7
asks us to compare is two checkouts. Each "save" is one real, complete
`iwe update -k <key> --set <field>=<value>` invocation against a real
document drawn from a realistic corpus: process start, config + graph
load, apply the frontmatter mutation, write the file, process exit. Since
per-save *overhead* is what's budgeted, and the load path is untouched by
M2 (M2 only extends canonical write paths with a permission/transaction
check), the wall-clock delta between the two binaries on the same corpus,
same op, isolates the write-path delta: shared costs (process start,
config parse, graph load, argument parsing) appear in both arms and
cancel in the paired difference.

Methodology:
  - N distinct document keys are sampled (without replacement, seeded) from
    a real corpus.
  - For each key, a *pair* of trials runs: one with the baseline binary,
    one with the M2 binary, in randomized order (guards against ordering
    bias e.g. warm vs. cold OS caches). Before each individual trial the
    target file is reset to its pristine content read fresh from the
    corpus, so every save starts from identical byte-for-byte input
    regardless of which binary or trial ran before it.
  - Pairs execute back-to-back in randomized order across the whole run
    (not sequential all-A-then-all-B blocks), so any transient shared-load
    contention affects both arms roughly evenly instead of skewing one arm's
    block.
  - A handful of untimed warm-up invocations run first for each binary.

Usage:
    python3 ab7_save_latency.py \\
        --baseline-bin /path/to/pre-m2/target/release/iwe \\
        --m2-bin /path/to/m2/target/release/iwe \\
        --corpus /path/to/real/corpus \\
        --work-dir /path/to/scratch/copy/of/corpus \\
        --pairs 300 --seed 42 --csv-out results.csv
"""

import argparse
import csv
import random
import shutil
import statistics
import subprocess
import sys
import time
from pathlib import Path


def discover_keys(corpus: Path) -> list[str]:
    keys = []
    for p in corpus.rglob("*.md"):
        rel = p.relative_to(corpus)
        keys.append(str(rel)[: -len(".md")])
    keys.sort()
    return keys


def reset_file(corpus: Path, work_dir: Path, key: str) -> None:
    src = corpus / f"{key}.md"
    dst = work_dir / f"{key}.md"
    dst.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(src, dst)


def run_save(binary: Path, work_dir: Path, key: str, probe_value: str) -> tuple[float, int]:
    """Runs one `iwe update` save, returns (elapsed_ms, returncode)."""
    cmd = [
        str(binary),
        "update",
        "-k",
        key,
        "--set",
        f"ab7_bench_probe={probe_value}",
    ]
    start = time.perf_counter()
    proc = subprocess.run(
        cmd,
        cwd=work_dir,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    elapsed_ms = (time.perf_counter() - start) * 1000.0
    return elapsed_ms, proc.returncode


def percentile(data: list[float], pct: float) -> float:
    if not data:
        return float("nan")
    s = sorted(data)
    k = (len(s) - 1) * (pct / 100.0)
    f = int(k)
    c = min(f + 1, len(s) - 1)
    if f == c:
        return s[f]
    return s[f] + (s[c] - s[f]) * (k - f)


def summarize(label: str, samples: list[float]) -> dict:
    return {
        "label": label,
        "n": len(samples),
        "mean": statistics.mean(samples) if samples else float("nan"),
        "stdev": statistics.stdev(samples) if len(samples) > 1 else float("nan"),
        "min": min(samples) if samples else float("nan"),
        "p50": percentile(samples, 50),
        "p95": percentile(samples, 95),
        "p99": percentile(samples, 99),
        "max": max(samples) if samples else float("nan"),
    }


def print_summary(s: dict) -> None:
    print(
        f"  {s['label']:<10} n={s['n']:<5} mean={s['mean']:7.2f}ms "
        f"stdev={s['stdev']:6.2f}ms min={s['min']:7.2f}ms "
        f"p50={s['p50']:7.2f}ms p95={s['p95']:7.2f}ms "
        f"p99={s['p99']:7.2f}ms max={s['max']:7.2f}ms"
    )


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--baseline-bin", required=True, type=Path, help="pre-M2 iwe binary (release)")
    ap.add_argument("--m2-bin", required=True, type=Path, help="M2-extended iwe binary (release)")
    ap.add_argument("--corpus", required=True, type=Path, help="read-only source corpus (real docs + .iwe/config.toml)")
    ap.add_argument("--work-dir", required=True, type=Path, help="mutable scratch copy of the corpus; must already exist with .iwe/config.toml")
    ap.add_argument("--pairs", type=int, default=300, help="number of (key, baseline-trial, m2-trial) pairs")
    ap.add_argument("--warmup", type=int, default=5, help="untimed warm-up saves per binary before measuring")
    ap.add_argument("--seed", type=int, default=42, help="RNG seed for key sampling and A/B order randomization")
    ap.add_argument("--budget-ms", type=float, default=50.0, help="AB7 regression budget")
    ap.add_argument("--csv-out", type=Path, default=None, help="optional path to write raw per-trial samples")
    args = ap.parse_args()

    for b, label in ((args.baseline_bin, "baseline"), (args.m2_bin, "m2")):
        if not b.is_file():
            print(f"error: {label} binary not found at {b}", file=sys.stderr)
            return 2

    if not (args.work_dir / ".iwe" / "config.toml").is_file():
        print(f"error: {args.work_dir} does not look like an iwe library (missing .iwe/config.toml)", file=sys.stderr)
        return 2

    rng = random.Random(args.seed)

    all_keys = discover_keys(args.corpus)
    # exclude the special "ab7-warmup" key namespace if present; not expected
    if len(all_keys) < args.pairs:
        print(
            f"error: corpus has only {len(all_keys)} documents, need at least {args.pairs} for --pairs {args.pairs}",
            file=sys.stderr,
        )
        return 2
    sample_keys = rng.sample(all_keys, args.pairs)

    print(f"Corpus: {args.corpus} ({len(all_keys)} documents total)")
    print(f"Sampling {args.pairs} distinct document keys (seed={args.seed})")
    print(f"Baseline binary: {args.baseline_bin}")
    print(f"M2 binary:       {args.m2_bin}")
    print()

    # Warm-up: exercise both binaries a few times on a throwaway key so page
    # cache / binary loading effects aren't concentrated on the first
    # measured trials of either arm.
    warmup_key = all_keys[0]
    for _ in range(args.warmup):
        reset_file(args.corpus, args.work_dir, warmup_key)
        run_save(args.baseline_bin, args.work_dir, warmup_key, "warmup")
        reset_file(args.corpus, args.work_dir, warmup_key)
        run_save(args.m2_bin, args.work_dir, warmup_key, "warmup")
    reset_file(args.corpus, args.work_dir, warmup_key)

    rows = []  # trial_index, key, label, order_position, elapsed_ms, returncode
    errors = []

    binaries = {"baseline": args.baseline_bin, "m2": args.m2_bin}

    for i, key in enumerate(sample_keys):
        order = ["baseline", "m2"]
        if rng.random() < 0.5:
            order.reverse()
        for pos, label in enumerate(order):
            reset_file(args.corpus, args.work_dir, key)
            elapsed_ms, rc = run_save(binaries[label], args.work_dir, key, f"trial{i}")
            rows.append(
                {
                    "trial_index": i,
                    "key": key,
                    "label": label,
                    "order_position": pos,
                    "elapsed_ms": elapsed_ms,
                    "returncode": rc,
                }
            )
            if rc != 0:
                errors.append((i, key, label, rc))

    # restore the work_dir's touched files to pristine
    for key in sample_keys:
        reset_file(args.corpus, args.work_dir, key)
    reset_file(args.corpus, args.work_dir, warmup_key)

    if args.csv_out:
        with open(args.csv_out, "w", newline="") as f:
            w = csv.DictWriter(f, fieldnames=["trial_index", "key", "label", "order_position", "elapsed_ms", "returncode"])
            w.writeheader()
            w.writerows(rows)
        print(f"Raw samples written to {args.csv_out}")

    if errors:
        print(f"\nWARNING: {len(errors)} trial(s) returned non-zero exit code:")
        for i, key, label, rc in errors[:10]:
            print(f"  trial {i} key={key} label={label} rc={rc}")
        if len(errors) > 10:
            print(f"  ... and {len(errors) - 10} more")

    baseline_samples = [r["elapsed_ms"] for r in rows if r["label"] == "baseline" and r["returncode"] == 0]
    m2_samples = [r["elapsed_ms"] for r in rows if r["label"] == "m2" and r["returncode"] == 0]

    # paired per-key delta (m2 - baseline), only for keys where both trials
    # succeeded
    by_key = {}
    for r in rows:
        if r["returncode"] != 0:
            continue
        by_key.setdefault(r["key"], {})[r["label"]] = r["elapsed_ms"]
    paired_deltas = [
        v["m2"] - v["baseline"] for v in by_key.values() if "m2" in v and "baseline" in v
    ]

    print("\nResults (single-invocation wall-clock, includes process start + load + save + exit):")
    base_summary = summarize("baseline", baseline_samples)
    m2_summary = summarize("m2", m2_samples)
    print_summary(base_summary)
    print_summary(m2_summary)

    print("\nPer-save overhead (M2 minus baseline):")
    delta_p50 = m2_summary["p50"] - base_summary["p50"]
    delta_p95 = m2_summary["p95"] - base_summary["p95"]
    delta_mean = m2_summary["mean"] - base_summary["mean"]
    print(f"  delta of means: {delta_mean:+.2f}ms")
    print(f"  delta of p50:   {delta_p50:+.2f}ms")
    print(f"  delta of p95:   {delta_p95:+.2f}ms")

    if paired_deltas:
        pd_summary = summarize("paired-delta", paired_deltas)
        print("\nPaired per-key delta (m2_i - baseline_i, same doc, matched trials):")
        print_summary(pd_summary)
        primary_p50 = pd_summary["p50"]
        primary_p95 = pd_summary["p95"]
        primary_mean = pd_summary["mean"]
    else:
        primary_p50, primary_p95, primary_mean = delta_p50, delta_p95, delta_mean

    print(f"\nAB7 budget: {args.budget_ms:.0f}ms")
    verdict_p50 = "PASS" if primary_p50 <= args.budget_ms else "FAIL"
    verdict_p95 = "PASS" if primary_p95 <= args.budget_ms else "FAIL"
    verdict_mean = "PASS" if primary_mean <= args.budget_ms else "FAIL"
    print(f"  paired median (p50) delta {primary_p50:+.2f}ms vs {args.budget_ms:.0f}ms budget: {verdict_p50}")
    print(f"  paired mean delta        {primary_mean:+.2f}ms vs {args.budget_ms:.0f}ms budget: {verdict_mean}")
    print(f"  paired p95 delta         {primary_p95:+.2f}ms vs {args.budget_ms:.0f}ms budget: {verdict_p95}")

    overall = "PASS" if verdict_p50 == "PASS" else "FAIL"
    print(f"\nAB7 VERDICT (primary metric: paired median per-save delta): {overall}")

    return 0 if overall == "PASS" else 1


if __name__ == "__main__":
    raise SystemExit(main())
