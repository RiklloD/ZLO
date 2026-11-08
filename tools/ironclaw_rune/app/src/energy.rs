//! The energy meter: what in ZLO does work, and what that work costs.
//!
//! Two halves:
//!
//! - **Counters**, always on: one relaxed atomic add per event (a frame, a
//!   view run, a poll that changed something, a reply landing on the UI
//!   thread). Cheap enough to leave in release builds.
//! - **The reporter**, off unless `ZLO_ENERGY` is set (to the interval in
//!   seconds; any other value means 10). It wakes once per interval, takes
//!   the counters' deltas and reads the kernel's own accounting for the UI
//!   and the backend it supervises (`proc_pid_rusage`: CPU time, idle
//!   wakeups, and the CPU energy in nanojoules macOS bills each process),
//!   then appends one JSON line to `~/Library/Logs/ZLO/energy.jsonl`.
//!
//! Start a metered run with `open --env ZLO_ENERGY=10 ~/Applications/ZLO.app`.
//! For A/B runs of any build (this meter or not), `energy_send.sh` streams a
//! reply into an open thread without touching the window and
//! `energy_probe.py` reads the same kernel numbers from outside; both live
//! in `tools/ironclaw_rune/`. `ZLO_WORD_FADE=1` (or `0`) forces the streamed
//! word fade on (off), to measure the focused look from a background window.
//! The kernel numbers are CPU only: GPU work shows as `frame_drawn` (and in
//! WindowServer), not in `energy_mj`.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

macro_rules! counters {
    ($($name:ident => $label:literal),* $(,)?) => {
        /// One thing the meter counts.
        #[derive(Clone, Copy, Debug)]
        #[allow(dead_code)]
        pub enum C { $($name),* }
        const NAMES: &[&str] = &[$($label),*];
    };
}

counters! {
    FrameDrawn => "frame_drawn",
    FrameUnchanged => "frame_unchanged",
    FrameSkipped => "frame_skipped",
    // Microseconds of frame update (events, poll, view, layout, paint).
    FrameUpdateUs => "frame_update_us",
    // Microseconds of frame encode (uploads, draw calls).
    FrameEncodeUs => "frame_encode_us",
    ViewRun => "view_run",
    PollCall => "poll_call",
    PollWorkspace => "poll_changed_workspace",
    PollQuick => "poll_changed_quick",
    PollChatPace => "poll_changed_chat_pace",
    PollChatClock => "poll_changed_chat_clock",
    PollChatCopied => "poll_changed_chat_copied",
    PollComputer => "poll_changed_computer",
    PollSide => "poll_changed_side",
    PollVoice => "poll_changed_voice",
    PollSnapshot => "poll_changed_snapshot",
    PollLook => "poll_changed_look",
    PollNativeView => "poll_changed_native_view",
    // A gateway reply landed (`jobs::run`).
    UpdateJob => "update_job",
    // A burst of stream messages landed.
    UpdateStream => "update_stream",
    UpdateBlocking => "update_blocking",
    UpdateTimer => "update_timer",
    // Stream messages that changed the transcript, and those that did not.
    StreamChanged => "stream_msg_changed",
    StreamIdle => "stream_msg_idle",
    // The transcript turned into rows again.
    RowsRebuilt => "rows_rebuilt",
    // Rows folded into list items.
    ItemsFolded => "items_folded",
    PacerStep => "pacer_step",
    // The push-to-talk watcher woken by a modifier change.
    PttWake => "ptt_wake",
}

const N: usize = NAMES.len();
static COUNTS: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];

/// Count one `c`.
#[inline]
pub fn hit(c: C) {
    COUNTS[c as usize].fetch_add(1, Ordering::Relaxed);
}

/// Count `n` of `c`.
#[inline]
pub fn add(c: C, n: u64) {
    COUNTS[c as usize].fetch_add(n, Ordering::Relaxed);
}

/// Count `changed` against a poll source; passes it through.
#[inline]
pub fn polled(c: C, changed: bool) -> bool {
    if changed {
        hit(c);
    }
    changed
}

/// Rune's per-frame report (`App::on_frame_encoded`).
pub fn frame(f: &rune::FrameEncoded) {
    match f.outcome() {
        rune::FrameOutcome::Drawn => {
            hit(C::FrameDrawn);
            add(
                C::FrameUpdateUs,
                (f.update_ms().unwrap_or(0.) * 1000.) as u64,
            );
            add(
                C::FrameEncodeUs,
                (f.encode_ms().unwrap_or(0.) * 1000.) as u64,
            );
        }
        rune::FrameOutcome::Unchanged => hit(C::FrameUnchanged),
        _ => hit(C::FrameSkipped),
    }
}

/// Start the reporter when `ZLO_ENERGY` asks for it. Call once at launch.
pub fn start() {
    let Some(raw) = std::env::var_os("ZLO_ENERGY") else {
        return;
    };
    let secs = raw
        .to_str()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(10);
    let Some(path) = dirs::home_dir().map(|h| h.join("Library/Logs/ZLO/energy.jsonl")) else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("zlo-energy".into())
        .spawn(move || report_loop(Duration::from_secs(secs), &path));
}

fn report_loop(every: Duration, path: &std::path::Path) {
    use std::io::Write as _;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(mut out) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        tracing::warn!(path = %path.display(), "energy meter: cannot open its log");
        return;
    };
    tracing::info!(path = %path.display(), secs = every.as_secs(), "energy meter on");
    let me = std::process::id();
    let mut last_ui = usage::read(me);
    let mut last_backend: Option<(u32, usage::Usage)> = None;
    let mut last_at = Instant::now();
    loop {
        std::thread::sleep(every);
        let secs = last_at.elapsed().as_secs_f64();
        last_at = Instant::now();
        let mut line = String::with_capacity(1024);
        let _ = write!(
            line,
            "{{\"t\":{},\"secs\":{secs:.2}",
            chrono::Utc::now().timestamp()
        );
        for (name, count) in NAMES.iter().zip(COUNTS.iter()) {
            let n = count.swap(0, Ordering::Relaxed);
            if n > 0 {
                let _ = write!(line, ",\"{name}\":{n}");
            }
        }
        let ui = usage::read(me);
        if let (Some(a), Some(b)) = (&last_ui, &ui) {
            b.since(a).write_json(&mut line, "ui", secs);
        }
        last_ui = ui;
        let backend = crate::service::backend_pid().and_then(|pid| Some((pid, usage::read(pid)?)));
        if let (Some((pa, a)), Some((pb, b))) = (&last_backend, &backend)
            && pa == pb
        {
            b.since(a).write_json(&mut line, "backend", secs);
        }
        last_backend = backend;
        line.push_str("}\n");
        if out.write_all(line.as_bytes()).is_err() {
            return;
        }
    }
}

/// The kernel's per-process accounting (`proc_pid_rusage`).
mod usage {
    use std::fmt::Write as _;

    /// `rusage_info_v6` (and v4 inside it) as u64 words: a 16-byte uuid,
    /// then the fields in `<sys/resource.h>` order.
    const WORDS: usize = 64;
    const USER_TIME: usize = 2;
    const SYSTEM_TIME: usize = 3;
    const IDLE_WAKEUPS: usize = 4;
    const INTERRUPT_WAKEUPS: usize = 5;
    const PHYS_FOOTPRINT: usize = 9;
    const BILLED_ENERGY: usize = 33;
    const ENERGY_NJ: usize = 42;
    const RUSAGE_INFO_V4: i32 = 4;
    const RUSAGE_INFO_V6: i32 = 6;

    unsafe extern "C" {
        fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut u64) -> i32;
        fn mach_timebase_info(info: *mut [u32; 2]) -> i32;
    }

    #[derive(Clone, Copy)]
    pub struct Usage {
        cpu_ns: u64,
        idle_wakeups: u64,
        interrupt_wakeups: u64,
        footprint: u64,
        /// CPU energy in nanojoules (v6), or the v4 billed energy in the
        /// same unit where v6 is missing.
        energy_nj: u64,
    }

    fn ticks_to_ns(ticks: u64) -> u64 {
        let mut tb = [0u32; 2];
        // SAFETY: `mach_timebase_info` fills two u32s (numer, denom).
        unsafe { mach_timebase_info(&mut tb) };
        if tb[1] == 0 {
            return ticks;
        }
        (ticks as u128 * tb[0] as u128 / tb[1] as u128) as u64
    }

    pub fn read(pid: u32) -> Option<Usage> {
        let mut buf = [0u64; WORDS];
        // SAFETY: the buffer is larger than `rusage_info_v6`.
        let v6 = unsafe { proc_pid_rusage(pid as i32, RUSAGE_INFO_V6, buf.as_mut_ptr()) } == 0;
        if !v6 {
            buf = [0; WORDS];
            // SAFETY: as above; v4 is smaller still.
            if unsafe { proc_pid_rusage(pid as i32, RUSAGE_INFO_V4, buf.as_mut_ptr()) } != 0 {
                return None;
            }
        }
        Some(Usage {
            cpu_ns: ticks_to_ns(buf[USER_TIME] + buf[SYSTEM_TIME]),
            idle_wakeups: buf[IDLE_WAKEUPS],
            interrupt_wakeups: buf[INTERRUPT_WAKEUPS],
            footprint: buf[PHYS_FOOTPRINT],
            energy_nj: if v6 {
                buf[ENERGY_NJ]
            } else {
                buf[BILLED_ENERGY]
            },
        })
    }

    impl Usage {
        pub fn since(&self, before: &Usage) -> Usage {
            Usage {
                cpu_ns: self.cpu_ns.saturating_sub(before.cpu_ns),
                idle_wakeups: self.idle_wakeups.saturating_sub(before.idle_wakeups),
                interrupt_wakeups: self
                    .interrupt_wakeups
                    .saturating_sub(before.interrupt_wakeups),
                footprint: self.footprint,
                energy_nj: self.energy_nj.saturating_sub(before.energy_nj),
            }
        }

        pub fn write_json(&self, out: &mut String, who: &str, secs: f64) {
            let _ = write!(
                out,
                ",\"{who}_cpu_pct\":{:.2},\"{who}_idle_wakeups\":{},\"{who}_interrupt_wakeups\":{},\"{who}_energy_mj\":{:.1},\"{who}_mw\":{:.1},\"{who}_footprint_mb\":{}",
                self.cpu_ns as f64 / 1e7 / secs,
                self.idle_wakeups,
                self.interrupt_wakeups,
                self.energy_nj as f64 / 1e6,
                self.energy_nj as f64 / 1e6 / secs,
                self.footprint / (1024 * 1024),
            );
        }
    }
}
