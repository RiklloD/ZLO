#!/usr/bin/env python3
"""ZLO frame-time bench: drive the `bench-*` scenes headless and report frame cost.

    bench/run.py run [NAME ...] [--runs 5] [--out results.json] [--no-pixels] [--no-build]
    bench/run.py compare BASE.json NEW.json
    bench/run.py show RESULTS.json

Each interaction is a scene (`app/src/bench.rs`) plus an input script
(`bench/scripts/NAME.json`), run by Rune's headless driver on its frame clock
at 120 Hz in real time (`--realtime`, so the app's own wall-clock timers, such
as the stream pacer, see real time). Timing comes from Rune's `frame-profile`
log (`RUNE_FRAME_LOG`), animation reasons from its motion trace (`--trace`,
`quality.why`), and wasted frames from one extra run that dumps every frame's
pixels. See bench/README.md.
"""

import argparse
import hashlib
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent  # tools/ironclaw_rune
BIN = ROOT / "target" / "release" / "zlo"
FPS = 120
SIZE = "1280x820"
BUDGET_MS = 1000 / FPS

# name -> (scene, seconds). Order is the report's order.
INTERACTIONS = {
    "stream": ("bench-stream", 7.5),
    "switch": ("bench-switch", 4.3),
    # The same switches, the pointer resting 0.3 s on each row before its tap.
    "switch-hover": ("bench-switch", 4.6),
    "sidebar": ("bench-sidebar", 3.5),
    "scroll": ("bench-scroll", 5.5),
    "dock": ("bench-dock", 3.3),
    "typing": ("bench-typing", 7.0),
    "shelf": ("bench-shelf", 4.0),
    "tools": ("bench-tools", 5.0),
}

PHASES = ["view_cpu", "commit_cpu", "layout_cpu", "text_cpu", "paint_cpu", "encode_ms"]


def build():
    cmd = ["cargo", "build", "-p", "ironclaw_rune", "--release", "--features", "rune/frame-profile"]
    print("+", " ".join(cmd), file=sys.stderr)
    subprocess.run(cmd, cwd=ROOT, check=True)


def pct(values, p):
    if not values:
        return 0.0
    v = sorted(values)
    return v[min(len(v) - 1, round((len(v) - 1) * p))]


def read_jsonl(path):
    with open(path) as f:
        return [json.loads(line) for line in f if line.strip()]


def run_once(name, tmp, pixels):
    scene, secs = INTERACTIONS[name]
    frames = int(secs * FPS)
    flog, trace = tmp / f"{name}-frames.jsonl", tmp / f"{name}-trace.jsonl"
    cmd = [str(BIN), "--scene", scene, "--script", str(HERE / "scripts" / f"{name}.json"),
           "--frames", str(frames), "--fps", str(FPS), "--size", SIZE, "--realtime", "--trace", str(trace)]
    out = None
    if pixels:
        out = tmp / f"{name}-px"
        shutil.rmtree(out, ignore_errors=True)
        out.mkdir()
        cmd += ["--out", str(out), "--every", "1"]
    env = dict(os.environ, RUNE_FRAME_LOG=str(flog))
    env.pop("RUNE_PERF", None)
    # Real time: a run lasts `secs`; a hung GPU init, readback or shutdown must
    # fail the suite, not stall it. Generous, since pixel capture is slow.
    limit = max(120.0, secs * 20)
    try:
        r = subprocess.run(cmd, cwd=ROOT, env=env, capture_output=True, text=True, timeout=limit)
    except subprocess.TimeoutExpired as e:
        err = e.stderr.decode(errors="replace") if isinstance(e.stderr, bytes) else (e.stderr or "")
        sys.exit(f"{name}: zlo did not finish within {limit:.0f} s (killed):\n{err[-3000:]}")
    if r.returncode != 0:
        sys.exit(f"{name}: zlo failed ({r.returncode}):\n{r.stderr[-3000:]}")
    rows = [x for x in read_jsonl(flog) if "k" in x]
    tr = [x for x in read_jsonl(trace) if x.get("type") != "header"]
    if len(tr) != len(rows):
        sys.exit(f"{name}: {len(rows)} frame-log rows but {len(tr)} trace frames")
    hashes = None
    if pixels:
        hashes = []
        for k in range(len(rows)):
            p = out / f"frame_{k:04d}.png"
            hashes.append(hashlib.blake2b(p.read_bytes(), digest_size=16).hexdigest() if p.exists() else None)
        shutil.rmtree(out, ignore_errors=True)
    return rows, tr, hashes


def drawn_mask(rows):
    """Frames a real window would draw: input arrived, an update committed, or the
    previous frame was still animating. Frame 0 (the scene's mount) is left out."""
    mask = [False]
    for k in range(1, len(rows)):
        r = rows[k]
        mask.append(bool(rows[k - 1]["animating"] or r["events"] or r["commits"]))
    return mask


def reasons(rows, tr, mask):
    """Why each drawn frame ran: the AnimSource bits that kept the previous frame
    animating (`quality.why`), or `input` / `update` when that started it."""
    out = {}
    for k in range(1, len(rows)):
        if not mask[k]:
            continue
        why = list(tr[k - 1].get("quality", {}).get("why") or []) if rows[k - 1]["animating"] else []
        if rows[k]["events"]:
            why.append("input")
        if rows[k]["commits"] and not rows[k]["events"]:
            why.append("update")
        for w in why or ["?"]:
            out[w] = out.get(w, 0) + 1
    return out


def commit_why(rows, mask):
    out = {}
    for k, r in enumerate(rows):
        if mask[k]:
            for w in r.get("why", []):
                out[w] = out.get(w, 0) + 1
    return out


def timing_stats(rows):
    mask = drawn_mask(rows)
    drawn = [r for k, r in enumerate(rows) if mask[k]]
    cpu = [r["cpu_ms"] for r in drawn]
    st = {
        "frames_total": len(rows),
        "frames_drawn": len(drawn),
        "cpu_p50": pct(cpu, 0.5), "cpu_p99": pct(cpu, 0.99), "cpu_max": max(cpu, default=0.0),
        "cpu_mean": statistics.fmean(cpu) if cpu else 0.0,
        "over_budget": sum(c > BUDGET_MS for c in cpu),
        "mount_ms": rows[0]["cpu_ms"] if rows else 0.0,
    }
    for ph in PHASES:
        vals = [r[ph] for r in drawn]
        st[f"{ph}_p50"] = pct(vals, 0.5)
        st[f"{ph}_p99"] = pct(vals, 0.99)
    return st, mask


def pixel_stats(rows, tr, hashes):
    mask = drawn_mask(rows)
    visible = [False] + [hashes[k] != hashes[k - 1] for k in range(1, len(rows))]
    wasted = [k for k in range(1, len(rows)) if mask[k] and not visible[k]]
    # Pixels changed with no input, update or animating frame before: paint-only
    # display-clock cycles (Rune's loop step), which a window draws without a full frame.
    unrequested = sum(1 for k in range(1, len(rows)) if visible[k] and not mask[k])
    wasted_set = set(wasted)
    why = reasons(rows, tr, [k in wasted_set for k in range(len(rows))])
    return {
        "frames_drawn": sum(mask),
        "frames_visible": sum(1 for k in range(1, len(rows)) if mask[k] and visible[k]),
        "frames_wasted": len(wasted),
        "visible_undrawn": unrequested,
        "wasted_why": why,
    }


def summarize(runs):
    """Median of each timing metric over the runs, with its min..max spread."""
    keys = [k for k in runs[0]["timing"] if isinstance(runs[0]["timing"][k], (int, float))]
    out = {}
    for k in keys:
        vals = [r["timing"][k] for r in runs]
        out[k] = {"median": statistics.median(vals), "min": min(vals), "max": max(vals)}
    return out


def cmd_run(args):
    names = args.names or list(INTERACTIONS)
    for n in names:
        if n not in INTERACTIONS:
            sys.exit(f"unknown interaction {n}; known: {', '.join(INTERACTIONS)}")
    if not args.no_build:
        build()
    if not BIN.exists():
        sys.exit(f"{BIN} missing; run without --no-build")
    commit = subprocess.run(["git", "rev-parse", "--short", "HEAD"], cwd=ROOT, capture_output=True, text=True).stdout.strip()
    result = {
        "meta": {"commit": commit, "machine": platform.machine(), "os": platform.platform(),
                 "cpu": subprocess.run(["sysctl", "-n", "machdep.cpu.brand_string"], capture_output=True, text=True).stdout.strip(),
                 "fps": FPS, "size": SIZE, "scale": 2, "runs": args.runs},
        "interactions": {},
    }
    with tempfile.TemporaryDirectory(prefix="zlo-bench-") as tmp:
        tmp = Path(tmp)
        for name in names:
            runs = []
            for i in range(args.runs):
                rows, tr, _ = run_once(name, tmp, False)
                st, mask = timing_stats(rows)
                runs.append({"timing": st, "why": reasons(rows, tr, mask), "commit_why": commit_why(rows, mask)})
                print(f"{name} run {i + 1}/{args.runs}: drawn {st['frames_drawn']} p50 {st['cpu_p50']:.2f} "
                      f"p99 {st['cpu_p99']:.2f} max {st['cpu_max']:.2f} ms", file=sys.stderr)
            entry = {"runs": runs, "summary": summarize(runs), "why": runs[len(runs) // 2]["why"],
                     "commit_why": runs[len(runs) // 2]["commit_why"]}
            if not args.no_pixels:
                rows, tr, hashes = run_once(name, tmp, True)
                entry["pixels"] = pixel_stats(rows, tr, hashes)
                print(f"{name} pixels: {entry['pixels']['frames_wasted']}/{entry['pixels']['frames_drawn']} wasted",
                      file=sys.stderr)
            result["interactions"][name] = entry
    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(result, indent=1, sort_keys=True) + "\n")
    print(f"wrote {out}", file=sys.stderr)
    show(result)


def ordered(result):
    """The interactions in the report's order (the JSON keeps them sorted)."""
    got = result["interactions"]
    return [(n, got[n]) for n in INTERACTIONS if n in got] + [(n, e) for n, e in got.items() if n not in INTERACTIONS]


def fmt_spread(m):
    return f"{m['median']:.2f} ({m['min']:.2f}–{m['max']:.2f})"


def show(result):
    meta = result["meta"]
    print(f"\nZLO frame bench @ {meta['commit']} · {meta.get('cpu') or meta['machine']} · {meta['fps']} Hz · "
          f"{meta['size']}@{meta['scale']}x · median of {meta['runs']} runs (min–max)\n")
    hdr = ["interaction", "drawn", "p50 ms", "p99 ms", "max ms", ">8.3ms", "layout p50/p99", "paint p50/p99",
           "submit p50", "wasted", "cycle"]
    lines = []
    for name, e in ordered(result):
        s = e["summary"]
        px = e.get("pixels")
        wasted = f"{px['frames_wasted']}/{px['frames_drawn']}" if px else "—"
        cycle = str(px["visible_undrawn"]) if px else "—"
        lines.append([name, f"{s['frames_drawn']['median']:.0f}", fmt_spread(s["cpu_p50"]), fmt_spread(s["cpu_p99"]),
                      fmt_spread(s["cpu_max"]), f"{s['over_budget']['median']:.0f}",
                      f"{s['layout_cpu_p50']['median']:.2f}/{s['layout_cpu_p99']['median']:.2f}",
                      f"{s['paint_cpu_p50']['median']:.2f}/{s['paint_cpu_p99']['median']:.2f}",
                      f"{s['encode_ms_p50']['median']:.2f}", wasted, cycle])
    table(hdr, lines)
    print("\nWhy frames ran (drawn frames, middle run) and why wasted frames ran:")
    for name, e in ordered(result):
        why = ", ".join(f"{k} {v}" for k, v in sorted(e["why"].items(), key=lambda kv: -kv[1]))
        print(f"  {name:8} {why}")
        if e.get("pixels") and e["pixels"]["wasted_why"]:
            ww = ", ".join(f"{k} {v}" for k, v in sorted(e["pixels"]["wasted_why"].items(), key=lambda kv: -kv[1]))
            print(f"  {'':8} wasted: {ww}")


def table(hdr, lines):
    widths = [max(len(str(x)) for x in col) for col in zip(hdr, *lines)]
    print(" | ".join(h.ljust(w) for h, w in zip(hdr, widths)))
    print("-+-".join("-" * w for w in widths))
    for l in lines:
        print(" | ".join(str(x).ljust(w) for x, w in zip(l, widths)))


def cmd_compare(args):
    base, new = json.loads(Path(args.base).read_text()), json.loads(Path(args.new).read_text())
    print(f"base {base['meta']['commit']} → new {new['meta']['commit']}  (Δ outside base's min–max spread marked *)\n")
    metrics = ["frames_drawn", "cpu_p50", "cpu_p99", "cpu_max", "over_budget", "layout_cpu_p50", "paint_cpu_p50", "encode_ms_p50"]
    hdr = ["interaction", "metric", "base", "new", "Δ", "Δ%", ""]
    lines = []
    for name, e in ordered(new):
        b = base["interactions"].get(name)
        if not b:
            continue
        for m in metrics:
            bm, nm = b["summary"][m], e["summary"][m]
            d = nm["median"] - bm["median"]
            rel = f"{100 * d / bm['median']:+.0f}%" if bm["median"] else "—"
            noisy = bm["min"] <= nm["median"] <= bm["max"]
            lines.append([name, m, f"{bm['median']:.2f}", f"{nm['median']:.2f}", f"{d:+.2f}", rel, "" if noisy else "*"])
        if b.get("pixels") and e.get("pixels"):
            bw, nw = b["pixels"]["frames_wasted"], e["pixels"]["frames_wasted"]
            lines.append([name, "frames_wasted", str(bw), str(nw), f"{nw - bw:+d}", "", "" if nw == bw else "*"])
    table(hdr, lines)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run")
    r.add_argument("names", nargs="*")
    r.add_argument("--runs", type=int, default=5)
    r.add_argument("--out", default=str(HERE / "results" / "latest.json"))
    r.add_argument("--no-pixels", action="store_true", help="skip the wasted-frame (pixel dump) pass")
    r.add_argument("--no-build", action="store_true")
    c = sub.add_parser("compare")
    c.add_argument("base")
    c.add_argument("new")
    s = sub.add_parser("show")
    s.add_argument("results")
    args = ap.parse_args()
    if args.cmd == "run":
        cmd_run(args)
    elif args.cmd == "compare":
        cmd_compare(args)
    else:
        show(json.loads(Path(args.results).read_text()))


if __name__ == "__main__":
    main()
