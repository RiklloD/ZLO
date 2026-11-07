//! Battery saver (t3code behaviour, GPUI `design/battery.rs`): decorative
//! animation stops while the window is in the background or nobody has
//! touched it for 30 s.
//!
//! GPUI flipped `reduce_motion`; on Rune the same switch is
//! `Cx::reduced_motion`, set every view run by `look::apply_motion` from
//! [`reduced`]. Under it loops (spinners, pulse dots, shimmer) hold their
//! resting pose and request no frames, travel snaps, and live content
//! (streamed text, new rows) still lands because it re-renders on data, not
//! on a clock. Rune's time scale is deliberately not used: at 0 it would
//! also freeze a new row mid-entrance, invisible, until the window wakes.
//!
//! Input is read from Rune itself (`rune::last_input`, stamped for every
//! key, pointer, wheel and gesture event), so no handler has to report it;
//! window focus comes from `App::on_focus` through `look::on_focus`.

use std::time::Duration;

use rune::Tx;
use rune::time::Instant;

use super::prefs::Prefs;

const IDLE_AFTER: Duration = Duration::from_secs(30);

pub struct BatterySaver {
    /// Idle: no input for [`IDLE_AFTER`].
    idle: bool,
    /// The window is the key window.
    active: bool,
    /// Counts as input: launch and every refocus.
    woke: Instant,
}

impl Default for BatterySaver {
    fn default() -> Self {
        Self {
            idle: false,
            active: true,
            woke: Instant::now(),
        }
    }
}

impl BatterySaver {
    /// Engaged: the window is in the background or idle. Pollers skip
    /// their tick while this holds.
    pub fn saving(&self) -> bool {
        self.idle || !self.active
    }

    /// The window gained or lost focus. Returns whether the saver changed.
    pub fn set_active(&mut self, active: bool) -> bool {
        let before = self.saving();
        self.active = active;
        if active {
            self.woke = Instant::now();
            self.idle = false;
        }
        before != self.saving()
    }

    /// From the app's poll: go idle at the deadline, wake on any input, and
    /// ask for one wake at the next deadline, so motion costs at most one
    /// wake per 30 s. Returns whether the saver changed.
    pub fn poll(&mut self, tx: &mut Tx) -> bool {
        let last = rune::last_input().map_or(self.woke, |t| t.max(self.woke));
        let quiet = last.elapsed();
        let idle = quiet >= IDLE_AFTER;
        let changed = idle != self.idle;
        self.idle = idle;
        if !idle && self.active {
            tx.wake_after(IDLE_AFTER - quiet);
        }
        changed
    }
}

/// Whether motion is reduced: "Reduce motion: On" in Appearance holds it
/// regardless; "System" follows the OS accessibility setting and the
/// battery saver; "Off" lets only the battery saver decide.
pub fn reduced(prefs: &Prefs, saver: &BatterySaver, system_reduced: bool) -> bool {
    match prefs.reduce_motion.as_str() {
        "on" => true,
        "off" => saver.saving(),
        _ => system_reduced || saver.saving(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_loss_engages_and_refocus_releases() {
        let mut saver = BatterySaver::default();
        assert!(!saver.saving());
        assert!(saver.set_active(false));
        assert!(saver.saving());
        assert!(saver.set_active(true));
        assert!(!saver.saving());
    }

    #[test]
    fn the_preference_overrides_the_saver() {
        let saver = BatterySaver::default();
        let mut prefs = Prefs {
            reduce_motion: "on".into(),
            ..Prefs::default()
        };
        assert!(reduced(&prefs, &saver, false));
        prefs.reduce_motion = "off".into();
        assert!(!reduced(&prefs, &saver, true));
        prefs.reduce_motion = "system".into();
        assert!(reduced(&prefs, &saver, true));
    }
}
