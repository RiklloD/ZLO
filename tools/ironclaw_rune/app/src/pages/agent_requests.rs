//! Cards for requests agents raise (a secret, a machine to link) and the
//! Computers page's "Linked environments" section. Behaviour lives in
//! `remote::linked`; this file only draws.

use rune::{El, col, row, text};
use rune_kit::Kit;

use super::common::controls::{input, muted, press_button, styled};
use super::common::ts::{MONO_XS, sm_medium, xs};
use super::common::{Btn, Tone, error_line, notice_line, rows_card, tag};
use super::flash;
use crate::design::motion;
use crate::remote::linked::{self, access_label, preference_label};
use crate::state::State;
use zlo_client::peer::{AgentRequestKind, AgentRequestView, SecretDelivery};

/// Pending requests, those of `thread` only when given.
pub fn cards(s: &State, k: &Kit, thread: Option<&str>) -> Vec<El> {
    s.remote
        .linked
        .requests
        .iter()
        .filter(|r| thread.is_none_or(|t| r.thread_id == t))
        .map(|r| card(s, k, r))
        .collect()
}

fn card(s: &State, k: &Kit, r: &AgentRequestView) -> El {
    let p = &s.palette;
    let id = r.request_id.clone();
    let busy = s.remote.linked.answering.as_deref() == Some(id.as_str());
    // Keyed by request: one an agent raises lands at once (a poll); one the
    // person answers folds away and the rest glide up.
    let mut body = col()
        .key(format!("agent-request-{id}"))
        .motion(motion::ROW_RESORT)
        .transition(motion::reveal())
        .w_full()
        .padding(14.)
        .gap(8.)
        .radius(10.)
        .border(1., p.border)
        .fill(p.surface);
    let from = r
        .thread_title
        .clone()
        .map(|t| format!("From “{t}”"))
        .unwrap_or_else(|| "From an agent".into());
    match &r.kind {
        AgentRequestKind::Secret {
            label,
            reason,
            url,
            placeholder,
            delivery,
        } => {
            body = body
                .child(
                    row()
                        .gap(8.)
                        .center_y()
                        .child(rune::icon(&crate::icons::SHIELD_CHECK, 15.).color(p.text_muted))
                        .child(text(label.clone()).font(sm_medium()).color(p.text))
                        .child(tag(p, "Secret", Tone::Secondary)),
                )
                .child(styled(reason.clone(), xs(), p.text).wrap())
                .child(styled(
                    match delivery {
                        SecretDelivery::SecretRef => {
                            "The value never enters the chat, and the agent never sees it: a tool uses it once."
                        }
                        SecretDelivery::File => {
                            "The agent's commands read it from a private file for 24 hours. It never enters the chat."
                        }
                    },
                    xs(),
                    p.text_muted,
                ).wrap());
            let field_id = id.clone();
            let submit_id = id.clone();
            let mut field = input(
                s.remote
                    .linked
                    .secret_inputs
                    .get(&id)
                    .unwrap_or(&EMPTY_SECRET),
            )
            .placeholder(placeholder.clone().unwrap_or_else(|| {
                "Value, or a 1Password reference (op://vault/item/field)".into()
            }))
            .label(label.clone())
            .on_edit(move |s: &mut State| linked::secret_input(s, &field_id))
            .on_submit(move |s: &mut State, _: &mut rune::Tx| {
                linked::save_secret(s, submit_id.clone())
            })
            .build(k)
            .key(format!("secret-input-{id}"));
            field = field.grow(1.).min_w(0.);
            let save_id = id.clone();
            let decline_id = id.clone();
            let mut actions = row().gap(6.).center_y().child(field).child(press_button(
                k,
                &format!("secret-save-{id}"),
                "Save securely",
                Btn::Primary,
                None,
                busy,
                move |s| linked::save_secret(s, save_id.clone()),
            ));
            actions = actions.child(press_button(
                k,
                &format!("secret-decline-{id}"),
                "Decline",
                Btn::Ghost,
                None,
                false,
                move |s| linked::decline(s, decline_id.clone()),
            ));
            body = body.child(actions);
            if let Some(url) = url.clone() {
                body = body.child(
                    super::common::button("Open setup page", Btn::Ghost)
                        .icon(&crate::icons::EXTERNAL_LINK)
                        .on_press_tx(move |_: &mut State, tx: &mut rune::Tx| {
                            super::common::controls::open_url(tx, &url)
                        })
                        .build(k)
                        .key(format!("secret-url-{id}"))
                        .press_scale(motion::PRESS),
                );
            }
        }
        AgentRequestKind::EnvironmentLink {
            hint,
            environment_id,
            url,
            access,
        } => {
            let wanted = access.unwrap_or(s.remote.linked.access.0);
            let title = match hint {
                Some(h) => format!("Link “{h}” so agents here can work there"),
                None => "Link another computer so agents here can work there".into(),
            };
            body = body
                .child(
                    row()
                        .gap(8.)
                        .center_y()
                        .child(rune::icon(&crate::icons::NETWORK, 15.).color(p.text_muted))
                        .child(text(title).font(sm_medium()).color(p.text).wrap())
                        .child(tag(p, access_label(wanted), Tone::Secondary)),
                )
                .child(styled(
                    "Pick a linked computer, or enter the other computer's address and a link code made there (Settings → Computers → Make a link code, or `zlo pair --link operate`). Only paste a code for an address you trust: it is sent there.",
                    xs(),
                    p.text_muted,
                ).wrap());
            // Already-linked computers: one click.
            if let Some(Ok(links)) = &s.remote.linked.links {
                let mut picks = row().gap(6.).flex_wrap();
                for l in links.iter().filter(|l| !l.revoked) {
                    let (rid, env) = (id.clone(), l.environment_id.clone());
                    let preselected = environment_id.as_deref() == Some(l.environment_id.as_str())
                        || hint.as_deref().is_some_and(|h| {
                            l.label
                                .to_ascii_lowercase()
                                .contains(&h.to_ascii_lowercase())
                        });
                    picks = picks.child(press_button(
                        k,
                        &format!("link-pick-{id}-{}", l.environment_id),
                        &l.label,
                        if preselected {
                            Btn::Primary
                        } else {
                            Btn::Secondary
                        },
                        None,
                        busy,
                        move |s| linked::answer_with_link(s, rid.clone(), env.clone(), wanted),
                    ));
                }
                body = body.child(picks);
            }
            let (uid, cid, nid) = (id.clone(), id.clone(), id.clone());
            let url_edit = s.remote.linked.request_urls.get(&id);
            let code_edit = s.remote.linked.request_codes.get(&id);
            let url_placeholder = url
                .clone()
                .unwrap_or_else(|| "https://vps.tailnet.ts.net".into());
            body = body.child(
                row()
                    .w_full()
                    .gap(6.)
                    .center_y()
                    .child(
                        input(url_edit.unwrap_or(&EMPTY))
                            .placeholder(url_placeholder)
                            .label("Address")
                            .on_edit(move |s: &mut State| linked::request_url(s, &uid))
                            .build(k)
                            .key(format!("link-url-{id}"))
                            .grow(1.)
                            .min_w(0.),
                    )
                    .child(
                        input(code_edit.unwrap_or(&EMPTY))
                            .placeholder("7KQ2-M9XD-4TRA")
                            .label("Link code")
                            .on_edit(move |s: &mut State| linked::request_code(s, &cid))
                            .build(k)
                            .key(format!("link-code-{id}"))
                            .w(150.),
                    )
                    .child(press_button(
                        k,
                        &format!("link-new-{id}"),
                        "Link",
                        Btn::Primary,
                        None,
                        busy,
                        move |s| linked::answer_with_new_link(s, nid.clone()),
                    )),
            );
            let decline_id = id.clone();
            body = body.child(press_button(
                k,
                &format!("link-decline-{id}"),
                "Decline",
                Btn::Ghost,
                None,
                false,
                move |s| linked::decline(s, decline_id.clone()),
            ));
        }
    }
    if let Some(e) = s.remote.linked.request_errors.get(&id) {
        body = body.child(
            styled(e.clone(), xs(), p.danger)
                .wrap()
                .key("error")
                .transition(motion::reveal()),
        );
    }
    body.child(styled(from, xs(), p.text_muted))
}

static EMPTY: std::sync::LazyLock<rune::TextEdit> =
    std::sync::LazyLock::new(|| rune::TextEdit::new(""));
static EMPTY_SECRET: std::sync::LazyLock<rune::TextEdit> =
    std::sync::LazyLock::new(|| rune::TextEdit::new("").secure());

/// Settings → Computers → Linked environments.
pub fn linked_section(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let l = &s.remote.linked;
    let mut card = col().w_full().padding(16.).gap(10.);
    if let Some(e) = &l.error {
        card = card.child(error_line(p, e).key("linked-error"));
    }
    if let Some(n) = &l.notice {
        card = card.child(notice_line(n, p.success).key("linked-notice"));
    }
    match &l.links {
        None => {
            card = card.child(muted(
                p,
                if l.loading {
                    "Loading…"
                } else {
                    "Not loaded."
                },
            ))
        }
        Some(Err(e)) => card = card.child(styled(e.clone(), xs(), p.danger).wrap()),
        Some(Ok(links)) if links.is_empty() => {
            card = card.child(muted(p, "No linked computers yet."));
        }
        Some(Ok(links)) => {
            for link in links {
                let env = link.environment_id.clone();
                let pref = link.preference;
                let mut caption = format!(
                    "{} · until {}",
                    access_label(link.access),
                    link.expires_at.format("%Y-%m-%d")
                );
                if let Some(url) = link.urls.first() {
                    caption.push_str(&format!(" · {url}"));
                }
                let confirming = l.confirm_forget.as_deref() == Some(env.as_str());
                let (e1, e2, e3) = (env.clone(), env.clone(), env.clone());
                let forget = if confirming {
                    press_button(
                        k,
                        &format!("forget-confirm-{env}"),
                        "Forget here",
                        Btn::Danger,
                        None,
                        false,
                        move |s| linked::forget(s, e1.clone()),
                    )
                } else {
                    press_button(
                        k,
                        &format!("forget-{env}"),
                        "Forget",
                        Btn::Ghost,
                        None,
                        false,
                        move |s| linked::ask_forget(s, e2.clone()),
                    )
                };
                card = card.child(
                    row()
                        .key(format!("link-{env}"))
                        .motion(motion::ROW_RESORT)
                        .w_full()
                        .gap(10.)
                        .center_y()
                        .child(
                            col()
                                .grow(1.)
                                .min_w(0.)
                                .gap(2.)
                                .child(
                                    row()
                                        .gap(8.)
                                        .center_y()
                                        .child(
                                            text(link.label.clone())
                                                .font(sm_medium())
                                                .color(p.text),
                                        )
                                        .children(
                                            link.revoked.then(|| tag(p, "Revoked", Tone::Danger)),
                                        ),
                                )
                                .child(styled(caption, xs(), p.text_muted).truncate().min_w(0.)),
                        )
                        .child(press_button(
                            k,
                            &format!("pref-{env}"),
                            &format!("Balance: {}", preference_label(pref)),
                            Btn::Secondary,
                            None,
                            false,
                            move |s| linked::cycle_preference(s, e3.clone(), pref),
                        ))
                        .child(forget),
                );
            }
        }
    }
    // Link another computer with a code made there.
    card = card
        .child(styled("Link a computer", xs(), p.text_muted))
        .child(
            row()
                .w_full()
                .gap(8.)
                .center_y()
                .child(
                    input(&l.url_input)
                        .placeholder("https://vps.tailnet.ts.net")
                        .label("Its address")
                        .on_edit(|s: &mut State| &mut s.remote.linked.url_input)
                        .build(k)
                        .key("link-url")
                        .grow(1.)
                        .min_w(0.),
                )
                .child(
                    input(&l.code_input)
                        .placeholder("Link code")
                        .label("Link code")
                        .on_edit(|s: &mut State| &mut s.remote.linked.code_input)
                        .on_submit(|s: &mut State, _: &mut rune::Tx| linked::link(s))
                        .build(k)
                        .key("link-code")
                        .w(150.),
                )
                .child(press_button(
                    k,
                    "access-pick",
                    access_label(l.access.0),
                    Btn::Secondary,
                    None,
                    false,
                    linked::cycle_access,
                ))
                .child(press_button(
                    k,
                    "link-submit",
                    "Link",
                    Btn::Primary,
                    None,
                    l.linking,
                    linked::link,
                )),
        );
    // Let another computer link this one.
    let mut make = row().w_full().gap(10.).center_y().child(
        muted(
            p,
            "Let another computer's agents work here: make a one-time link code with the access picked above.",
        )
        .grow(1.),
    );
    make = make.child(press_button(
        k,
        "make-link-code",
        "Make a link code",
        Btn::Secondary,
        None,
        l.issuing,
        linked::issue_link_code,
    ));
    card = card.child(make);
    if let Some(code) = &l.link_code {
        let shown = code.code.clone();
        let copied = flash::shows(s, "copy-link-code");
        // Wipes in when the person makes a link code.
        card = card.child(
            row()
                .key("link-code-shown")
                .transition(motion::reveal())
                .gap(12.)
                .center_y()
                .child(
                    text(code.code.clone())
                        .font(rune::TextStyle::new(20., 600.))
                        .color(p.text)
                        .selectable(true),
                )
                .child(styled(
                    format!(
                        "{} · expires {}",
                        access_label(code.access),
                        crate::jobs::when(code.expires_at)
                    ),
                    xs(),
                    p.text_muted,
                ))
                .child(
                    // "Copied" with a check for a moment, then back.
                    super::common::button(if copied { "Copied" } else { "Copy" }, Btn::Ghost)
                        .icon(if copied {
                            &crate::icons::CHECK
                        } else {
                            &crate::icons::COPY
                        })
                        .rolling("copy-link-code-label")
                        .on_press_tx(move |s: &mut State, tx: &mut rune::Tx| {
                            tx.copy(shown.clone());
                            flash::flash(s, "copy-link-code", tx);
                        })
                        .build(k)
                        .key("copy-link-code")
                        .press_scale(motion::PRESS),
                ),
        );
    }
    card = card.child(styled(
        "Agents pass environment=<label> to t3_* tools, or t3_delegate environment=auto to pick the computer with the most headroom (Balance sets each one's share).",
        MONO_XS,
        p.text_muted,
    ).wrap());
    let mut out = col().w_full().gap(8.).child(rows_card(p, [card]));
    let pending = cards(s, k, None);
    if !pending.is_empty() {
        out = out
            .child(styled("Waiting for you", xs(), p.text_muted))
            .children(pending);
    }
    out
}
