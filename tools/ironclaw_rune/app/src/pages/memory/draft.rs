//! The Memory page's open-document state and how a gateway reply is applied
//! to it (GPUI `pages/memory_draft.rs`, ported unchanged). Kept UI-free
//! (generic over the editor, a `rune::TextEdit` here) so the late-reply
//! rules are unit-tested: a reply only ever touches the draft that sent it —
//! the same path AND the same edit generation — never a document opened or
//! edited since.

use zlo_client::memory_page::MemoryDocument;

/// The open document and its editor, when editing.
pub(super) struct Open<E> {
    pub(super) document: MemoryDocument,
    pub(super) editor: Option<E>,
    /// The save hit a newer version; `document` is that version.
    pub(super) conflict: bool,
    /// Bumped every time a document is opened, created, or put into edit.
    pub(super) generation: u64,
}

/// Which draft a request was sent from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Ticket {
    pub(super) path: String,
    pub(super) generation: u64,
}

impl<E> Open<E> {
    pub(super) fn ticket(&self) -> Ticket {
        Ticket {
            path: self.document.path.clone(),
            generation: self.generation,
        }
    }

    fn owns(&self, ticket: &Ticket) -> bool {
        self.document.path == ticket.path && self.generation == ticket.generation
    }
}

/// A save reply, as the gateway answered it.
pub(super) enum SaveReply {
    Written(MemoryDocument),
    /// The stored document moved; it is the current version when known.
    Conflict(Option<MemoryDocument>),
    WrittenWithoutReadBack,
}

/// What the view should tell the user after applying a reply.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Effect {
    Notice(String),
    Error(String),
}

/// Apply a save reply. The reply's own draft is updated (saved: back to the
/// read-only view of the stored version; conflict: the user's text is kept and
/// the newer stored version recorded). A reply for a draft no longer open
/// changes nothing on screen beyond a message.
pub(super) fn apply_save<E>(
    open: &mut Option<Open<E>>,
    ticket: &Ticket,
    reply: SaveReply,
    next_generation: &mut u64,
) -> Effect {
    let owned = open.as_ref().is_some_and(|current| current.owns(ticket));
    match reply {
        SaveReply::Written(document) => {
            let message = format!("Saved {}.", document.path);
            if owned {
                *next_generation += 1;
                *open = Some(Open {
                    document,
                    editor: None,
                    conflict: false,
                    generation: *next_generation,
                });
            }
            Effect::Notice(message)
        }
        SaveReply::Conflict(current) => match open.as_mut().filter(|_| owned) {
            Some(draft) => {
                if let Some(current) = current {
                    draft.document = current;
                }
                draft.conflict = true;
                Effect::Error(format!(
                    "{} changed since you opened it; your text is kept.",
                    ticket.path
                ))
            }
            None => Effect::Error(format!(
                "{} changed before your earlier save landed; nothing was saved. Reopen it to \
                 try again.",
                ticket.path
            )),
        },
        SaveReply::WrittenWithoutReadBack => {
            Effect::Error(format!("Saved {}, but the read-back failed.", ticket.path))
        }
    }
}

/// Apply a successful archive: close the draft only if it is the one that was
/// archived.
pub(super) fn apply_archived<E>(open: &mut Option<Open<E>>, ticket: &Ticket) {
    if open.as_ref().is_some_and(|current| current.owns(ticket)) {
        *open = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zlo_client::memory_page::{MemoryDocumentKind, MemoryProvenance};

    fn doc(path: &str, content: &str) -> MemoryDocument {
        MemoryDocument {
            path: path.to_string(),
            kind: MemoryDocumentKind::Note,
            content: content.to_string(),
            content_hash: format!("hash-{content}"),
            word_count: 1,
            provenance: MemoryProvenance::default(),
        }
    }

    fn editing(path: &str, content: &str, generation: u64) -> Option<Open<&'static str>> {
        Some(Open {
            document: doc(path, content),
            editor: Some("editor"),
            conflict: false,
            generation,
        })
    }

    /// Save A, then open and edit B: A's late conflict must not swap B's
    /// document for A's (a later "Save anyway" would write B's text into A).
    #[test]
    fn late_conflict_for_another_document_leaves_the_current_draft_alone() {
        let ticket_a = Ticket {
            path: "a.md".into(),
            generation: 1,
        };
        let mut open = editing("b.md", "b text", 2);
        let mut generation = 2;
        let effect = apply_save(
            &mut open,
            &ticket_a,
            SaveReply::Conflict(Some(doc("a.md", "a newer"))),
            &mut generation,
        );
        let draft = open.unwrap();
        assert_eq!(draft.document.path, "b.md");
        assert_eq!(draft.document.content, "b text");
        assert!(!draft.conflict);
        assert!(draft.editor.is_some());
        assert!(matches!(effect, Effect::Error(text) if text.contains("a.md")));
    }

    /// A's late success must not close B's editor.
    #[test]
    fn late_success_for_another_document_keeps_the_current_editor() {
        let ticket_a = Ticket {
            path: "a.md".into(),
            generation: 1,
        };
        let mut open = editing("b.md", "b text", 2);
        let mut generation = 2;
        let effect = apply_save(
            &mut open,
            &ticket_a,
            SaveReply::Written(doc("a.md", "a saved")),
            &mut generation,
        );
        let draft = open.unwrap();
        assert_eq!(draft.document.path, "b.md");
        assert!(draft.editor.is_some());
        assert_eq!(effect, Effect::Notice("Saved a.md.".into()));
    }

    /// Same path, but re-opened since the save (a newer generation): still
    /// not the reply's draft.
    #[test]
    fn late_reply_for_an_older_generation_of_the_same_path_is_ignored() {
        let ticket = Ticket {
            path: "a.md".into(),
            generation: 1,
        };
        let mut open = editing("a.md", "re-edited", 3);
        let mut generation = 3;
        apply_save(
            &mut open,
            &ticket,
            SaveReply::Conflict(Some(doc("a.md", "stored"))),
            &mut generation,
        );
        let draft = open.unwrap();
        assert_eq!(draft.document.content, "re-edited");
        assert!(!draft.conflict);
    }

    #[test]
    fn own_replies_update_the_draft() {
        let ticket = Ticket {
            path: "a.md".into(),
            generation: 1,
        };
        let mut open = editing("a.md", "mine", 1);
        let mut generation = 1;
        apply_save(
            &mut open,
            &ticket,
            SaveReply::Conflict(Some(doc("a.md", "theirs"))),
            &mut generation,
        );
        let draft = open.as_ref().unwrap();
        assert!(draft.conflict);
        assert_eq!(draft.document.content, "theirs");
        assert!(draft.editor.is_some(), "the user's text is kept");

        let ticket = open.as_ref().unwrap().ticket();
        apply_save(
            &mut open,
            &ticket,
            SaveReply::Written(doc("a.md", "mine")),
            &mut generation,
        );
        let draft = open.unwrap();
        assert!(draft.editor.is_none());
        assert!(!draft.conflict);
        assert_eq!(draft.generation, 2);
    }

    #[test]
    fn archive_closes_only_its_own_draft() {
        let ticket_a = Ticket {
            path: "a.md".into(),
            generation: 1,
        };
        let mut open = editing("b.md", "b", 2);
        apply_archived(&mut open, &ticket_a);
        assert!(open.is_some());
        let mut open = editing("a.md", "a", 1);
        apply_archived(&mut open, &ticket_a);
        assert!(open.is_none());
    }
}
