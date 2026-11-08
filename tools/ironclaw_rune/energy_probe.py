#!/usr/bin/env python3
"""Measure ZLO's energy from the outside, for A/B runs of any build.

Reads the kernel's per-process accounting (proc_pid_rusage v6: CPU time,
idle wakeups, CPU energy in nanojoules) for the ZLO UI (`zlo`) and its
backend (`zlo-backend`) over a window, and prints one summary line each.

    python3 tools/ironclaw_rune/energy_probe.py 60          # 60 s window
    python3 tools/ironclaw_rune/energy_probe.py 60 --json   # machine readable

The app's own meter (ZLO_ENERGY, app/src/energy.rs) says *why* the work
happened; this says how much it cost, the same way for old and new builds.
Energy is CPU only; GPU work shows up under WindowServer.
"""
import ctypes
import json
import subprocess
import sys
import time

libc = ctypes.CDLL("/usr/lib/libSystem.dylib")
WORDS = 64
USER, SYSTEM, IDLE, INTR, FOOT, ENERGY = 2, 3, 4, 5, 9, 42


class Timebase(ctypes.Structure):
    _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]


tb = Timebase()
libc.mach_timebase_info(ctypes.byref(tb))


def usage(pid):
    buf = (ctypes.c_uint64 * WORDS)()
    if libc.proc_pid_rusage(pid, 6, buf) != 0:
        return None
    ticks = buf[USER] + buf[SYSTEM]
    return {
        "cpu_ns": ticks * tb.numer // tb.denom,
        "idle_wakeups": buf[IDLE],
        "interrupt_wakeups": buf[INTR],
        "energy_nj": buf[ENERGY],
        "footprint": buf[FOOT],
    }


def pids():
    out = subprocess.run(["ps", "-axo", "pid=,comm="], capture_output=True, text=True).stdout
    found = {}
    for line in out.splitlines():
        pid, _, comm = line.strip().partition(" ")
        name = comm.rsplit("/", 1)[-1]
        if name in ("zlo", "zlo-backend"):
            found[name] = int(pid)
    return found


def main():
    secs = float(sys.argv[1]) if len(sys.argv) > 1 else 30
    as_json = "--json" in sys.argv
    procs = pids()
    if "zlo" not in procs:
        sys.exit("ZLO is not running")
    before = {n: usage(p) for n, p in procs.items()}
    t0 = time.monotonic()
    time.sleep(secs)
    dt = time.monotonic() - t0
    result = {}
    for name, pid in procs.items():
        a, b = before[name], usage(pid)
        if not a or not b:
            continue
        result[name] = {
            "cpu_pct": round((b["cpu_ns"] - a["cpu_ns"]) / 1e7 / dt, 2),
            "mw": round((b["energy_nj"] - a["energy_nj"]) / 1e6 / dt, 1),
            "idle_wakeups_s": round((b["idle_wakeups"] - a["idle_wakeups"]) / dt, 1),
            "intr_wakeups_s": round((b["interrupt_wakeups"] - a["interrupt_wakeups"]) / dt, 1),
            "footprint_mb": b["footprint"] // (1 << 20),
        }
    if as_json:
        print(json.dumps({"secs": round(dt, 1), **result}))
        return
    for name, r in result.items():
        print(
            f"{name:12} cpu {r['cpu_pct']:6.2f}%  {r['mw']:7.1f} mW  "
            f"idle wakeups {r['idle_wakeups_s']:6.1f}/s  intr {r['intr_wakeups_s']:6.1f}/s  "
            f"{r['footprint_mb']} MB"
        )


if __name__ == "__main__":
    main()
