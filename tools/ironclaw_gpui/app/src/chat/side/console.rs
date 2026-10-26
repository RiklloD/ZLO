//! Console: run a shell command in the project folder and watch its output.
//! The light stand-in for MonoCode's terminal tab: a login `zsh -lc` per
//! command with stdout and stderr streamed line by line, Stop to end it,
//! ↑/↓ through earlier commands. No PTY, so full-screen programs (vim,
//! top) don't belong here; everything that prints lines does.

use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Instant;

use futures::StreamExt;
use futures::channel::mpsc;

use super::{ChatView, short_path};
use crate::design::Palette;
use crate::ui::*;

/// Oldest lines go first past this.
const LINES_MAX: usize = 4000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
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
pub(super) struct State {
    input: Option<(Entity<InputState>, Subscription)>,
    lines: Vec<(Kind, String)>,
    running: Option<Running>,
    history: Vec<String>,
    /// Position while stepping through `history` with the arrows.
    recall: Option<usize>,
    scroll: gpui_kit::ScrollHandle,
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
    tx: mpsc::UnboundedSender<Msg>,
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
            if tx.unbounded_send(Msg::Line(kind, line)).is_err() {
                break;
            }
            buf.clear();
        }
    })
}

/// Start `command` under a login shell in its own process group, so Stop
/// ends whatever it spawned too.
fn spawn(command: &str, dir: &PathBuf) -> Result<(u32, mpsc::UnboundedReceiver<Msg>), String> {
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
    let (tx, rx) = mpsc::unbounded();
    let out = child.stdout.take().map(|s| pump(s, Kind::Out, tx.clone()));
    let err = child.stderr.take().map(|s| pump(s, Kind::Err, tx.clone()));
    std::thread::spawn(move || {
        for reader in [out, err].into_iter().flatten() {
            let _ = reader.join();
        }
        let code = child.wait().ok().and_then(|s| s.code());
        let _ = tx.unbounded_send(Msg::Exit(code));
    });
    Ok((pid, rx))
}

impl ChatView {
    fn console_push(&mut self, kind: Kind, text: String) {
        let lines = &mut self.side.console.lines;
        lines.push((kind, text));
        if lines.len() > LINES_MAX {
            let drop = lines.len() - LINES_MAX;
            lines.drain(..drop);
        }
    }

    fn console_run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((input, _)) = self.side.console.input.as_ref() else {
            return;
        };
        let command = input.read(cx).value().trim().to_string();
        if command.is_empty() || self.side.console.running.is_some() {
            return;
        }
        if command == "clear" {
            self.side.console.lines.clear();
            input.update(cx, |s, cx| s.set_value("", window, cx));
            cx.notify();
            return;
        }
        let Some(dir) = self.side.folder.clone() else {
            return;
        };
        input.update(cx, |s, cx| s.set_value("", window, cx));
        let state = &mut self.side.console;
        state.history.retain(|h| *h != command);
        state.history.push(command.clone());
        state.recall = None;
        self.console_push(Kind::Command, command.clone());
        match spawn(&command, &dir) {
            Err(e) => self.console_push(Kind::Err, e),
            Ok((pid, mut rx)) => {
                self.side.console.running = Some(Running {
                    pid,
                    started: Instant::now(),
                });
                cx.spawn(async move |this, cx| {
                    while let Some(first) = rx.next().await {
                        let mut batch = vec![first];
                        while let Ok(next) = rx.try_recv() {
                            batch.push(next);
                        }
                        let alive = this.update(cx, |this, cx| {
                            for msg in batch {
                                match msg {
                                    Msg::Line(kind, text) => this.console_push(kind, text),
                                    Msg::Exit(code) => {
                                        let secs = this
                                            .side
                                            .console
                                            .running
                                            .take()
                                            .map(|r| r.started.elapsed().as_secs_f32())
                                            .unwrap_or_default();
                                        let text = match code {
                                            Some(0) => format!("Done in {secs:.1} s"),
                                            Some(c) => format!("Exited with {c} after {secs:.1} s"),
                                            None => "Stopped".into(),
                                        };
                                        this.console_push(Kind::Info, text);
                                    }
                                }
                            }
                            this.side.console.scroll.scroll_to_bottom();
                            cx.notify();
                        });
                        if alive.is_err() {
                            break;
                        }
                    }
                })
                .detach();
            }
        }
        self.side.console.scroll.scroll_to_bottom();
        cx.notify();
    }

    fn console_stop(&mut self) {
        if let Some(run) = self.side.console.running.as_ref() {
            // The negative pid names the whole process group.
            let _ = Command::new("/bin/kill")
                .args(["-TERM", "--", &format!("-{}", run.pid)])
                .status();
        }
    }

    /// ↑ and ↓ walk the command history into the input.
    fn console_recall(&mut self, up: bool, window: &mut Window, cx: &mut Context<Self>) {
        let state = &mut self.side.console;
        if state.history.is_empty() {
            return;
        }
        let last = state.history.len() - 1;
        let next = match (state.recall, up) {
            (None, true) => Some(last),
            (None, false) => return,
            (Some(i), true) => Some(i.saturating_sub(1)),
            (Some(i), false) if i >= last => None,
            (Some(i), false) => Some(i + 1),
        };
        state.recall = next;
        let text = next.map(|i| state.history[i].clone()).unwrap_or_default();
        if let Some((input, _)) = state.input.as_ref() {
            input.update(cx, |s, cx| s.set_value(text, window, cx));
        }
    }

    pub(super) fn render_console(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = *Palette::of(cx);
        if self.side.console.input.is_none() {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("Run a command…"));
            let sub = cx.subscribe_in(&input, window, |this, _, event, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.console_run(window, cx);
                }
            });
            self.side.console.input = Some((input, sub));
        }
        let state = &self.side.console;
        let mono = cx.theme().mono_font_family.clone();
        let folder = self
            .side
            .folder
            .as_deref()
            .map(short_path)
            .unwrap_or_default();
        let running = state.running.is_some();
        let lines = v_flex()
            .id("console-out")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&state.scroll)
            .px_3()
            .py_2()
            .font_family(mono)
            .text_xs()
            .when(state.lines.is_empty(), |d| {
                d.child(
                    div()
                        .text_color(p.text_faint)
                        .whitespace_normal()
                        .child(format!("Commands run in {folder} with your login shell. Try `git log --oneline -5` or `npm test`.")),
                )
            })
            .children(state.lines.iter().map(|(kind, text)| {
                let line = div().whitespace_normal().min_h(px(16.));
                match kind {
                    Kind::Command => line
                        .mt_1()
                        .text_color(p.accent)
                        .child(format!("$ {text}")),
                    Kind::Out => line.text_color(p.text).child(text.clone()),
                    Kind::Err => line.text_color(p.danger.opacity(0.9)).child(text.clone()),
                    Kind::Info => line.text_color(p.text_faint).child(text.clone()),
                }
            }));
        let Some((input, _)) = state.input.as_ref() else {
            return div().into_any_element();
        };
        v_flex()
            .flex_1()
            .min_h_0()
            .child(lines)
            .child(
                h_flex()
                    .px_2()
                    .py_2()
                    .gap_1()
                    .items_center()
                    .border_t_1()
                    .border_color(p.border)
                    .capture_key_down(cx.listener(|this, e: &KeyDownEvent, window, cx| {
                        match e.keystroke.key.as_str() {
                            "up" => {
                                cx.stop_propagation();
                                this.console_recall(true, window, cx);
                            }
                            "down" => {
                                cx.stop_propagation();
                                this.console_recall(false, window, cx);
                            }
                            _ => {}
                        }
                    }))
                    .child(div().text_xs().text_color(p.accent).child("$"))
                    .child(div().flex_1().child(Input::new(input).small()))
                    .child(if running {
                        Button::new("console-stop")
                            .label("Stop")
                            .icon(IconName::Square)
                            .xsmall()
                            .danger()
                            .on_click(cx.listener(|this, _, _, _| this.console_stop()))
                            .into_any_element()
                    } else {
                        Button::new("console-run")
                            .icon(IconName::Play)
                            .xsmall()
                            .ghost()
                            .tooltip("Run")
                            .accessibility_label("Run")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.console_run(window, cx)),
                            )
                            .into_any_element()
                    })
                    .child(
                        Button::new("console-clear")
                            .icon(IconName::Delete)
                            .xsmall()
                            .ghost()
                            .tooltip("Clear")
                            .accessibility_label("Clear output")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.side.console.lines.clear();
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }
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
