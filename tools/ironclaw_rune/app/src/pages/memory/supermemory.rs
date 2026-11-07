//! The Memory page's Supermemory tab (the second half of GPUI
//! `pages/memory.rs`): the chosen profile's container on the user's own
//! Supermemory account — what was captured there, semantic search, and
//! deleting a wrong document or forgetting a wrong memory.

use ironclaw_gpui_client::memory_page::{
    SupermemoryBindingInfo, SupermemoryBindingOrigin, SupermemoryDocumentPage,
    SupermemoryDocumentSummary, SupermemoryMemoryHit, SupermemoryStatus,
};
use rune::{El, TextEdit, col, row};
use rune_kit::{Kit, Spinner, TextInput};

use super::super::usage::common_stub::{
    BODY_SEMIBOLD, Look, SM, SM_MEDIUM, XS, button, empty_state, icon_button, icon_button_disabled,
    label, rule,
};
use super::{MemoryState, fail, field_row};
use crate::icons;
use crate::jobs;
use crate::state::State;

/// Profiles, status and the first page of documents for the chosen profile.
pub(super) fn load(s: &mut State) {
    let m = &s.pages.memory;
    let profile = m.profile.clone();
    let container = m.container.clone();
    let asked = profile.clone();
    jobs::run(
        s,
        "memory",
        move |c| async move {
            // silent-ok: without the orchestrator there are no profiles, only the default.
            let profiles = c
                .profiles()
                .await
                .map(|list| {
                    list.profiles
                        .into_iter()
                        .map(|p| (p.profile_id, p.settings.display_name))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let status = c.supermemory_status(profile.as_deref()).await?;
            let page = if status.active {
                Some(c.supermemory_documents(profile.as_deref(), container.as_deref(), 1).await?)
            } else {
                None
            };
            Ok((profiles, status, page))
        },
        move |s, result, _tx| {
            let m = &mut s.pages.memory;
            // Another profile was picked meanwhile: its own load answers.
            if m.profile != asked {
                return;
            }
            match result {
                Ok((profiles, status, page)) => {
                    m.profiles = profiles;
                    m.status = Some(status);
                    m.page = page;
                    m.error = None;
                }
                Err(e) => m.error = Some(e.to_string()),
            }
        },
    );
}

fn load_page(s: &mut State, page: u32) {
    let m = &s.pages.memory;
    let (profile, container) = (m.profile.clone(), m.container.clone());
    let asked = (profile.clone(), container.clone());
    jobs::run(
        s,
        "memory",
        move |c| async move {
            c.supermemory_documents(profile.as_deref(), container.as_deref(), page).await
        },
        move |s, result, _tx| {
            let m = &mut s.pages.memory;
            if (m.profile.clone(), m.container.clone()) != asked {
                return;
            }
            match result {
                Ok(page) => m.page = Some(page),
                Err(e) => m.error = Some(e.to_string()),
            }
        },
    );
}

fn run_search(s: &mut State) {
    let m = &mut s.pages.memory;
    let query = m.sm_search.text.trim().to_string();
    if query.is_empty() {
        m.sm_hits = None;
        return;
    }
    let (profile, container) = (m.profile.clone(), m.container.clone());
    jobs::run(
        s,
        "memory",
        move |c| async move {
            c.supermemory_search(profile.as_deref(), &query, container.as_deref()).await
        },
        |s, result, _tx| {
            let m = &mut s.pages.memory;
            match result {
                Ok(found) => m.sm_hits = Some(found.hits),
                Err(e) => m.error = Some(e.to_string()),
            }
        },
    );
}

fn delete_remote_document(s: &mut State, id: String) {
    let m = &mut s.pages.memory;
    m.confirm_remote = None;
    m.busy = true;
    let profile = m.profile.clone();
    jobs::run(
        s,
        "memory",
        move |c| async move { c.supermemory_delete_document(profile.as_deref(), &id).await },
        |s, result, _tx| match result {
            Ok(evidence) => {
                let m = &mut s.pages.memory;
                m.busy = false;
                m.notice = Some(if evidence.verified {
                    "Deleted from Supermemory.".into()
                } else {
                    "Supermemory accepted the delete; it may take a moment to disappear.".into()
                });
                let page = m.page.as_ref().map_or(1, |p| p.page);
                load_page(s, page);
            }
            Err(e) => fail(s, e),
        },
    );
}

fn forget_remote_memory(s: &mut State, container: String, id: String) {
    let m = &mut s.pages.memory;
    m.confirm_remote = None;
    m.busy = true;
    let profile = m.profile.clone();
    jobs::run(
        s,
        "memory",
        move |c| async move { c.supermemory_forget(profile.as_deref(), &container, &id).await },
        |s, result, _tx| match result {
            Ok(evidence) if evidence.forgotten => {
                let m = &mut s.pages.memory;
                m.busy = false;
                m.notice = Some("Supermemory will no longer recall that.".into());
                if let Some(hits) = m.sm_hits.as_mut() {
                    hits.retain(|hit| hit.id != evidence.id);
                }
            }
            Ok(_) => {
                let m = &mut s.pages.memory;
                m.busy = false;
                m.error = Some("Supermemory did not confirm the forget.".into());
            }
            Err(e) => fail(s, e),
        },
    );
}

fn pick_profile(s: &mut State, id: Option<String>) {
    let m = &mut s.pages.memory;
    m.profile = id;
    m.container = None;
    m.sm_hits = None;
    load(s);
}

fn pick_container(s: &mut State, tag: String) {
    s.pages.memory.container = Some(tag);
    load_page(s, 1);
}

fn binding_line(status: &SupermemoryStatus) -> String {
    match (&status.binding, status.key_configured) {
        (_, false) => "No Supermemory API key is stored, so local memory is used. Add the \
                       key in Settings to turn Supermemory on."
            .to_string(),
        (None, true) => "This profile has no Supermemory container; local memory is used.".to_string(),
        (Some(binding), true) => format!(
            "Container {} ({}){}. Recall {}, capture {}.",
            binding.primary_container,
            match binding.origin {
                SupermemoryBindingOrigin::Profile => "from this profile",
                SupermemoryBindingOrigin::Default => "deployment default",
            },
            if binding.read_containers.is_empty() {
                String::new()
            } else {
                format!("; also reads {}", binding.read_containers.join(", "))
            },
            if status.auto_recall { "on" } else { "off" },
            if status.auto_capture { "on" } else { "off" },
        ),
    }
}

pub(super) fn render(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let m = &s.pages.memory;
    let Some(status) = m.status.as_ref() else {
        return row().w_full().py(24.0).justify_center().child(Spinner::new().build(k));
    };
    if !status.available {
        return empty_state(p, "Supermemory is not part of this build.");
    }
    let mut profile_picker = row().key("sm-profiles").w_full().gap(8.0).flex_wrap();
    let choices = std::iter::once((None, "Default".to_string()))
        .chain(m.profiles.iter().map(|(id, name)| (Some(id.clone()), name.clone())));
    for (ix, (id, name)) in choices.enumerate() {
        let look = if m.profile == id { Look::Primary } else { Look::Ghost };
        profile_picker = profile_picker.child(button(
            k,
            &format!("sm-profile-{ix}"),
            &name,
            look,
            None,
            false,
            move |s: &mut State| pick_profile(s, id.clone()),
        ));
    }
    let primary = status.binding.as_ref().map(|b| b.primary_container.clone());
    let containers: Vec<String> = status
        .binding
        .as_ref()
        .map(|b| std::iter::once(b.primary_container.clone()).chain(b.read_containers.iter().cloned()).collect())
        .unwrap_or_default();
    let current = m.container.clone().or(primary.clone());
    let mut container_picker = row().key("sm-containers").gap(8.0);
    for (ix, tag) in containers.into_iter().enumerate() {
        let look = if current.as_deref() == Some(tag.as_str()) { Look::Outline } else { Look::Ghost };
        let t = tag.clone();
        container_picker = container_picker.child(button(
            k,
            &format!("sm-container-{ix}"),
            &tag,
            look,
            None,
            false,
            move |s: &mut State| pick_container(s, t.clone()),
        ));
    }
    let writable = current.is_some() && current == primary;
    let mut c = col()
        .w_full()
        .gap(12.0)
        .child(profile_picker)
        .child(label(binding_line(status), SM, p.text_muted).wrap());
    if !status.active {
        return c;
    }
    c = c.child(container_picker).child(field_row(
        TextInput::new(&m.sm_search)
            .width(None)
            .placeholder("Search Supermemory")
            .label("Search Supermemory")
            .on_edit(|s: &mut State| &mut s.pages.memory.sm_search)
            .on_submit(|s: &mut State, _| run_search(s))
            .build(k)
            .key("sm-search-field"),
        icon_button(k, "sm-search", &icons::SEARCH, "Search Supermemory", run_search),
    ));
    if let Some(hits) = &m.sm_hits {
        c = c.child(render_hits(s, k, hits, primary.as_deref()));
    }
    if let Some(page) = &m.page {
        c = c.child(render_documents(s, k, page, writable));
    }
    c
}

fn render_hits(s: &State, k: &Kit, hits: &[SupermemoryMemoryHit], primary: Option<&str>) -> El {
    let p = &s.palette;
    let m = &s.pages.memory;
    if hits.is_empty() {
        return empty_state(p, "Nothing matches.").key("sm-hits");
    }
    let mut list = col().key("sm-hits").w_full();
    for (ix, hit) in hits.iter().enumerate() {
        let key = format!("memory:{}", hit.id);
        let confirming = m.confirm_remote.as_deref() == Some(key.as_str());
        let (container, id) = (hit.container.clone(), hit.id.clone());
        let can_forget = primary == Some(hit.container.as_str());
        let meta = format!(
            "{}{}{}",
            hit.container,
            hit.similarity.map(|s| format!(" · {:.0}%", s * 100.)).unwrap_or_default(),
            hit.updated_at.as_deref().map(|t| format!(" · {t}")).unwrap_or_default()
        );
        let mut r = row()
            .key(format!("sm-hit-{}", hit.id))
            .w_full()
            .gap(12.0)
            .py(8.0)
            .child(
                col()
                    .grow(1.0)
                    .min_w(0.0)
                    .child(label(hit.content.clone(), SM, p.text).wrap())
                    .child(label(meta, XS, p.text_muted)),
            );
        if can_forget {
            r = r.child(if confirming {
                button(k, &format!("sm-forget-confirm-{ix}"), "Forget", Look::Danger, None, m.busy, move |s: &mut State| {
                    forget_remote_memory(s, container.clone(), id.clone())
                })
            } else {
                icon_button(k, &format!("sm-forget-{ix}"), &icons::DELETE, "Forget this memory", move |s: &mut State| {
                    s.pages.memory.confirm_remote = Some(key.clone())
                })
            });
        }
        list = list.child(r).child(rule(p).key(format!("sm-hit-rule-{ix}")));
    }
    list
}

fn render_documents(s: &State, k: &Kit, page: &SupermemoryDocumentPage, writable: bool) -> El {
    let p = &s.palette;
    let m = &s.pages.memory;
    let mut list = col()
        .key("sm-documents")
        .w_full()
        .child(col().pt(16.0).pb(4.0).child(label(
            format!("Captured documents in {}", page.container),
            BODY_SEMIBOLD,
            p.text,
        )));
    if page.documents.is_empty() {
        list = list.child(empty_state(p, "Nothing captured here yet."));
    }
    for (ix, document) in page.documents.iter().enumerate() {
        let key = format!("document:{}", document.id);
        let confirming = m.confirm_remote.as_deref() == Some(key.as_str());
        let id = document.id.clone();
        let title = document
            .title
            .clone()
            .or(document.custom_id.clone())
            .unwrap_or_else(|| document.id.clone());
        let mut text_col = col().grow(1.0).min_w(0.0).child(label(title, SM_MEDIUM, p.text).truncate());
        if let Some(preview) = &document.preview {
            text_col = text_col.child(label(preview.clone(), XS, p.text_muted).wrap());
        }
        text_col = text_col.child(label(
            format!(
                "{}{}",
                document.status.clone().unwrap_or_default(),
                document.updated_at.as_deref().map(|t| format!(" · {t}")).unwrap_or_default()
            ),
            XS,
            p.text_muted,
        ));
        let mut r = row().key(format!("sm-doc-{}", document.id)).w_full().gap(12.0).py(8.0).child(text_col);
        if writable {
            r = r.child(if confirming {
                button(
                    k,
                    &format!("sm-delete-confirm-{ix}"),
                    "Delete from Supermemory",
                    Look::Danger,
                    None,
                    m.busy,
                    move |s: &mut State| delete_remote_document(s, id.clone()),
                )
            } else {
                icon_button(k, &format!("sm-delete-{ix}"), &icons::DELETE, "Delete document", move |s: &mut State| {
                    s.pages.memory.confirm_remote = Some(key.clone())
                })
            });
        }
        list = list.child(r).child(rule(p).key(format!("sm-doc-rule-{ix}")));
    }
    let total = page.total_pages.unwrap_or(page.page);
    let current_page = page.page;
    let prev = if current_page <= 1 {
        icon_button_disabled(k, "sm-prev", &icons::CHEVRON_LEFT, "Previous page")
    } else {
        icon_button(k, "sm-prev", &icons::CHEVRON_LEFT, "Previous page", move |s: &mut State| {
            load_page(s, current_page.saturating_sub(1).max(1))
        })
    };
    let next = if current_page >= total {
        icon_button_disabled(k, "sm-next", &icons::CHEVRON_RIGHT, "Next page")
    } else {
        icon_button(k, "sm-next", &icons::CHEVRON_RIGHT, "Next page", move |s: &mut State| {
            load_page(s, current_page + 1)
        })
    };
    list.child(
        row()
            .key("sm-pager")
            .w_full()
            .gap(8.0)
            .pt(8.0)
            .justify_end()
            .center_y()
            .child(prev)
            .child(label(format!("{current_page} / {total}"), XS, p.text_muted))
            .child(next),
    )
}

/// A configured profile container with two hits and a page of documents.
pub(super) fn fixture(m: &mut MemoryState) {
    m.profiles = vec![("work".into(), "Work".into()), ("home".into(), "Home".into())];
    m.profile = Some("work".into());
    m.status = Some(SupermemoryStatus {
        available: true,
        key_configured: true,
        profile_id: Some("work".into()),
        binding: Some(SupermemoryBindingInfo {
            primary_container: "zlo-work".into(),
            read_containers: vec!["zlo-shared".into()],
            origin: SupermemoryBindingOrigin::Profile,
        }),
        active: true,
        auto_recall: true,
        auto_capture: false,
    });
    m.sm_search = TextEdit::new("deploy");
    m.sm_hits = Some(vec![
        SupermemoryMemoryHit {
            id: "mem_1".into(),
            container: "zlo-work".into(),
            content: "Deploys go through the staging gateway first; production needs a green canary.".into(),
            similarity: Some(0.87),
            updated_at: Some("2026-11-05".into()),
        },
        SupermemoryMemoryHit {
            id: "mem_2".into(),
            container: "zlo-shared".into(),
            content: "The user prefers release notes as a short bullet list.".into(),
            similarity: Some(0.64),
            updated_at: None,
        },
    ]);
    m.confirm_remote = Some("memory:mem_1".into());
    let doc = |id: &str, title: Option<&str>, preview: Option<&str>| SupermemoryDocumentSummary {
        id: id.into(),
        custom_id: None,
        title: title.map(str::to_string),
        preview: preview.map(str::to_string),
        status: Some("done".into()),
        container_tags: vec!["zlo-work".into()],
        created_at: None,
        updated_at: Some("2026-11-06".into()),
    };
    m.page = Some(SupermemoryDocumentPage {
        container: "zlo-work".into(),
        documents: vec![
            doc("doc_a", Some("Thread: staging deploy"), Some("Captured from a thread about the canary rollout.")),
            doc("doc_b", None, None),
        ],
        page: 1,
        total_pages: Some(3),
        total_items: Some(41),
    });
}
