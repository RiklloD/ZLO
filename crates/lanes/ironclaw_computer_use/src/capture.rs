//! Pictures for the live view (the Hermes plugin's `live.capture_*` and
//! `stream._Producer`). The primary path is macOS's own window capture
//! (`screencapture -l <window>`), which reads one window even when it is
//! covered; when that fails, the agent's latest screenshot of that window is
//! shown instead (`snapshot`), never the whole desktop. Capture never talks
//! to the Open Computer Use server, so the model's element indices survive.
//!
//! One producer per (session, window, width) captures for every reader at
//! the fastest rate any of them asked for, and stops a second after the last
//! read. Each picture is fenced by the desktop lease: nothing is captured
//! while a person holds the desktop, and a picture taken across a takeover
//! (the epoch moved) is dropped.

use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageFormat, imageops::FilterType};
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::leftovers::Leftovers;
use crate::live::{Live, WatchedWindow, desk};

pub(crate) const MAX_FPS: f64 = 10.0;
/// A producer with no reader for this long stops.
const PRODUCER_IDLE: Duration = Duration::from_secs(1);
/// macOS window capture is retried this long after it failed for a window.
const CAPTURE_RETRY: Duration = Duration::from_secs(10);
const SCREENCAPTURE_TIMEOUT: Duration = Duration::from_secs(4);
/// A reader waits this long for a picture newer than the one it has.
const LONG_POLL: Duration = Duration::from_millis(1000);
const MAX_PRODUCERS: usize = 6;
const JPEG_QUALITY: u8 = 72;
/// Window captures live in their own directory under the temp dir.
const CAPTURE_DIR_NAME: &str = "ironclaw-cu-captures";
/// Earlier builds wrote captures straight into the temp dir with this prefix.
const LEGACY_CAPTURE_PREFIX: &str = "ironclaw_cu_";
/// A capture file older than this at startup is a leftover of an earlier
/// process (a capture lives at most `SCREENCAPTURE_TIMEOUT` plus decoding).
const STALE_CAPTURE_AGE: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    Window,
    Snapshot,
}

impl Method {
    fn as_str(self) -> &'static str {
        match self {
            Self::Window => "window",
            Self::Snapshot => "snapshot",
        }
    }
}

#[derive(Debug)]
pub(crate) struct Shot {
    pub seq: u64,
    pub jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub method: Method,
    pub epoch: u64,
    pub at: Instant,
}

impl Shot {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "seq": self.seq,
            "mimeType": "image/jpeg",
            "data": base64::engine::general_purpose::STANDARD.encode(&self.jpeg),
            "width": self.width,
            "height": self.height,
            "method": self.method.as_str(),
            "ageMs": u64::try_from(self.at.elapsed().as_millis()).unwrap_or(u64::MAX),
        })
    }
}

/// Why a window has no picture right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    Ended,
    HumanHasControl,
    CaptureFailed,
}

impl Refusal {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Ended => "ended",
            Self::HumanHasControl => "human_has_control",
            Self::CaptureFailed => "capture_failed",
        }
    }
}

type ProducerKey = (WatchedWindow, u32);

struct Producer {
    latest: watch::Sender<Option<Arc<Shot>>>,
    refusal: Mutex<Option<Refusal>>,
    last_read: Mutex<Instant>,
    fps: Mutex<f64>,
}

impl Producer {
    fn read(&self, fps: f64) {
        *lock(&self.last_read) = Instant::now();
        let mut current = lock(&self.fps);
        *current = current.max(fps);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) struct Capture {
    producers: Mutex<HashMap<ProducerKey, Arc<Producer>>>,
    capture_dir: PathBuf,
    leftovers: Arc<Leftovers>,
    next_file: AtomicU64,
}

impl Capture {
    /// Captures go to a private directory under the temp dir. Leftover
    /// screenshots of an earlier process are deleted here, at startup.
    pub(crate) fn new(leftovers: Arc<Leftovers>) -> Self {
        let temp = std::env::temp_dir();
        leftovers.sweep_stale(&temp, LEGACY_CAPTURE_PREFIX, ".jpg", STALE_CAPTURE_AGE);
        Self::in_dir(temp.join(CAPTURE_DIR_NAME), leftovers, STALE_CAPTURE_AGE)
    }

    pub(crate) fn in_dir(capture_dir: PathBuf, leftovers: Arc<Leftovers>, stale: Duration) -> Self {
        leftovers.sweep_stale(&capture_dir, "", ".jpg", stale);
        Self {
            producers: Mutex::new(HashMap::new()),
            capture_dir,
            leftovers,
            next_file: AtomicU64::new(0),
        }
    }

    /// Where the next window capture is written. Retries any screenshot an
    /// earlier capture could not delete first.
    fn next_capture_path(&self) -> PathBuf {
        self.leftovers.sweep();
        create_private_dir(&self.capture_dir);
        self.capture_dir.join(format!(
            "{}_{}.jpg",
            std::process::id(),
            self.next_file.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// Delete a finished capture; a failed delete is retried, then swept.
    async fn discard_capture(&self, path: &Path) {
        self.leftovers.remove(path).await;
    }

    /// The newest picture of `window` after `after_seq`, waiting briefly for
    /// one. `Err` names why there is none.
    pub(crate) async fn frame(
        self: &Arc<Self>,
        live: &Arc<Live>,
        window: WatchedWindow,
        width: u32,
        fps: f64,
        after_seq: Option<u64>,
    ) -> Result<Option<Arc<Shot>>, Refusal> {
        let desk = desk(&window.session);
        if live.lease(&desk).0 {
            return Err(Refusal::HumanHasControl);
        }
        let producer = self.producer(live, window, width)?;
        producer.read(fps.clamp(0.2, MAX_FPS));
        let mut rx = producer.latest.subscribe();
        let newer = |shot: &Option<Arc<Shot>>| {
            shot.as_ref()
                .is_some_and(|shot| after_seq.is_none_or(|after| shot.seq > after))
        };
        if !newer(&rx.borrow()) {
            #[expect(
                clippy::let_underscore_must_use,
                reason = "a timeout means nothing newer yet, which is answered below"
            )]
            let _ = tokio::time::timeout(LONG_POLL, rx.wait_for(newer)).await;
        }
        let shot = rx.borrow().clone();
        // Publication fence: nothing goes out while a person holds the
        // desktop or after the lease changed since the picture was taken.
        let (held, epoch) = live.lease(&desk);
        if held {
            return Err(Refusal::HumanHasControl);
        }
        match shot {
            Some(shot) if shot.epoch == epoch && newer(&Some(Arc::clone(&shot))) => Ok(Some(shot)),
            Some(_) => Ok(None),
            None => match *lock(&producer.refusal) {
                Some(refusal) => Err(refusal),
                None => Ok(None),
            },
        }
    }

    fn producer(
        self: &Arc<Self>,
        live: &Arc<Live>,
        window: WatchedWindow,
        width: u32,
    ) -> Result<Arc<Producer>, Refusal> {
        let mut producers = lock(&self.producers);
        let key = (window, width);
        if let Some(producer) = producers.get(&key) {
            return Ok(Arc::clone(producer));
        }
        if producers.len() >= MAX_PRODUCERS {
            return Err(Refusal::CaptureFailed);
        }
        let producer = Arc::new(Producer {
            latest: watch::Sender::new(None),
            refusal: Mutex::new(None),
            last_read: Mutex::new(Instant::now()),
            fps: Mutex::new(1.0),
        });
        producers.insert(key.clone(), Arc::clone(&producer));
        drop(producers);
        tokio::spawn(produce(
            Arc::clone(self),
            Arc::clone(live),
            key,
            Arc::clone(&producer),
        ));
        Ok(producer)
    }

    fn remove(&self, key: &ProducerKey, producer: &Arc<Producer>) {
        let mut producers = lock(&self.producers);
        if producers.get(key).is_some_and(|p| Arc::ptr_eq(p, producer)) {
            producers.remove(key);
        }
    }
}

async fn produce(
    capture: Arc<Capture>,
    live: Arc<Live>,
    key: ProducerKey,
    producer: Arc<Producer>,
) {
    let (window, width) = &key;
    let desk = desk(&window.session);
    let mut seq = 0;
    loop {
        if lock(&producer.last_read).elapsed() > PRODUCER_IDLE {
            break;
        }
        let started = Instant::now();
        let fps = *lock(&producer.fps);
        if live.watched(&window.session, window.window_id).is_none() {
            *lock(&producer.refusal) = Some(Refusal::Ended);
            break;
        }
        let (held, epoch) = live.lease(&desk);
        if held {
            *lock(&producer.refusal) = Some(Refusal::HumanHasControl);
            // Never keep (or show) what was on screen around a takeover.
            producer.latest.send_replace(None);
        } else {
            match capture_window(&capture, &live, window.window_id, *width).await {
                Some((jpeg, w, h, method)) => {
                    let (held_after, epoch_after) = live.lease(&desk);
                    if held_after || epoch_after != epoch {
                        *lock(&producer.refusal) = Some(Refusal::HumanHasControl);
                        producer.latest.send_replace(None);
                    } else {
                        *lock(&producer.refusal) = None;
                        let changed = producer
                            .latest
                            .borrow()
                            .as_ref()
                            .is_none_or(|last| last.jpeg != jpeg);
                        if changed {
                            seq += 1;
                            producer.latest.send_replace(Some(Arc::new(Shot {
                                seq,
                                jpeg,
                                width: w,
                                height: h,
                                method,
                                epoch,
                                at: Instant::now(),
                            })));
                        }
                    }
                }
                None => *lock(&producer.refusal) = Some(Refusal::CaptureFailed),
            }
        }
        let period = Duration::from_secs_f64(1.0 / fps.clamp(0.2, MAX_FPS));
        tokio::time::sleep(
            period
                .saturating_sub(started.elapsed())
                .max(Duration::from_millis(20)),
        )
        .await;
    }
    capture.remove(&key, &producer);
}

/// (jpeg, width, height, method) of one window, or `None`. Health is kept
/// per window, so one failing window never pushes the others onto the
/// fallback.
async fn capture_window(
    capture: &Capture,
    live: &Live,
    window_id: u32,
    max_width: u32,
) -> Option<(Vec<u8>, u32, u32, Method)> {
    let health = live.health(window_id);
    if cfg!(target_os = "macos")
        && health
            .failed_until
            .is_none_or(|until| Instant::now() >= until)
    {
        match screencapture(capture, window_id, max_width).await {
            Ok((jpeg, w, h)) => {
                live.set_health(window_id, |health| {
                    health.method = Some("window");
                    health.error = None;
                    health.at = Some(Instant::now());
                    health.failed_until = None;
                });
                return Some((jpeg, w, h, Method::Window));
            }
            Err(error) => live.set_health(window_id, |health| {
                health.error = Some(error);
                health.failed_until = Some(Instant::now() + CAPTURE_RETRY);
            }),
        }
    }
    let Some(png) = live.snapshot_png(window_id) else {
        live.set_health(window_id, |health| {
            health.error.get_or_insert_with(|| {
                "no window capture and no agent screenshot of this window yet".to_string()
            });
        });
        return None;
    };
    let encoded = tokio::task::spawn_blocking(move || {
        let image = image::load_from_memory_with_format(&png, ImageFormat::Png)
            .map_err(|error| format!("agent screenshot unreadable: {error}"))?;
        encode(image, max_width)
    })
    .await;
    match encoded {
        Ok(Ok((jpeg, w, h))) => {
            live.set_health(window_id, |health| {
                health.method = Some("snapshot");
                health.at = Some(Instant::now());
            });
            Some((jpeg, w, h, Method::Snapshot))
        }
        Ok(Err(error)) => {
            live.set_health(window_id, |health| health.error = Some(error));
            None
        }
        Err(error) => {
            tracing::debug!(%error, "computer-use snapshot encode task failed");
            None
        }
    }
}

fn encode(image: DynamicImage, max_width: u32) -> Result<(Vec<u8>, u32, u32), String> {
    let image = if image.width() > max_width {
        let height = (u64::from(image.height()) * u64::from(max_width)
            / u64::from(image.width().max(1)))
        .max(1);
        image.resize_exact(
            max_width,
            u32::try_from(height).unwrap_or(1),
            FilterType::Triangle,
        )
    } else {
        image
    };
    let rgb = image.to_rgb8();
    let mut out = Cursor::new(Vec::new());
    JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)
        .encode_image(&rgb)
        .map_err(|error| format!("frame encode failed: {error}"))?;
    Ok((out.into_inner(), rgb.width(), rgb.height()))
}

/// A window capture without Screen Recording permission comes back as one
/// flat colour.
fn looks_blank(image: &DynamicImage) -> bool {
    let small = image.resize_exact(16, 16, FilterType::Nearest).to_luma8();
    let (lo, hi) = small.pixels().fold((u8::MAX, u8::MIN), |(lo, hi), pixel| {
        (lo.min(pixel[0]), hi.max(pixel[0]))
    });
    hi.saturating_sub(lo) < 3
}

/// The capture directory, readable only by this user where the OS allows.
fn create_private_dir(dir: &Path) {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    if let Err(error) = builder.create(dir) {
        tracing::debug!(%error, "computer-use capture directory unavailable");
    }
}

/// macOS window capture by CGWindowID: works for covered windows, needs
/// Screen Recording for the IronClaw process.
async fn screencapture(
    capture: &Capture,
    window_id: u32,
    max_width: u32,
) -> Result<(Vec<u8>, u32, u32), String> {
    let path = capture.next_capture_path();
    let run = tokio::process::Command::new("/usr/sbin/screencapture")
        .arg("-x")
        .arg("-o")
        .arg(format!("-l{window_id}"))
        .args(["-t", "jpg"])
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output();
    let result = match tokio::time::timeout(SCREENCAPTURE_TIMEOUT, run).await {
        Err(_elapsed) => Err("screencapture timed out".to_string()),
        Ok(Err(error)) => Err(format!("screencapture failed: {error}")),
        Ok(Ok(done)) => {
            let bytes = tokio::fs::read(&path).await.unwrap_or_default(); // silent-ok: a missing file is reported below
            if !done.status.success() || bytes.len() < 1000 {
                let stderr = String::from_utf8_lossy(&done.stderr);
                let mut message: String = stderr.trim().chars().take(200).collect();
                if message.is_empty() {
                    message = "screencapture produced no image".to_string();
                }
                if message.contains("could not create image") {
                    message.push_str(
                        " (window closed, minimized, on another Space, or the screen is locked)",
                    );
                }
                Err(message)
            } else {
                tokio::task::spawn_blocking(move || {
                    let image = image::load_from_memory_with_format(&bytes, ImageFormat::Jpeg)
                        .map_err(|error| format!("screencapture image unreadable: {error}"))?;
                    if looks_blank(&image) {
                        return Err("the window capture came back blank: IronClaw may lack \
                                    Screen Recording permission, or the screen is locked"
                            .to_string());
                    }
                    encode(image, max_width)
                })
                .await
                .map_err(|error| format!("frame encode task failed: {error}"))?
            }
        }
    };
    // The capture file may not exist when screencapture failed. Any other
    // delete failure is retried, then swept by the next capture.
    capture.discard_capture(&path).await;
    result
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::leftovers::{FileRemover, Leftovers};

    struct FailingOnce(AtomicU64);

    impl FileRemover for FailingOnce {
        fn remove(&self, path: &Path) -> std::io::Result<()> {
            // Fails the first four calls (one attempt + three retries).
            if self.0.fetch_add(1, Ordering::SeqCst) < 4 {
                return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
            }
            std::fs::remove_file(path)
        }
    }

    #[tokio::test]
    async fn an_undeletable_capture_is_swept_by_the_next_capture() {
        let dir = tempfile::tempdir().unwrap();
        let leftovers = Arc::new(Leftovers::with_remover(
            Arc::new(FailingOnce(AtomicU64::new(0))),
            vec![Duration::from_millis(1); 3],
        ));
        let capture = Capture::in_dir(dir.path().join("captures"), leftovers, STALE_CAPTURE_AGE);

        let first = capture.next_capture_path();
        std::fs::write(&first, b"a screenshot").unwrap();
        capture.discard_capture(&first).await;
        assert!(first.exists(), "the injected failure kept the screenshot");

        let second = capture.next_capture_path();

        assert_ne!(first, second);
        assert!(
            !first.exists(),
            "the next capture must delete the leftover screenshot"
        );
    }

    #[test]
    fn startup_deletes_stale_captures_in_the_capture_directory() {
        let dir = tempfile::tempdir().unwrap();
        let captures = dir.path().join("captures");
        std::fs::create_dir_all(&captures).unwrap();
        let leftover = captures.join("4242_7.jpg");
        std::fs::write(&leftover, b"a screenshot from a crashed process").unwrap();

        let _capture = Capture::in_dir(captures, Arc::new(Leftovers::new()), Duration::ZERO);

        assert!(!leftover.exists());
    }

    #[cfg(unix)]
    #[test]
    fn the_capture_directory_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let capture = Capture::in_dir(
            dir.path().join("captures"),
            Arc::new(Leftovers::new()),
            STALE_CAPTURE_AGE,
        );

        let path = capture.next_capture_path();

        let mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}
