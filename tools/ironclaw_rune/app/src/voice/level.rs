//! The mic's live level, for the voice pill's waveform.
//!
//! The audio tap (`apple.rs`, Core Audio's thread) folds each PCM buffer
//! into one number with [`step`]: RMS → dBFS → 0..1 above a noise gate,
//! rising fast and falling smoothly, stored as `f32` bits in an
//! `AtomicU32`. That thread allocates nothing, takes no lock and logs
//! nothing. The UI samples it from the voice poll at [`TICK`] (~16 Hz),
//! only while the pill shows a live mic and motion is allowed
//! (`session::poll`), and turns it into [`BARS`] quantised heights with
//! [`bars`]. Idle voice reads nothing and asks for no wake.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

/// Bars in the pill's waveform.
pub const BARS: usize = 5;
/// Height steps per bar: a bar is `0..=STEPS` steps above its floor.
pub const STEPS: u8 = 10;
/// How often the UI samples the level while the mic is live (16.7 Hz).
pub const TICK: Duration = Duration::from_millis(60);

/// Below this the room counts as silent (the noise gate).
const GATE_DB: f32 = -52.;
/// At or above this the level is full.
const FULL_DB: f32 = -14.;
/// Fraction of the way to a louder target one buffer moves (~21 ms at
/// 1024 frames / 48 kHz): speech onsets show at once.
const ATTACK: f32 = 0.7;
/// Fraction of the way to a quieter target one buffer moves: a falling
/// level halves in about 110 ms.
const DECAY: f32 = 0.12;
/// Per UI tick while push-to-talk finishes (the tap has stopped): the bars
/// settle to rest over about 0.4 s.
pub const SETTLE: f32 = 0.55;
/// Levels under this land on silence (no endless tail of tiny commits).
const FLOOR: f32 = 0.01;

/// One writer (the tap), one reader (the UI); `f32` bits, relaxed: a stale
/// read by a buffer is invisible.
#[derive(Clone, Debug, Default)]
pub struct MicLevel(Arc<AtomicU32>);

impl MicLevel {
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }

    pub fn set(&self, level: f32) {
        self.0.store(level.to_bits(), Ordering::Relaxed);
    }

    /// Fold one buffer of samples into the level. Audio thread: no
    /// allocation, no lock.
    pub fn feed(&self, samples: &[f32]) {
        self.set(step(self.get(), samples));
    }
}

/// The level after one buffer: `prev` moved toward the buffer's loudness
/// (0 at or below the gate, 1 at [`FULL_DB`]), fast up and slow down.
pub fn step(prev: f32, samples: &[f32]) -> f32 {
    let target = loudness(samples);
    let rate = if target > prev { ATTACK } else { DECAY };
    let next = prev + (target - prev) * rate;
    if next < FLOOR { 0. } else { next.min(1.) }
}

/// A buffer's loudness, 0..1 on a dB scale above the noise gate.
pub fn loudness(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.;
    }
    let mut sum = 0f32;
    for &x in samples {
        // A NaN from a broken driver counts as silence.
        let x = if x.is_finite() { x } else { 0. };
        sum += x * x;
    }
    let rms = (sum / samples.len() as f32).sqrt();
    if rms <= 0. {
        return 0.;
    }
    let db = 20. * rms.log10();
    ((db - GATE_DB) / (FULL_DB - GATE_DB)).clamp(0., 1.)
}

/// Each bar's share of the level: tallest in the middle, as a voice
/// waveform reads.
const PROFILE: [f32; BARS] = [0.55, 0.8, 1., 0.8, 0.55];

/// The bars' heights in steps for `level` at sample `tick`: the profile
/// scaled by the level, each bar wobbling a little (±12 %) from tick to
/// tick so the shape breathes with the voice instead of moving as one.
pub fn bars(level: f32, tick: u32) -> [u8; BARS] {
    let level = level.clamp(0., 1.);
    let mut out = [0; BARS];
    for (i, (bar, share)) in out.iter_mut().zip(PROFILE).enumerate() {
        let wobble = 0.88 + 0.24 * jitter(tick, i as u32);
        let h = (level * share * wobble * f32::from(STEPS)).round();
        *bar = h.clamp(0., f32::from(STEPS)) as u8;
    }
    out
}

/// A cheap, deterministic 0..1 per (tick, bar).
fn jitter(tick: u32, bar: u32) -> f32 {
    let mut x = tick.wrapping_mul(0x9E37_79B9) ^ bar.wrapping_mul(0x85EB_CA6B);
    x ^= x >> 15;
    x = x.wrapping_mul(0x2C1B_3C6D);
    x ^= x >> 12;
    (x & 0xFFFF) as f32 / 65535.
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(amplitude: f32) -> Vec<f32> {
        (0..1024)
            .map(|i| amplitude * (i as f32 * 0.07).sin())
            .collect()
    }

    #[test]
    fn silence_and_the_noise_floor_read_as_zero() {
        assert_eq!(loudness(&[]), 0.);
        assert_eq!(loudness(&[0.; 1024]), 0.);
        // Room hiss around -60 dBFS stays under the gate.
        assert_eq!(loudness(&tone(0.0014)), 0.);
        assert_eq!(step(0., &tone(0.0014)), 0.);
        assert_eq!(loudness(&[f32::NAN; 64]), 0., "a broken buffer is silence");
    }

    #[test]
    fn full_scale_reads_as_one() {
        assert_eq!(loudness(&tone(1.)), 1.);
        assert_eq!(loudness(&[1.; 256]), 1.);
        let mut level = 0.;
        for _ in 0..8 {
            level = step(level, &tone(1.));
        }
        assert!(level > 0.99 && level <= 1., "{level}");
    }

    #[test]
    fn speech_sits_in_the_middle_of_the_scale() {
        // Ordinary speech at the mic, about -30 dBFS RMS.
        let l = loudness(&tone(0.045));
        assert!((0.4..0.75).contains(&l), "{l}");
    }

    #[test]
    fn the_level_rises_fast_and_falls_smoothly() {
        let loud = tone(0.5);
        let one = step(0., &loud);
        assert!(one >= 0.6, "one buffer of speech shows at once: {one}");
        let mut level = 1.;
        let mut buffers = 0;
        while level > 0.5 {
            level = step(level, &[0.; 1024]);
            buffers += 1;
        }
        // Half in ~5-6 buffers (~110 ms), not in one.
        assert!((4..=7).contains(&buffers), "{buffers}");
        let mut level = 1.;
        for _ in 0..60 {
            level = step(level, &[0.; 1024]);
        }
        assert_eq!(level, 0., "silence lands on zero, not an endless tail");
    }

    #[test]
    fn the_atomic_carries_the_level_between_threads() {
        let level = MicLevel::default();
        assert_eq!(level.get(), 0.);
        let writer = level.clone();
        std::thread::spawn(move || writer.feed(&tone(1.)))
            .join()
            .expect("writer thread");
        assert!(level.get() > 0.6);
    }

    #[test]
    fn bars_follow_the_level_with_a_little_variation() {
        assert_eq!(bars(0., 3), [0; BARS], "silence is a flat line");
        let full = bars(1., 3);
        assert!(full[2] >= 9, "the middle bar reaches the top: {full:?}");
        assert!(full[0] < full[2] && full[4] < full[2], "{full:?}");
        let ticks: Vec<_> = (0..8).map(|t| bars(0.6, t)).collect();
        assert!(
            ticks.windows(2).any(|w| w[0] != w[1]),
            "bars vary from tick to tick at a steady level"
        );
        for b in ticks.iter().flatten() {
            assert!(*b <= STEPS);
        }
        assert_eq!(bars(0.6, 5), bars(0.6, 5), "deterministic");
    }
}
