#!/usr/bin/env python3
"""Parse powermetrics text output + phase logs into a per-phase baseline report.

Sample-to-phase mapping: powermetrics sample k (0-based) covers
[launch_epoch + k*interval, launch_epoch + (k+1)*interval]; the midpoint is
matched against phase intervals from phases.log.
"""
import argparse
import glob
import json
import os
import re
import statistics
import sys
from collections import defaultdict

OUT = "/Users/acfufu/Codehub/alas-launcher/probe/out"
INTERVAL_S = 2.0

SAMPLE_RE = re.compile(r"\*\*\* Sampled system activity .*? \(([\d.]+)ms elapsed\) \*\*\*")
POWER_RE = re.compile(r"^\s*([A-Za-z0-9 +/()+-]+?)\s+Power:\s+([\d.]+)\s+mW", re.M)
QEMU_GPU_LINE_RE = re.compile(r"qemu-system-aarch64.*?([\d.]+)\s*$")

KEYS_WANT = [
    "E-Cluster", "P-Cluster", "P0-Cluster", "P1-Cluster",
    "CPU", "GPU", "ANE", "DRAM", "GPU SRAM",
    "CPU + GPU + ANE", "System Average",
]


def parse_pm(path):
    text = open(path, encoding="utf-8", errors="replace").read()
    parts = SAMPLE_RE.split(text)
    # parts: [pre, elapsed1, body1, elapsed2, body2, ...]
    samples = []
    for i in range(1, len(parts), 2):
        body = parts[i + 1]
        powers = {}
        for name, val in POWER_RE.findall(body):
            name = name.strip()
            if name not in powers:
                powers[name] = float(val)
        qemu_gpu = None
        m = QEMU_GPU_LINE_RE.search(body)
        if m:
            qemu_gpu = float(m.group(1))
        samples.append({"idx": len(samples), "powers": powers, "qemu_gpu_ms": qemu_gpu})
    return samples


def parse_phases(path):
    intervals = {}
    open_phase = None
    start = None
    for line in open(path, encoding="utf-8"):
        m = re.match(r"([\d.]+) PHASE_(START|END) (\S+)", line)
        if not m:
            continue
        t, kind, name = float(m.group(1)), m.group(2), m.group(3)
        if kind == "START":
            open_phase, start = name, t
        elif kind == "END" and open_phase == name:
            intervals[name] = (start, t)
            open_phase = None
    return intervals


def parse_cpu_samples(path):
    rows = []
    for line in open(path, encoding="utf-8"):
        m = re.match(r"([\d.]+) qemu=([\d.]+|NA)", line)
        if m:
            rows.append((float(m.group(1)), m.group(2)))
    return rows


def phase_of(t, intervals):
    for name, (s, e) in intervals.items():
        if s <= t < e:
            return name
    return None


def fmt_w(v):
    return f"{v:7.0f}" if v is not None else "     --"


def main():
    ap = argparse.ArgumentParser(description="Parse powermetrics + phase logs into a per-phase report")
    ap.add_argument("--pm", default=os.path.join(OUT, "pm.txt"))
    ap.add_argument("--phases", default=os.path.join(OUT, "phases.log"))
    ap.add_argument("--cpu-log", default=os.path.join(OUT, "cpu_samples.log"))
    ap.add_argument("--meta", default=os.path.join(OUT, "pm_meta.log"))
    ap.add_argument("--bench-glob", default=os.path.join(OUT, "bench_P*.json"))
    ap.add_argument("--report", default=os.path.join(OUT, "baseline_report.md"))
    ap.add_argument("--title", default="ALAS macOS 功耗基线报告（M5 Air / Azurelane 5554 / 2026-10-27）")
    args = ap.parse_args()

    samples = parse_pm(args.pm)
    intervals = parse_phases(args.phases)
    cpu_rows = parse_cpu_samples(args.cpu_log)
    launch = None
    for line in open(args.meta):
        if line.startswith("PM_LAUNCH_EPOCH"):
            launch = float(line.split()[1])
    if launch is None or not samples:
        print("missing pm_meta.log launch epoch or no samples parsed", file=sys.stderr)
        sys.exit(1)

    phase_names = [n for n, _ in sorted(intervals.items(), key=lambda kv: kv[1][0])]

    per_phase = defaultdict(lambda: defaultdict(list))
    qemu_gpu_per_phase = defaultdict(list)
    unmatched = 0
    for s in samples:
        mid = launch + (s["idx"] + 0.5) * INTERVAL_S
        ph = phase_of(mid, intervals)
        if ph is None:
            unmatched += 1
            continue
        for k, v in s["powers"].items():
            per_phase[ph][k].append(v)
        if s["qemu_gpu_ms"] is not None:
            qemu_gpu_per_phase[ph].append(s["qemu_gpu_ms"])

    qemu_cpu_per_phase = defaultdict(list)
    for t, v in cpu_rows:
        ph = phase_of(t, intervals)
        if ph is not None and v != "NA":
            qemu_cpu_per_phase[ph].append(float(v))

    # Which power keys exist (prefer short names when both exist)
    all_keys = set()
    for ph in per_phase:
        all_keys |= set(per_phase[ph].keys())
    ordered_keys = [k for k in all_keys if any(w in k for w in KEYS_WANT)]
    ordered_keys.sort(key=lambda k: (0 if "Cluster" in k else 1, k))

    benches = {}
    for f in sorted(glob.glob(args.bench_glob)):
        try:
            benches[os.path.basename(f)] = json.load(open(f))
        except Exception as e:  # noqa: BLE001
            benches[os.path.basename(f)] = {"error": str(e)}

    lines = []
    lines.append(f"# {args.title}\n")
    lines.append(f"- powermetrics samples: {len(samples)}，phase-unmatched: {unmatched}")
    lines.append(f"- phases: {phase_names}\n")

    lines.append("## 各相位功耗（mW，median）\n")
    header = "| phase | " + " | ".join(ordered_keys) + " | qemu CPU% | qemu GPU ms/s |"
    lines.append(header)
    lines.append("|" + "---|" * (len(ordered_keys) + 3))
    base = per_phase.get("P0_idle", {})
    for ph in phase_names:
        vals = []
        for k in ordered_keys:
            arr = per_phase[ph].get(k)
            vals.append(fmt_w(statistics.median(arr)) if arr else "     --")
        qcpu = statistics.mean(qemu_cpu_per_phase[ph]) if qemu_cpu_per_phase[ph] else None
        qgpu = statistics.median(qemu_gpu_per_phase[ph]) if qemu_gpu_per_phase[ph] else None
        qcpu_s = f"{qcpu:.0f}" if qcpu is not None else "--"
        qgpu_s = f"{qgpu:.1f}" if qgpu is not None else "--"
        lines.append(f"| {ph} | " + " | ".join(vals) + f" | {qcpu_s} | {qgpu_s} |")

    lines.append("\n## 各相位相对 idle 的增量（mW，median）\n")
    header2 = "| phase | " + " | ".join("Δ" + k for k in ordered_keys) + " |"
    lines.append(header2)
    lines.append("|" + "---|" * (len(ordered_keys) + 1))
    for ph in phase_names:
        if ph == "P0_idle":
            continue
        vals = []
        for k in ordered_keys:
            a, b = per_phase[ph].get(k), base.get(k)
            if a and b:
                vals.append(fmt_w(statistics.median(a) - statistics.median(b)))
            else:
                vals.append("     --")
        lines.append(f"| {ph} | " + " | ".join(vals) + " |")

    lines.append("\n## 微基准吞吐\n")
    for name, b in benches.items():
        lines.append(f"- `{name}`: {json.dumps(b, ensure_ascii=False)}")

    report = "\n".join(lines) + "\n"
    with open(args.report, "w") as fp:
        fp.write(report)
    print(report)


if __name__ == "__main__":
    main()
