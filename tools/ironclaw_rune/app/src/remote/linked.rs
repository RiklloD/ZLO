//! Linked environments (one computer's agents working on another) and the
//! requests agents raise for the person: a secret typed into a private
//! card, a machine to link. Requests are polled from the computer in use,
//! so they can be answered from any Mac connected to it. The Computers page
//! and the request cards draw this state; nothing here draws.

use std::collections::HashMap;
use std::time::Duration;

use rune::TextEdit;
use zlo_client::environment_access::{PairingCodeRequest, PeerLinkAccess};
use zlo_client::peer::{
    AgentRequestAnswer, AgentRequestKind, AgentRequestView, BalancePreference,
    LinkEnvironmentRequest, PeerLinkSummary,
};

use crate::jobs;
use crate::state::{Conn, State};

/// How often pending requests are read while connected.
const POLL: Duration = Duration::from_secs(4);

#[derive(Default)]
pub struct LinkedState {
    pub links: Option<Result<Vec<PeerLinkSummary>, String>>,
    pub loading: bool,
    /// "Link a computer": its address and the code made there.
    pub url_input: TextEdit,
    pub code_input: TextEdit,
    pub access: PeerLinkAccessPick,
    pub linking: bool,
    pub error: Option<String>,
    pub notice: Option<String>,
    /// A peer-link code made here for another computer to redeem.
    pub link_code: Option<LinkCode>,
    pub issuing: bool,
    /// The link whose Forget waits for a confirm.
    pub confirm_forget: Option<String>,
    /// Pending agent requests on the computer in use.
    pub requests: Vec<AgentRequestView>,
    /// Typed secrets, per request (never logged, cleared once answered).
    pub secret_inputs: HashMap<String, TextEdit>,
    /// Link requests: address and code typed for a new link.
    pub request_urls: HashMap<String, TextEdit>,
    pub request_codes: HashMap<String, TextEdit>,
    pub answering: Option<String>,
    pub request_errors: HashMap<String, String>,
    polling: bool,
}

/// The access picked for a new link or code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerLinkAccessPick(pub PeerLinkAccess);

impl Default for PeerLinkAccessPick {
    fn default() -> Self {
        Self(PeerLinkAccess::Operate)
    }
}

#[derive(Clone, Debug)]
pub struct LinkCode {
    pub code: String,
    pub access: PeerLinkAccess,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

pub fn access_label(access: PeerLinkAccess) -> &'static str {
    match access {
        PeerLinkAccess::ReadOnly => "Read only",
        PeerLinkAccess::Operate => "Supervised",
        PeerLinkAccess::Full => "Full access",
    }
}

pub fn preference_label(p: BalancePreference) -> &'static str {
    match p {
        BalancePreference::Prefer => "Prefer",
        BalancePreference::Normal => "Normal",
        BalancePreference::LessOften => "Less often",
        BalancePreference::ManualOnly => "Manual only",
    }
}

fn next_access(a: PeerLinkAccess) -> PeerLinkAccess {
    match a {
        PeerLinkAccess::ReadOnly => PeerLinkAccess::Operate,
        PeerLinkAccess::Operate => PeerLinkAccess::Full,
        PeerLinkAccess::Full => PeerLinkAccess::ReadOnly,
    }
}

fn next_preference(p: BalancePreference) -> BalancePreference {
    match p {
        BalancePreference::Prefer => BalancePreference::Normal,
        BalancePreference::Normal => BalancePreference::LessOften,
        BalancePreference::LessOften => BalancePreference::ManualOnly,
        BalancePreference::ManualOnly => BalancePreference::Prefer,
    }
}

/// The Computers page opened (or a new computer answered).
pub fn opened(s: &mut State) {
    load_links(s);
}

pub fn cycle_access(s: &mut State) {
    let l = &mut s.remote.linked;
    l.access = PeerLinkAccessPick(next_access(l.access.0));
}

pub fn load_links(s: &mut State) {
    if s.rt.backend.is_none() || s.remote.linked.loading {
        return;
    }
    s.remote.linked.loading = true;
    jobs::run(
        s,
        "peer links",
        |c| async move { c.peer_links().await },
        |s, result, _| {
            let l = &mut s.remote.linked;
            l.loading = false;
            l.links = Some(result.map(|list| list.links).map_err(|e| {
                if e.is_not_found() {
                    "This computer's IronClaw is too old for linked environments. Update it."
                        .to_string()
                } else {
                    e.to_string()
                }
            }));
        },
    );
}

/// "Link": redeem the code from the other computer at its address.
pub fn link(s: &mut State) {
    let l = &mut s.remote.linked;
    if l.linking {
        return;
    }
    let url = l.url_input.text.trim().to_string();
    let code = l.code_input.text.trim().to_string();
    if url.is_empty() || code.is_empty() {
        l.error = Some("Enter the other computer's address and the link code made there.".into());
        return;
    }
    l.linking = true;
    l.error = None;
    l.notice = None;
    let request = LinkEnvironmentRequest {
        url,
        pairing_code: code,
        access: l.access.0,
    };
    jobs::run(
        s,
        "link computer",
        move |c| async move { c.link_environment(&request).await },
        |s, result, _| {
            let l = &mut s.remote.linked;
            l.linking = false;
            match result {
                Ok(link) => {
                    l.url_input = TextEdit::new("");
                    l.code_input = TextEdit::new("");
                    l.notice = Some(format!(
                        "Linked {} ({}). Agents here can work there now.",
                        link.label,
                        access_label(link.access)
                    ));
                    load_links(s);
                }
                Err(e) => l.error = Some(e.to_string()),
            }
        },
    );
}

/// "Make a link code": a one-time code another computer redeems to link
/// this one, with the access picked here.
pub fn issue_link_code(s: &mut State) {
    let l = &mut s.remote.linked;
    if l.issuing {
        return;
    }
    l.issuing = true;
    l.error = None;
    let access = l.access.0;
    let request = PairingCodeRequest {
        scopes: None,
        peer_link: Some(access),
    };
    jobs::run(
        s,
        "link code",
        move |c| async move { c.issue_scoped_code(&request).await },
        move |s, result, tx| {
            // The person's Make a link code: the code wipes in.
            tx.animated();
            let l = &mut s.remote.linked;
            l.issuing = false;
            match result {
                Ok(issued) => {
                    l.link_code = Some(LinkCode {
                        code: issued.code,
                        access: issued.peer_link.unwrap_or(access),
                        expires_at: issued.expires_at,
                    })
                }
                Err(e) => l.error = Some(e.to_string()),
            }
        },
    );
}

pub fn ask_forget(s: &mut State, environment_id: String) {
    s.remote.linked.confirm_forget = Some(environment_id);
}

pub fn forget(s: &mut State, environment_id: String) {
    s.remote.linked.confirm_forget = None;
    jobs::run(
        s,
        "forget link",
        move |c| async move { c.forget_environment(&environment_id).await },
        |s, result, _| {
            if let Err(e) = result {
                s.remote.linked.error = Some(e.to_string());
            }
            load_links(s);
        },
    );
}

pub fn cycle_preference(s: &mut State, environment_id: String, current: BalancePreference) {
    let next = next_preference(current);
    jobs::run(
        s,
        "link preference",
        move |c| async move { c.set_link_preference(&environment_id, next).await },
        |s, result, _| {
            if let Err(e) = result {
                s.remote.linked.error = Some(e.to_string());
            }
            load_links(s);
        },
    );
}

// ── agent requests ──────────────────────────────────────────────────────

/// Start reading pending requests (connected); one chain per connection.
pub(crate) fn start_polling(s: &mut State) {
    if s.fixture || s.remote.linked.polling {
        return;
    }
    s.remote.linked.polling = true;
    poll(s);
}

fn poll(s: &mut State) {
    if !matches!(s.conn, Conn::Ready) || s.rt.backend.is_none() {
        s.remote.linked.polling = false;
        return;
    }
    let tag = s.remote.tag.clone();
    jobs::run(
        s,
        "agent requests",
        |c| async move { c.agent_requests().await },
        move |s, result, _| {
            match result {
                Ok(list) => apply_requests(s, list.requests),
                // An older backend has no such route: stop asking.
                Err(e) if e.is_not_found() => {
                    s.remote.linked.polling = false;
                    return;
                }
                Err(e) => tracing::debug!(error = %e, "agent requests not read"),
            }
            jobs::after(s, POLL, move |s, _| {
                if s.remote.tag == tag {
                    poll(s);
                } else {
                    s.remote.linked.polling = false;
                }
            });
        },
    );
}

fn apply_requests(s: &mut State, requests: Vec<AgentRequestView>) {
    let l = &mut s.remote.linked;
    let open: Vec<String> = requests.iter().map(|r| r.request_id.clone()).collect();
    // Typed values of requests that closed elsewhere go with them.
    l.secret_inputs.retain(|id, _| open.contains(id));
    l.request_urls.retain(|id, _| open.contains(id));
    l.request_codes.retain(|id, _| open.contains(id));
    l.request_errors.retain(|id, _| open.contains(id));
    l.requests = requests;
}

/// The secret input of one request (masked).
pub fn secret_input<'a>(s: &'a mut State, id: &str) -> &'a mut TextEdit {
    s.remote
        .linked
        .secret_inputs
        .entry(id.to_string())
        .or_insert_with(|| TextEdit::new("").secure())
}

pub fn request_url<'a>(s: &'a mut State, id: &str) -> &'a mut TextEdit {
    s.remote
        .linked
        .request_urls
        .entry(id.to_string())
        .or_insert_with(|| TextEdit::new(""))
}

pub fn request_code<'a>(s: &'a mut State, id: &str) -> &'a mut TextEdit {
    s.remote
        .linked
        .request_codes
        .entry(id.to_string())
        .or_insert_with(|| TextEdit::new(""))
}

fn answer(s: &mut State, id: String, answer: AgentRequestAnswer) {
    if s.remote.linked.answering.is_some() {
        return;
    }
    s.remote.linked.answering = Some(id.clone());
    s.remote.linked.request_errors.remove(&id);
    let request_id = id.clone();
    jobs::run(
        s,
        "answer request",
        move |c| async move { c.answer_agent_request(&request_id, &answer).await },
        move |s, result, tx| {
            // The person's answer: the card folds away (or its error wipes in).
            tx.animated();
            let l = &mut s.remote.linked;
            l.answering = None;
            match result {
                Ok(_) => {
                    l.secret_inputs.remove(&id);
                    l.requests.retain(|r| r.request_id != id);
                }
                Err(e) => {
                    l.request_errors.insert(id, e.to_string());
                }
            }
        },
    );
}

/// "Save securely": the typed value (or an `op://` reference).
pub fn save_secret(s: &mut State, id: String) {
    let value = s
        .remote
        .linked
        .secret_inputs
        .get(&id)
        .map(|t| t.text.clone())
        .unwrap_or_default();
    if value.is_empty() {
        s.remote
            .linked
            .request_errors
            .insert(id, "Type or paste the value first.".into());
        return;
    }
    answer(s, id, AgentRequestAnswer::Secret { value });
}

pub fn decline(s: &mut State, id: String) {
    answer(s, id, AgentRequestAnswer::Decline);
}

/// Answer a link request with a computer that is already linked.
pub fn answer_with_link(s: &mut State, id: String, environment_id: String, access: PeerLinkAccess) {
    answer(
        s,
        id,
        AgentRequestAnswer::Link {
            environment_id: Some(environment_id),
            url: None,
            pairing_code: None,
            access,
        },
    );
}

/// Answer a link request with a new link from the typed address and code.
pub fn answer_with_new_link(s: &mut State, id: String) {
    let l = &s.remote.linked;
    let url = l
        .request_urls
        .get(&id)
        .map(|t| t.text.trim().to_string())
        .unwrap_or_default();
    let code = l
        .request_codes
        .get(&id)
        .map(|t| t.text.trim().to_string())
        .unwrap_or_default();
    if url.is_empty() || code.is_empty() {
        s.remote.linked.request_errors.insert(
            id,
            "Enter the other computer's address and a link code made there.".into(),
        );
        return;
    }
    let access = match l
        .requests
        .iter()
        .find(|r| r.request_id == id)
        .map(|r| &r.kind)
    {
        Some(AgentRequestKind::EnvironmentLink {
            access: Some(a), ..
        }) => *a,
        _ => l.access.0,
    };
    answer(
        s,
        id,
        AgentRequestAnswer::Link {
            environment_id: None,
            url: Some(url),
            pairing_code: Some(code),
            access,
        },
    );
}
