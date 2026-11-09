//! Settings → Computers (T3 Code's environments page, on Tailscale): this
//! Mac with "Run IronClaw on this Mac", the saved computers with their
//! state, Add a computer (found on the tailnet, or a pasted link or code),
//! and for the computer in use its pairing codes and paired devices.
//! Behaviour lives in `remote::pairing`; this file only draws.

use rune::{Cx, El, col, row, text};
use rune_kit::Kit;

use super::common::controls::{input, muted, press_button, styled};
use super::common::ts::{MONO_XS, sm_medium, xs};
use super::common::{
    Btn, Tone, error_line, group_header, header, notice_line, page_body, refresh_button, rows_card,
    setting_row, switch, tag,
};
use crate::remote::pairing::{self, Issued};
use crate::remote::updates::{self, RowState};
use crate::remote::{self, EnvState};
use crate::state::State;
use zlo_client::environments::{Active, SavedEnvironment, THIS_MAC};

pub mod fixtures;

pub fn opened(s: &mut State) {
    pairing::opened(s);
}

fn refresh(s: &mut State) {
    pairing::opened(s);
}

pub fn view(s: &State, _cx: &mut Cx<State>, k: &Kit, _layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let page = &s.remote.page;
    let mut body = col()
        .w_full()
        .child(header(p, "Computers", refresh_button(k, refresh)));
    if let Some(e) = &page.error {
        body = body.child(error_line(p, e));
    }
    if let Some(n) = &page.notice {
        body = body.child(notice_line(n, p.success));
    }
    // A Light bundle has no IronClaw of its own: This Mac is not offered.
    if !s.remote.app_only {
        body = body
            .child(group_header(
                p,
                "This Mac",
                Some("ZLO works on one computer at a time."),
            ))
            .child(this_mac(s, k));
    }
    if !remote::saved(s).is_empty() {
        body = body
            .child(group_header(p, "Other computers", None))
            .child(rows_card(
                p,
                remote::saved(s).iter().map(|env| saved_row(s, k, env)),
            ));
    }
    body = body
        .child(group_header(
            p,
            "Updates",
            Some("Each computer updates itself from its own source checkout, others first, this Mac last."),
        ))
        .child(updates_card(s, k));
    body = body
        .child(group_header(
            p,
            "Add a computer",
            Some("Pair another computer that runs IronClaw. Over Tailscale it stays private and encrypted end to end."),
        ))
        .child(rows_card(p, [col().w_full().padding(16.).child(add_computer(s, k))]));
    if s.rt.backend.is_some() || s.fixture {
        let label = s.onboarding.computer.label();
        body = body
            .child(group_header(
                p,
                "Pair a device",
                Some(&format!(
                    "Let another Mac use {label}: make a one-time code there, or open the link."
                )),
            ))
            .child(pair_device(s, k))
            .child(group_header(
                p,
                "Paired devices",
                Some("Devices with their own token for this computer. Revoking one signs it out for good."),
            ))
            .child(devices(s, k));
    }
    page_body("computers-page", body)
}

fn state_tag(s: &State, st: &EnvState) -> Option<El> {
    let p = &s.palette;
    Some(match st {
        EnvState::Connected => tag(p, "Connected", Tone::Success),
        EnvState::Connecting => tag(p, "Connecting…", Tone::Secondary),
        EnvState::Asleep => tag(p, "Asleep", Tone::Secondary),
        EnvState::Blocked(_) => tag(p, "Not connected", Tone::Danger),
        EnvState::Saved => return None,
    })
}

/// A computer's name, caption, state and actions.
fn computer_row(
    s: &State,
    key: String,
    title: String,
    caption: Option<String>,
    st: &EnvState,
    actions: Vec<El>,
) -> El {
    let p = &s.palette;
    let mut info = col().grow(1.).min_w(0.).gap(2.).child(
        row()
            .gap(8.)
            .center_y()
            .child(
                text(title)
                    .font(sm_medium())
                    .color(p.text)
                    .truncate()
                    .min_w(0.),
            )
            .children(state_tag(s, st)),
    );
    if let Some(c) = caption {
        info = info.child(styled(c, xs(), p.text_muted).truncate().min_w(0.));
    }
    if let EnvState::Blocked(why) = st {
        info = info.child(styled(why.clone(), xs(), p.danger).wrap());
    }
    row()
        .key(key)
        .w_full()
        .min_h(60.)
        .px(16.)
        .py(12.)
        .gap(12.)
        .center_y()
        .child(rune::icon(&crate::icons::MONITOR, 16.).color(p.text_muted))
        .child(info)
        .child(row().gap(6.).center_y().children(actions))
}

fn this_mac(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let st = remote::state_of(s, THIS_MAC);
    let active = !remote::backend_is_remote(s) && s.remote.switching.is_none();
    let mut actions = Vec::new();
    if !active && !matches!(st, EnvState::Connecting) {
        actions.push(press_button(
            k,
            "use-this-mac",
            "Use",
            Btn::Secondary,
            None,
            false,
            |s| remote::switch_to(s, Active::ThisMac),
        ));
    }
    let runs = s.remote.registry.run_local_backend;
    let caption = if remote::env_override() {
        Some("IRONCLAW_DESKTOP_URL decides the gateway.".to_string())
    } else {
        None
    };
    let mut rows = vec![
        col()
            .w_full()
            .child(computer_row(
                s,
                "computer-this-mac".into(),
                "This Mac".into(),
                caption,
                &st,
                actions,
            ))
            .children(update_line(s, THIS_MAC)),
        setting_row(
            p,
            "Run IronClaw on this Mac",
            Some(
                "ZLO starts IronClaw here and stops it when you quit. Off: this Mac only connects to other computers.",
            ),
            switch(
                k,
                runs,
                false,
                "Run IronClaw on this Mac",
                move |s: &mut State| pairing::set_run_local(s, !runs),
            ),
        ),
    ];
    if active && runs && (s.rt.backend.is_some() || s.fixture) {
        rows.push(remote_access(s, k));
    }
    rows_card(p, rows)
}

fn remote_access(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let access = &s.remote.page.remote_access;
    let on = pairing::remote_access_on(s);
    let mut desc = match s.remote.descriptor.as_ref().and_then(|d| d.tailnet_url.clone()) {
        Some(url) => format!("Reachable at {url} on your tailnet."),
        None => "Your other devices reach this Mac over Tailscale, with HTTPS. Needs Tailscale running here.".into(),
    };
    if access.busy {
        desc = "Working…".into();
    }
    let mut row_el = col().w_full().child(setting_row(
        p,
        "Let my other devices use this Mac",
        Some(&desc),
        switch(
            k,
            on,
            access.busy,
            "Let my other devices use this Mac",
            move |s: &mut State| pairing::set_remote_access(s, !on),
        ),
    ));
    if let Some(e) = &access.error {
        row_el = row_el.child(styled(e.clone(), xs(), p.danger).wrap().px(16.).pb(12.));
    }
    if let Some(n) = &access.note {
        row_el = row_el.child(styled(n.clone(), xs(), p.text_muted).wrap().px(16.).pb(12.));
    }
    row_el
}

fn saved_row(s: &State, k: &Kit, env: &SavedEnvironment) -> El {
    let p = &s.palette;
    let id = env.environment_id.clone();
    let st = remote::state_of(s, &id);
    let connected = s.onboarding.computer.key() == id && matches!(st, EnvState::Connected);
    let mut caption = env
        .routes
        .first()
        .map(|r| r.caption())
        .unwrap_or_else(|| "No address".into());
    if env.legacy_token {
        caption.push_str(" · Re-pair for a device token");
    }
    let mut actions = Vec::new();
    if s.remote.page.confirm_remove.as_deref() == Some(id.as_str()) {
        let yes = id.clone();
        actions.push(press_button(
            k,
            &format!("remove-yes-{id}"),
            "Remove",
            Btn::Danger,
            None,
            false,
            move |s| pairing::remove(s, yes.clone()),
        ));
        actions.push(press_button(
            k,
            &format!("remove-no-{id}"),
            "Keep",
            Btn::Ghost,
            None,
            false,
            |s| s.remote.page.confirm_remove = None,
        ));
    } else {
        if !connected && !matches!(st, EnvState::Connecting) {
            let target = id.clone();
            actions.push(press_button(
                k,
                &format!("connect-{id}"),
                "Connect",
                Btn::Secondary,
                None,
                false,
                move |s| remote::switch_to(s, Active::Environment(target.clone())),
            ));
        }
        let target = id.clone();
        actions.push(press_button(
            k,
            &format!("remove-{id}"),
            "Remove",
            Btn::Ghost,
            None,
            false,
            move |s| s.remote.page.confirm_remove = Some(target.clone()),
        ));
    }
    let mut out = col().w_full().child(computer_row(
        s,
        format!("computer-{id}"),
        env.label.clone(),
        Some(caption),
        &st,
        actions,
    ));
    out = out.children(update_line(s, &id));
    if s.remote.page.confirm_remove.as_deref() == Some(id.as_str()) {
        out = out.child(
            muted(p, "Removes it and its token from this Mac only. Its conversations stay on that computer.")
                .px(16.)
                .pb(12.),
        );
    }
    out
}

/// Add a computer: what Tailscale found, then a link or code. Shared with
/// setup's Connect step.
pub fn add_computer(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let page = &s.remote.page;
    let busy = page.busy.is_some();
    let mut out = col().key("add-computer").w_full().gap(12.);
    let found = pairing::found_unsaved(s);
    if page.tailscale != Some(false) {
        let mut list = col().key("tailnet-found").w_full().gap(6.);
        let title = if page.discovering && found.is_empty() {
            "Looking on your tailnet…"
        } else if found.is_empty() {
            "No other IronClaw found on your tailnet."
        } else {
            "Found on your tailnet"
        };
        list = list.child(styled(title, xs(), p.text_muted));
        for f in found {
            let origin = f.peer.origin();
            let label = f.descriptor.label.clone();
            let one_click = f
                .descriptor
                .pairing
                .contains(&zlo_client::environment_access::PairingMethod::TailnetIdentity);
            let pairing_this = page.busy.as_deref() == Some(label.as_str());
            let mut actions = Vec::new();
            if one_click {
                let (o, l) = (origin.clone(), label.clone());
                actions.push(press_button(
                    k,
                    &format!("one-click-{}", f.peer.dns_name),
                    "Connect",
                    Btn::Primary,
                    None,
                    pairing_this,
                    move |s| pairing::one_click(s, o.clone(), l.clone()),
                ));
            }
            let (o, l) = (origin.clone(), label.clone());
            actions.push(press_button(
                k,
                &format!("use-code-{}", f.peer.dns_name),
                "Use a code",
                Btn::Ghost,
                None,
                false,
                move |s| pairing::use_code_for(s, o.clone(), l.clone()),
            ));
            list = list.child(
                row()
                    .key(format!("found-{}", f.peer.dns_name))
                    .w_full()
                    .gap(10.)
                    .px(12.)
                    .py(8.)
                    .radius(8.)
                    .center_y()
                    .border(1., p.border)
                    .child(rune::icon(&crate::icons::MONITOR, 16.).color(p.text_muted))
                    .child(
                        col()
                            .grow(1.)
                            .min_w(0.)
                            .child(
                                text(label)
                                    .font(sm_medium())
                                    .color(p.text)
                                    .truncate()
                                    .min_w(0.),
                            )
                            .child(
                                styled(f.peer.dns_name.clone(), xs(), p.text_muted)
                                    .truncate()
                                    .min_w(0.),
                            ),
                    )
                    .children(actions),
            );
        }
        out = out.child(list);
    }
    let target = page
        .pair_target
        .as_ref()
        .map(|(_, label)| format!("Pairing code for {label}"))
        .unwrap_or_else(|| "Pairing link or code".into());
    out = out.child(styled(target.clone(), xs(), p.text_muted)).child(
        row()
            .w_full()
            .gap(8.)
            .center_y()
            .child(
                col().grow(1.).min_w(0.).child(
                    input(&page.input)
                        .placeholder(
                            "https://computer.tailnet.ts.net/pair#code=…  or  7KQ2-M9XD-4TRA",
                        )
                        .label(target)
                        .on_edit(|s: &mut State| &mut s.remote.page.input)
                        .on_submit(|s: &mut State, _: &mut rune::Tx| pairing::submit(s))
                        .build(k)
                        .key("pairing-input"),
                ),
            )
            .child(
                super::common::loading(
                    super::common::button(if busy { "Pairing…" } else { "Pair" }, Btn::Primary)
                        .disabled(page.input.text.trim().is_empty())
                        .on_press(pairing::submit),
                    busy,
                )
                .build(k)
                .key("pairing-submit"),
            ),
    );
    out = out.child(ssh_row(s, k));
    if let Some(e) = &page.error {
        out = out.child(styled(e.clone(), xs(), p.danger).wrap());
    }
    out.child(muted(
        p,
        "On the other computer: ZLO → Settings → Computers → Pair a device, or run `zlo pair`. Turn on remote access there first (`zlo remote enable`). Over SSH, IronClaw stays private on that computer and your SSH key does the pairing.",
    ))
}

/// "Over SSH": `user@host[:port]` and Connect, for computers without
/// Tailscale.
fn ssh_row(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let page = &s.remote.page;
    let busy = page.busy.is_some();
    let pairing_this = page.busy.as_deref().is_some_and(|b| {
        zlo_client::ssh::parse_target(&page.ssh_input.text).is_ok_and(|t| t.destination() == b)
    });
    col()
        .key("ssh-pairing")
        .w_full()
        .gap(6.)
        .child(styled("Over SSH, without Tailscale", xs(), p.text_muted))
        .child(
            row()
                .w_full()
                .gap(8.)
                .center_y()
                .child(
                    col().grow(1.).min_w(0.).child(
                        input(&page.ssh_input)
                            .placeholder("user@host  or  user@host:2222")
                            .label("Over SSH, without Tailscale")
                            .on_edit(|s: &mut State| &mut s.remote.page.ssh_input)
                            .on_submit(|s: &mut State, _: &mut rune::Tx| pairing::submit_ssh(s))
                            .build(k)
                            .key("ssh-input"),
                    ),
                )
                .child(
                    super::common::loading(
                        super::common::button(
                            if pairing_this {
                                "Connecting…"
                            } else {
                                "Connect"
                            },
                            Btn::Secondary,
                        )
                        .disabled(busy || page.ssh_input.text.trim().is_empty())
                        .on_press(pairing::submit_ssh),
                        pairing_this,
                    )
                    .build(k)
                    .key("ssh-submit"),
                ),
        )
}

/// A Light bundle with no computer yet: the whole window is "Add a
/// computer" (setup's first step, before there is anything to set up).
pub fn first_computer(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    col()
        .key("first-computer")
        .w_full()
        .h_full()
        .center()
        .padding(32.)
        .child(
            col()
                .w(560.)
                .gap(16.)
                .child(
                    text("Add a computer")
                        .font(rune::TextStyle::new(20., 600.))
                        .color(p.text),
                )
                .child(muted(
                    p,
                    "This ZLO has no IronClaw of its own. Pair the computer that runs it: found on your tailnet, with a pairing link or code, or over SSH.",
                ))
                .child(rows_card(
                    p,
                    [col().w_full().padding(16.).child(add_computer(s, k))],
                )),
        )
}

fn pair_device(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let page = &s.remote.page;
    let mut card = col().w_full().padding(16.).gap(10.);
    match &page.issued {
        Some(issued) => card = card.child(issued_code(s, k, issued)),
        None => {
            card = card.child(
                row()
                    .w_full()
                    .gap(12.)
                    .center_y()
                    .child(muted(p, "Codes work once and expire after five minutes.").grow(1.))
                    .child(press_button(
                        k,
                        "issue-code",
                        "Pair a device",
                        Btn::Secondary,
                        Some(&crate::icons::PLUS),
                        page.issuing,
                        pairing::issue_code,
                    )),
            )
        }
    }
    if let Some(e) = &page.issue_error {
        card = card.child(styled(e.clone(), xs(), p.danger).wrap());
    }
    rows_card(p, [card])
}

fn issued_code(s: &State, k: &Kit, issued: &Issued) -> El {
    let p = &s.palette;
    let expired = issued.expires_at <= chrono::Utc::now();
    let link = issued.link.clone();
    let code = issued.code.clone();
    col()
        .key("issued-code")
        .w_full()
        .gap(8.)
        .child(
            row()
                .gap(12.)
                .center_y()
                .child(
                    text(issued.code.clone())
                        .font(rune::TextStyle::new(22., 600.))
                        .color(if expired { p.text_muted } else { p.text })
                        .selectable(true),
                )
                .child(styled(
                    if expired {
                        "Expired".to_string()
                    } else {
                        format!("Expires {}", crate::jobs::when(issued.expires_at))
                    },
                    xs(),
                    p.text_muted,
                )),
        )
        .children((!link.is_empty()).then(|| {
            styled(link.clone(), MONO_XS, p.text_muted)
                .selectable(true)
                .truncate()
                .min_w(0.)
        }))
        .child(
            row()
                .gap(6.)
                .child(
                    super::common::button("Copy link", Btn::Secondary)
                        .icon(&crate::icons::COPY)
                        .on_press_tx(move |_: &mut State, tx: &mut rune::Tx| {
                            tx.copy(if link.is_empty() {
                                code.clone()
                            } else {
                                link.clone()
                            })
                        })
                        .build(k)
                        .key("copy-link"),
                )
                .child(press_button(
                    k,
                    "new-code",
                    "New code",
                    Btn::Ghost,
                    None,
                    s.remote.page.issuing,
                    pairing::issue_code,
                )),
        )
}

fn devices(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let page = &s.remote.page;
    let rows: Vec<El> = match &page.devices {
        None => vec![
            muted(
                p,
                if page.devices_loading {
                    "Loading…"
                } else {
                    "Not loaded."
                },
            )
            .padding(16.),
        ],
        Some(Err(e)) => vec![styled(e.clone(), xs(), p.danger).wrap().padding(16.)],
        Some(Ok(list)) if list.is_empty() => {
            vec![muted(p, "No devices paired yet.").padding(16.)]
        }
        Some(Ok(list)) => list
            .iter()
            .map(|d| {
                let mut caption = format!(
                    "{} · {}",
                    d.os,
                    match d.paired_by {
                        zlo_client::environment_access::DevicePairedBy::OneTimeCode =>
                            "paired with a code",
                        zlo_client::environment_access::DevicePairedBy::TailnetIdentity =>
                            "paired by tailnet login",
                    }
                );
                if let Some(seen) = d.last_seen_at {
                    caption.push_str(&format!(" · seen {}", crate::jobs::ago(seen)));
                }
                let revoked = d.revoked_at.is_some();
                let id = d.device_id.clone();
                let action = if revoked {
                    tag(p, "Revoked", Tone::Secondary)
                } else {
                    press_button(
                        k,
                        &format!("revoke-{id}"),
                        "Revoke",
                        Btn::Ghost,
                        None,
                        page.revoking.as_deref() == Some(id.as_str()),
                        move |s| pairing::revoke(s, id.clone()),
                    )
                };
                row()
                    .key(format!("device-{}", d.device_id))
                    .w_full()
                    .min_h(52.)
                    .px(16.)
                    .py(10.)
                    .gap(12.)
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
                                        text(d.label.clone())
                                            .font(sm_medium())
                                            .color(p.text)
                                            .truncate()
                                            .min_w(0.),
                                    )
                                    .children(
                                        d.current.then(|| tag(p, "This device", Tone::Success)),
                                    ),
                            )
                            .child(styled(caption, xs(), p.text_muted).truncate().min_w(0.)),
                    )
                    .child(action)
            })
            .collect(),
    };
    rows_card(p, rows)
}

/// "Check for updates" and "Update all machines".
fn updates_card(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let u = &s.remote.updates;
    let busy = u.checking || u.updating;
    let note = if u.updating {
        "Updating… This Mac goes last; ZLO reopens when it is done."
    } else {
        "Fast-forwards each computer's checkout, rebuilds and restarts IronClaw there."
    };
    let mut buttons = vec![press_button(
        k,
        "check-updates",
        "Check for updates",
        Btn::Secondary,
        None,
        u.checking,
        updates::check,
    )];
    if !busy {
        buttons.push(press_button(
            k,
            "update-all",
            "Update all machines",
            Btn::Primary,
            None,
            false,
            updates::update_everything,
        ));
    }
    let mut card = col().w_full().padding(16.).gap(10.).child(
        row()
            .w_full()
            .gap(12.)
            .center_y()
            .child(muted(p, note).grow(1.))
            .child(row().gap(6.).center_y().children(buttons)),
    );
    if u.rows.values().any(|r| *r == RowState::NotSetUp) {
        card = card.child(styled(updates::ENABLE_HINT, MONO_XS, p.text_muted).wrap());
    }
    rows_card(p, [card])
}

/// One computer's update state, under its row.
fn update_line(s: &State, key: &str) -> Option<El> {
    let p = &s.palette;
    let state = s.remote.updates.rows.get(key)?;
    let ink = match state {
        RowState::UpToDate | RowState::Updated(_) => p.success,
        RowState::Behind(_) => p.warning,
        RowState::Failed(_) => p.danger,
        RowState::Checking | RowState::NotSetUp | RowState::Updating(_) => p.text_muted,
    };
    Some(
        styled(state.label(), xs(), ink)
            .wrap()
            .px(16.)
            .pb(12.)
            .key(format!("update-{key}")),
    )
}
