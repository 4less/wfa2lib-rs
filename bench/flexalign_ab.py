#!/usr/bin/env python3
"""A/B benchmark driver for the flexalign-shaped workloads.

Runs a candidate binary against the current best binary, alternating, pinned
to one core, and keeps the minimum ns/pair per workload for each side. Appends
one record to the history (JSONL) and regenerates the HTML report.

Usage:
  bench/flexalign_ab.py --label "v3: foo" --candidate PATH [--baseline PATH]
                        [--rounds 3] [--reps 3] [--note TEXT] [--status taken|dropped|failed]
  bench/flexalign_ab.py --report-only
"""
import argparse
import json
import os
import subprocess
import sys
import time
from datetime import datetime

HERE = os.path.dirname(os.path.abspath(__file__))
HISTORY = os.path.join(HERE, "flexalign_history.jsonl")
REPORT = os.path.join(HERE, "flexalign_report.html")
CORE = os.environ.get("BENCH_CORE", "4")

# Weights for the headline "flexalign speed" number: geometric mean over the
# workloads flexalign actually hits most (short-read end2end + drops + spans).
WEIGHTS = {
    "e2e_150_2pct": 1.0,
    "e2e_150_drop": 1.0,
    "dovetail_150": 1.0,
    "extend_150": 1.0,
    "e2e_1k_5pct": 0.5,
    "e2e_1k_05pct": 0.5,
}


def run_once(binary, reps):
    out = subprocess.run(
        ["taskset", "-c", CORE, binary, str(reps), "1"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    rows = {}
    for line in out.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        r = json.loads(line)
        rows[r["workload"]] = r
    return rows


def merge_min(acc, rows):
    for k, r in rows.items():
        if k not in acc:
            acc[k] = dict(r)
        else:
            acc[k]["ns_per_pair"] = min(acc[k]["ns_per_pair"], r["ns_per_pair"])
            acc[k]["peak_heap"] = max(acc[k]["peak_heap"], r["peak_heap"])
            if acc[k]["checksum"] != r["checksum"]:
                acc[k]["checksum"] = "MISMATCH"
    return acc


def ab(candidate, baseline, rounds, reps):
    cand, base = {}, {}
    for i in range(rounds):
        # Alternate order each round to spread thermal/frequency drift.
        order = [(candidate, cand), (baseline, base)] if i % 2 == 0 else [(baseline, base), (candidate, cand)]
        for binary, acc in order:
            if binary is None:
                continue
            merge_min(acc, run_once(binary, reps))
    return cand, base


def geo_ratio(cand, base):
    import math

    num = 0.0
    den = 0.0
    for w, wt in WEIGHTS.items():
        if w in cand and w in base and base[w]["ns_per_pair"] > 0:
            num += wt * math.log(cand[w]["ns_per_pair"] / base[w]["ns_per_pair"])
            den += wt
    return math.exp(num / den) if den else float("nan")


def load_history():
    if not os.path.exists(HISTORY):
        return []
    with open(HISTORY) as f:
        return [json.loads(l) for l in f if l.strip()]


def write_report(history):
    workloads = list(WEIGHTS.keys())
    taken = [h for h in history if h["status"] == "taken"]
    v0 = taken[0] if taken else None

    def fmt_ns(v):
        return f"{v / 1000:.2f} µs" if v is not None else "–"

    def fmt_mb(v):
        return f"{v / 1e6:.2f} MB" if v is not None else "–"

    # Series for the chart: ns/pair per taken version, normalised to v0.
    series = {w: [] for w in workloads}
    labels = []
    for h in taken:
        labels.append(h["label"])
        for w in workloads:
            c = h["candidate"].get(w)
            b = v0["candidate"].get(w) if v0 else None
            series[w].append(round(c["ns_per_pair"] / b["ns_per_pair"], 4) if c and b else None)

    rows_html = []
    for h in history:
        cls = {"taken": "taken", "dropped": "dropped", "failed": "failed"}.get(h["status"], "")
        cells = []
        for w in workloads:
            c = h["candidate"].get(w)
            b = h["baseline"].get(w) if h.get("baseline") else None
            if not c:
                cells.append("<td>–</td>")
                continue
            ratio = f"{c['ns_per_pair'] / b['ns_per_pair']:.3f}×" if b else ""
            vs0 = ""
            if v0 and w in v0["candidate"]:
                vs0 = f"<span class=vs0>{c['ns_per_pair'] / v0['candidate'][w]['ns_per_pair']:.3f}× v0</span>"
            chk = "" if c.get("checksum") != "MISMATCH" else " <b class=bad>checksum!</b>"
            cells.append(
                f"<td><b>{fmt_ns(c['ns_per_pair'])}</b><br><small>{ratio} {vs0}</small>"
                f"<br><small class=mem>{fmt_mb(c['peak_heap'])}</small>{chk}</td>"
            )
        gr = h.get("geo_ratio")
        vs0_geo = ""
        if v0 and h is not v0:
            vs0_geo = f"<br><small>{geo_ratio(h['candidate'], v0['candidate']):.3f}× v0</small>"
        rows_html.append(
            f"<tr class={cls}><td>{h['iteration']}</td><td>{h['time'][:16]}</td>"
            f"<td><b>{h['label']}</b><br><small>{h.get('note', '')}</small></td>"
            f"<td class=status>{h['status']}</td>"
            f"<td>{'' if gr is None else f'{gr:.3f}×'}{vs0_geo}</td>" + "".join(cells) + "</tr>"
        )

    best = taken[-1] if taken else None
    headline = ""
    if v0 and best:
        g = geo_ratio(best["candidate"], v0["candidate"])
        headline = (
            f"<p class=headline>Current best <b>{best['label']}</b> runs at "
            f"<b>{g:.3f}×</b> of v0 time (geo-mean, flexalign weights) "
            f"= <b>{(1 / g - 1) * 100:+.1f}%</b> throughput. Goal: 50% speedup (0.667× time).</p>"
        )

    html = f"""<!doctype html>
<html><head><meta charset=utf-8><title>flexalign bench history</title>
<style>
:root {{ --bg:#fff; --fg:#111; --muted:#666; --ok:#e6f7e6; --bad:#fbe9e9; --warn:#fff6e0; --line:#ddd; }}
@media (prefers-color-scheme: dark) {{ :root {{ --bg:#15171a; --fg:#eee; --muted:#aaa; --ok:#1e3a1e; --bad:#4a1f1f; --warn:#3d3416; --line:#333; }} }}
body {{ background:var(--bg); color:var(--fg); font:14px/1.4 system-ui, sans-serif; margin:16px; }}
table {{ border-collapse:collapse; width:100%; font-size:13px; }}
th, td {{ border:1px solid var(--line); padding:4px 6px; vertical-align:top; text-align:left; }}
tr.taken {{ background:var(--ok); }} tr.dropped {{ background:var(--warn); }} tr.failed {{ background:var(--bad); }}
.status {{ font-weight:bold; text-transform:uppercase; }} .mem {{ color:var(--muted); }} .vs0 {{ color:var(--muted); }}
.bad {{ color:#c00; }} .headline {{ font-size:16px; }}
svg {{ max-width:100%; height:auto; background:var(--bg); }}
</style></head><body>
<h1>wfa2lib-rs · flexalign-shaped benchmark history</h1>
<p>Gap-affine (0,10,12,3), CIGAR scope, WF-adaptive (10,50,1), ANI max-score abort, ends-free spans.
Each cell: min ns/pair over alternating A/B rounds pinned to core {CORE}; ratio vs. the baseline it was
measured against, and vs. v0. Memory = peak live heap of the workload (counting allocator).
Rows: <span style="background:var(--ok)">taken</span> ·
<span style="background:var(--warn)">dropped (slower)</span> ·
<span style="background:var(--bad)">failed (tests / build / checksum)</span>.</p>
{headline}
{svg_chart(labels, series, workloads)}
<table><thead><tr><th>#</th><th>when</th><th>version</th><th>status</th><th>geo-mean vs base</th>
{''.join(f'<th>{w}</th>' for w in workloads)}</tr></thead><tbody>
{''.join(rows_html)}
</tbody></table>
<p><small>Generated {datetime.now().isoformat(timespec='seconds')} · history: bench/flexalign_history.jsonl · harness: examples/flexalign_bench.rs</small></p>
</body></html>"""
    with open(REPORT, "w") as f:
        f.write(html)


def svg_chart(labels, series, workloads):
    if not labels:
        return ""
    W, H, L, B, T, R = 900, 320, 50, 60, 20, 20
    n = len(labels)
    vals = [v for s in series.values() for v in s if v is not None]
    if not vals:
        return ""
    ymin = min(0.5, min(vals) * 0.95)
    ymax = max(1.05, max(vals) * 1.05)
    palette = ["#4e79a7", "#f28e2b", "#e15759", "#76b7b2", "#59a14f", "#edc948"]

    def x(i):
        return L + (W - L - R) * (i / max(1, n - 1))

    def y(v):
        return T + (H - T - B) * (1 - (v - ymin) / (ymax - ymin))

    parts = [f'<svg viewBox="0 0 {W} {H}" xmlns="http://www.w3.org/2000/svg">']
    for g in [0.5, 0.667, 0.8, 1.0]:
        if ymin <= g <= ymax:
            parts.append(
                f'<line x1="{L}" x2="{W - R}" y1="{y(g):.1f}" y2="{y(g):.1f}" stroke="#999" stroke-dasharray="4 4"/>'
                f'<text x="{L - 4}" y="{y(g) + 4:.1f}" font-size="11" text-anchor="end" fill="currentColor">{g:.3f}</text>'
            )
    for i, lab in enumerate(labels):
        parts.append(
            f'<text x="{x(i):.1f}" y="{H - B + 14}" font-size="10" text-anchor="end" transform="rotate(-30 {x(i):.1f} {H - B + 14})" fill="currentColor">{lab[:28]}</text>'
        )
    for j, w in enumerate(workloads):
        pts = [(x(i), y(v)) for i, v in enumerate(series[w]) if v is not None]
        if not pts:
            continue
        d = " ".join(f"{px:.1f},{py:.1f}" for px, py in pts)
        parts.append(f'<polyline fill="none" stroke="{palette[j % len(palette)]}" stroke-width="2" points="{d}"/>')
        for px, py in pts:
            parts.append(f'<circle cx="{px:.1f}" cy="{py:.1f}" r="3" fill="{palette[j % len(palette)]}"/>')
        parts.append(
            f'<text x="{W - R - 160}" y="{T + 14 + 14 * j}" font-size="11" fill="{palette[j % len(palette)]}">■ {w}</text>'
        )
    parts.append(f'<text x="{L}" y="{T - 6}" font-size="11" fill="currentColor">time per pair relative to v0 (lower is better)</text>')
    parts.append("</svg>")
    return "".join(parts)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--label")
    ap.add_argument("--candidate")
    ap.add_argument("--baseline")
    ap.add_argument("--rounds", type=int, default=3)
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--note", default="")
    ap.add_argument("--status", help="force status (e.g. failed) instead of measuring")
    ap.add_argument("--report-only", action="store_true")
    args = ap.parse_args()

    history = load_history()
    if args.report_only:
        write_report(history)
        print(REPORT)
        return

    rec = {
        "iteration": len(history) + 1,
        "time": datetime.now().isoformat(timespec="seconds"),
        "label": args.label,
        "note": args.note,
        "candidate": {},
        "baseline": {},
        "geo_ratio": None,
        "status": args.status or "pending",
    }
    if not args.status:
        t0 = time.time()
        cand, base = ab(args.candidate, args.baseline, args.rounds, args.reps)
        rec["candidate"], rec["baseline"] = cand, base
        if base:
            rec["geo_ratio"] = geo_ratio(cand, base)
            mismatch = any(
                cand[w]["checksum"] != base[w]["checksum"] for w in cand if w in base
            ) or any(r["checksum"] == "MISMATCH" for r in cand.values())
            if mismatch:
                rec["status"] = "failed"
                rec["note"] += " [output checksum differs from baseline]"
            else:
                rec["status"] = "taken" if rec["geo_ratio"] < 0.995 else "dropped"
        else:
            rec["status"] = "taken"
        rec["seconds"] = round(time.time() - t0, 1)
    with open(HISTORY, "a") as f:
        f.write(json.dumps(rec) + "\n")
    history.append(rec)
    write_report(history)
    print(json.dumps({k: rec[k] for k in ("iteration", "label", "status", "geo_ratio")}))
    for w in WEIGHTS:
        c = rec["candidate"].get(w)
        b = rec["baseline"].get(w)
        if c:
            print(
                f"  {w:14s} {c['ns_per_pair']:10.1f} ns"
                + (f"  vs {b['ns_per_pair']:10.1f}  ({c['ns_per_pair'] / b['ns_per_pair']:.3f}x)" if b else "")
                + f"  heap {c['peak_heap'] / 1e6:.2f} MB"
            )


if __name__ == "__main__":
    main()
