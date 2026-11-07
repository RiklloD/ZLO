//! Console: run a shell command in the project folder and watch its output
//! (GPUI `chat/side/console.rs`). The light stand-in for MonoCode's
//! terminal tab: a login `zsh -lc` per command with stdout and stderr
//! streamed line by line, Stop to end it, ↑/↓ through earlier commands. No
//! PTY, so full-screen programs (vim, top) don't belong here; everything
//! that prints lines does.

use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Instant;

use rune::{El, Key, KeyEvent, ScrollSpec, TextEdit, Tx, col, el, row, text};
use rune_kit::{ButtonVariant, Kit, TextInput};

use super::ui::{self, MONO_XS, XS};
use super::{short_path, side, side_mut, side_of, side_total, thread_id};
use crate::design::smooth_scroll::WheelGlide;
use crate::icons;
use crate::state::State;

/// Oldest lines go first past this.
const LINES_MAX: usize = 4000;
const OUT_KEY: &str = "console-out";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Kind {
    Command,
    Out,
    Err,
    Info,
}

enum Msg {
    Line(Kind, String),
    Exit(Option<i32>),
}

struct Running {
    pid: u32,
    started: Instant,
}

#[derive(Default)]
pub(super) struct ConsoleState {
    input: TextEdit,
    pub(super) lines: Vec<(Kind, String)>,
    running: Option<Running>,
    history: Vec<String>,
    /// Position while stepping through `history` with the arrows.
    recall: Option<usize>,
}

/// Drop terminal escape sequences (colors, cursor moves, titles).
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            if c != '\r' {
                out.push(c);
            }
            continue;
        }
        match chars.next() {
            // CSI: parameters, then one final byte in @..~.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: up to BEL or ESC \.
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn pump(
    reader: impl Read + Send + 'static,
    kind: Kind,
    tx: mpsc::Sender<Msg>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut buf = Vec::new();
        while reader
            .read_until(b'\n', &mut buf)
            .map(|n| n > 0)
            .unwrap_or(false)
        {
            let line = String::from_utf8_lossy(&buf);
            let line = strip_ansi(line.trim_end_matches('\n'));
            if tx.send(Msg::Line(kind, line)).is_err() {
                break;
            }
            buf.clear();
        }
    })
}

/// Start `command` under a login shell in its own process group, so Stop
/// ends whatever it spawned too.
fn spawn(command: &str, dir: &Path) -> Result<(u32, mpsc::Receiver<Msg>), String> {
    let mut child = Command::new("/bin/zsh")
        .arg("-lc")
        .arg(command)
        .current_dir(dir)
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .env("CLICOLOR", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("Couldn't start the shell: {e}"))?;
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    let out = child.stdout.take().map(|s| pump(s, Kind::Out, tx.clone()));
    let err = child.stderr.take().map(|s| pump(s, Kind::Err, tx.clone()));
    std::thread::spawn(move || {
        for reader in [out, err].into_iter().flatten() {
            let _ = reader.join();
        }
        let code = child.wait().ok().and_then(|s| s.code());
        let _ = tx.send(Msg::Exit(code));
    });
    Ok((pid, rx))
}

fn push(state: &mut ConsoleState, kind: Kind, text: String) {
    state.lines.push((kind, text));
    if state.lines.len() > LINES_MAX {
        let drop = state.lines.len() - LINES_MAX;
        state.lines.drain(..drop);
    }
}

fn run(s: &mut State, tx: &mut Tx) {
    let Some(thread) = thread_id(s) else { return };
    let Some(x) = side_mut(s) else { return };
    let command = x.console.input.text.trim().to_string();
    if command.is_empty() || x.console.running.is_some() {
        return;
    }
    if command == "clear" {
        x.console.lines.clear();
        x.console.input.clear();
        return;
    }
    let Some(dir) = x.folder.clone() else { return };
    x.console.input.clear();
    let state = &mut x.console;
    state.history.retain(|h| *h != command);
    state.history.push(command.clone());
    state.recall = None;
    push(state, Kind::Command, command.clone());
    match spawn(&command, &dir) {
        Err(e) => push(state, Kind::Err, e),
        Ok((pid, rx)) => {
            state.running = Some(Running {
                pid,
                started: Instant::now(),
            });
            forward(s, thread, rx);
        }
    }
    tx.scroll_to_bottom(OUT_KEY);
}

/// Hand the command's output to the UI thread in batches.
fn forward(s: &State, thread: String, rx: mpsc::Receiver<Msg>) {
    let handle = s.rt.handle.clone();
    std::thread::spawn(move || {
        while let Ok(first) = rx.recv() {
            let mut batch = vec![first];
            batch.extend(rx.try_iter());
            let thread = thread.clone();
            handle.update(move |s, tx| {
                let Some(x) = side_of(s, &thread) else { return };
                let state = &mut x.console;
                for msg in batch {
                    match msg {
                        Msg::Line(kind, text) => push(state, kind, text),
                        Msg::Exit(code) => {
                            let secs = state
                                .running
                                .take()
                                .map(|r| r.started.elapsed().as_secs_f32())
                                .unwrap_or_default();
                            let text = match code {
                                Some(0) => format!("Done in {secs:.1} s"),
                                Some(c) => format!("Exited with {c} after {secs:.1} s"),
                                None => "Stopped".into(),
                            };
                            push(state, Kind::Info, text);
                        }
                    }
                }
                tx.scroll_to_bottom(OUT_KEY);
            });
        }
    });
}

fn stop(s: &mut State) {
    if let Some(run) = side(s).and_then(|x| x.console.running.as_ref()) {
        // The negative pid names the whole process group.
        let _ = Command::new("/bin/kill")
            .args(["-TERM", "--", &format!("-{}", run.pid)])
            .status();
    }
}

/// ↑ and ↓ walk the command history into the input.
fn recall(s: &mut State, up: bool) -> bool {
    let Some(x) = side_mut(s) else { return false };
    let state = &mut x.console;
    if state.history.is_empty() {
        return false;
    }
    let last = state.history.len() - 1;
    let next = match (state.recall, up) {
        (None, true) => Some(last),
        (None, false) => return false,
        (Some(i), true) => Some(i.saturating_sub(1)),
        (Some(i), false) if i >= last => None,
        (Some(i), false) => Some(i + 1),
    };
    state.recall = next;
    let text = next
        .and_then(|i| state.history.get(i).cloned())
        .unwrap_or_default();
    state.input.replace_text(text);
    true
}

pub(super) fn view(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let Some(x) = side(s) else {
        return ui::loading(k);
    };
    let state = &x.console;
    let folder = x.folder.as_deref().map(short_path).unwrap_or_default();
    let running = state.running.is_some();
    let mut lines = col().w_full().px(12.).py(8.);
    if state.lines.is_empty() {
        lines = lines.child(ui::para(
            format!("Commands run in {folder} with your login shell. Try `git log --oneline -5` or `npm test`."),
            XS,
            p.text_faint,
        ));
    }
    for (ix, (kind, line)) in state.lines.iter().enumerate() {
        let (content, color, top) = match kind {
            Kind::Command => (format!("$ {line}"), p.accent, 4.),
            Kind::Out => (line.clone(), p.text, 0.),
            Kind::Err => (line.clone(), p.danger.alpha(0.9), 0.),
            Kind::Info => (line.clone(), p.text_faint, 0.),
        };
        lines = lines.child(
            text(content)
                .font(MONO_XS)
                .color(color)
                .wrap()
                .wrap_anywhere()
                .min_h(16.)
                .mt(top)
                .key(format!("console-line-{ix}")),
        );
    }
    let out = col()
        .key(OUT_KEY)
        .w_full()
        .grow(1.)
        .min_h(0.)
        .scroll(ScrollSpec { stick_bottom: true })
        .wheel_glide(OUT_KEY)
        .child(lines);
    let action = if running {
        ui::btn("Stop")
            .icon(&icons::SQUARE)
            .variant(ButtonVariant::Danger)
            .on_press(stop)
            .build(k)
            .key("console-stop")
    } else {
        ui::ibtn(&icons::PLAY, "Run")
            .on_press_tx(run)
            .build(k)
            .key("console-run")
    };
    let input = row()
        .w_full()
        .px(8.)
        .py(8.)
        .gap(4.)
        .items_center()
        .child(ui::line("$", XS, p.accent))
        .child(
            row().grow(1.).min_w(0.).child(
                TextInput::new(&state.input)
                    .placeholder("Run a command…")
                    .label("Command")
                    .on_edit(|s: &mut State| &mut side_total(s).console.input)
                    .on_submit(run)
                    .on_key(|s: &mut State, ev: &KeyEvent, _| match ev.key {
                        Key::Up => recall(s, true),
                        Key::Down => recall(s, false),
                        _ => false,
                    })
                    .build(k)
                    .key("console-input"),
            ),
        )
        .child(action)
        .child(
            ui::ibtn(&icons::DELETE, "Clear")
                .label("Clear output")
                .on_press(|s: &mut State| {
                    if let Some(x) = side_mut(s) {
                        x.console.lines.clear();
                    }
                })
                .build(k)
                .key("console-clear"),
        );
    ui::fill_col()
        .child(out)
        .child(el().w_full().h(1.).fill(p.border))
        .child(input)
}

#[cfg(test)]
mod tests {
    use super::strip_ansi;

    #[test]
    fn escape_sequences_are_dropped() {
        assert_eq!(strip_ansi("\u{1b}[1;31merror\u{1b}[0m: x\r"), "error: x");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}ok"), "ok");
    }
}
