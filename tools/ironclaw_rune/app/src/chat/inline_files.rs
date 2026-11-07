//! Files placed in the text (GPUI `chat/inline_files.rs`). A staged file
//! shows as a card at the spot it was attached: in the composer it is an
//! atomic pill (Rune's `Mark::Pill`: one Backspace removes it, undo brings
//! it back), and in the sent bubble a card between the words. On send each
//! pill becomes an anchor (`{{attachment:N}}`), so the backend puts the
//! file's reference where the person put it. Both composers (thread and
//! new-thread draft) share these helpers.
//!
//! `@` mentions are pills too: their id names the mention
//! (`mentions::token_id`), and on send each becomes its Markdown link.

use std::collections::HashSet;
use std::ops::Range;
use std::path::PathBuf;

use ironclaw_gpui_client::attachments::{self, StagedFile};
use ironclaw_gpui_client::inbound_requests::attachment_anchor;
use ironclaw_gpui_client::mentions;
use rune::{ClipboardContent, El, Icon, TextEdit, icon, row, text};

use crate::icons;
use crate::snapshot;
use crate::state::State;

/// Put a pill for each of `new` (already pushed onto `all`) at the
/// composer's caret, each followed by a space. A SnapShot's text half rides
/// with its picture and gets none.
pub fn insert(s: &State, composer: &mut TextEdit, new: &[StagedFile], all: &[StagedFile]) {
    for f in new.iter().filter(|f| !snapshot::is_companion(f, all)) {
        let (title, _label) =
            snapshot::names(s, f).unwrap_or_else(|| (f.filename.clone(), f.filename.clone()));
        composer.undo_group(|e| {
            e.insert_pill(f.key.to_string(), &printable(&title));
            e.insert(" ");
        });
    }
    composer.focused = true;
}

/// Drop the staged files whose pill is gone from the composer (a SnapShot's
/// text half goes with its picture).
pub fn prune(composer: &TextEdit, staged: &mut Vec<StagedFile>) {
    let live: HashSet<String> = composer.pills().map(|(_, id)| id.to_string()).collect();
    let kept: Vec<bool> = staged
        .iter()
        .map(|f| live.contains(&f.key.to_string()))
        .collect();
    let mut ix = 0;
    let all = staged.clone();
    staged.retain(|f| {
        let keep = kept[ix]
            || all
                .iter()
                .zip(&kept)
                .any(|(image, kept)| *kept && snapshot::pairs(f, image));
        ix += 1;
        keep
    });
}

/// The composer's pills as `(range, id)`, in text order.
pub fn pills(composer: &TextEdit) -> Vec<(Range<usize>, String)> {
    let mut out: Vec<(Range<usize>, String)> = composer
        .pills()
        .map(|(range, id)| (range, id.to_string()))
        .collect();
    out.sort_by_key(|(r, _)| r.start);
    out
}

/// What to send: the text with an anchor at each file pill, and the files
/// in anchor order (a SnapShot's text half right after its picture, with no
/// anchor of its own). A pill whose file is gone sends its name as text.
pub fn outgoing(composer: &TextEdit, staged: &[StagedFile]) -> (String, Vec<StagedFile>) {
    let text = &composer.text;
    let mut files: Vec<StagedFile> = Vec::new();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (range, id) in pills(composer) {
        if range.start < last {
            continue;
        }
        out.push_str(&text[last..range.start]);
        last = range.end;
        let chip = &text[range.clone()];
        if let Some(uri) = mentions::token_mention(&id) {
            out.push_str(&mentions::outgoing(&uri, chip));
            continue;
        }
        let Some(file) = staged.iter().find(|f| f.key.to_string() == id) else {
            out.push_str(chip);
            continue;
        };
        let slot = match files.iter().position(|f| f.key == file.key) {
            Some(slot) => slot,
            None => {
                files.push(file.clone());
                let slot = files.len() - 1;
                files.extend(
                    staged
                        .iter()
                        .filter(|text| snapshot::pairs(text, file))
                        .cloned(),
                );
                slot
            }
        };
        out.push_str(&attachment_anchor(slot));
    }
    out.push_str(&text[last..]);
    (out.trim().to_string(), files)
}

/// What a paste brings that becomes a card: files copied in Finder, or a
/// picture (a screenshot copied to the clipboard).
pub enum Pasted {
    Paths(Vec<PathBuf>),
    Image { bytes: Vec<u8>, mime: &'static str },
}

/// `None` for a text paste, which the composer inserts as usual.
pub fn pasted(content: &ClipboardContent) -> Option<Pasted> {
    if !content.files().is_empty() {
        return Some(Pasted::Paths(content.files().to_vec()));
    }
    content.image().map(|image| Pasted::Image {
        bytes: image.png().to_vec(),
        mime: "image/png",
    })
}

/// A pasted picture as a file to upload, within the gateway's limits.
pub fn pasted_image(
    bytes: Vec<u8>,
    mime: &str,
    staged: &[StagedFile],
    limits: &ironclaw_gpui_client::wire::AttachmentLimits,
) -> Result<StagedFile, String> {
    let ext = mime.rsplit('/').next().unwrap_or("png");
    let filename = format!("Pasted image.{ext}");
    let mime = attachments::mime_for(&filename, limits)
        .ok_or("This gateway does not accept pasted pictures of this type.")?;
    let file = StagedFile::from_bytes(filename, mime, bytes);
    attachments::check(&file, staged, limits)?;
    Ok(file)
}

/// The icon a composer pill shows: a SnapShot's frame, a mention's kind, or
/// the plain file icon.
pub fn pill_icon_for(snapshots: &[String], id: &str) -> Option<&'static Icon> {
    if snapshots.iter().any(|k| k == id) {
        return Some(&icons::FRAME);
    }
    if let Some(uri) = mentions::token_mention(id) {
        return Some(super::menu::kind_icon(super::menu::uri_kind(&uri)));
    }
    Some(&icons::FILE)
}

/// The keys of staged SnapShot pictures, for their pills' icon.
pub fn snapshot_keys(s: &State, staged: &[StagedFile]) -> Vec<String> {
    staged
        .iter()
        .filter(|f| snapshot::names(s, f).is_some())
        .map(|f| f.key.to_string())
        .collect()
}

/// Set `composer` to `text` with `pills` (byte ranges and ids; each label
/// is the text under its range), as a draft or a sent message comes back.
pub fn set_content(composer: &mut TextEdit, text: &str, pills: &[(Range<usize>, String)]) {
    composer.replace_text(text);
    let mut sorted: Vec<&(Range<usize>, String)> = pills.iter().collect();
    // Back to front, so earlier ranges stay put.
    sorted.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
    for (range, id) in sorted {
        let Some(label) = text.get(range.clone()) else {
            tracing::debug!("composer: pill outside its text not restored");
            continue;
        };
        let label = label.to_string();
        composer.replace_with_pill(range.clone(), id.clone(), &label);
    }
    let end = composer.text.len();
    composer.set_caret(end);
    composer.clear_history();
}

/// Composer content for a sent message's text: each mention link folded
/// back into its pill, for edit and resend.
pub fn restored(text: &str) -> (String, Vec<(Range<usize>, String)>) {
    let (plain, chips) = mentions::restore(text);
    let pills = chips.into_iter().map(|chip| (chip.range, chip.id)).collect();
    (plain, pills)
}

/// Pill text may hold no control characters.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// The one-line card that sits inside a sent message's text: icon tile and
/// name, the same look as the full attachment card, at the height of a line.
pub fn inline_card(s: &State, key: String, glyph: Option<&'static Icon>, title: &str, line_height: f32) -> El {
    let p = &s.palette;
    let tile = (line_height - 6.).max(12.);
    row()
        .key(key)
        .h(line_height - 2.)
        .my(1.)
        .gap(5.)
        .pl(2.)
        .pr(7.)
        .center_y()
        .radius(6.)
        .border(1., p.border_strong)
        .fill(p.hover)
        .child(
            row()
                .size(tile, tile)
                .center()
                .radius(4.)
                .border(1., p.border_strong)
                .child(icon(glyph.unwrap_or(&icons::FILE), 10.).color(p.text_muted)),
        )
        .child(
            text(title.to_string())
                .font(super::flow::type_at(s, 13., 18., 500.))
                .color(p.text)
                .truncate()
                .max_w(240.),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str) -> StagedFile {
        StagedFile::from_bytes(name.into(), "text/plain".into(), b"x".to_vec())
    }

    /// A file pill becomes an anchor at its place; a deleted pill drops its
    /// file from the staged list.
    #[test]
    fn pills_become_anchors_and_pruned_files_go() {
        let a = file("a.txt");
        let b = file("b.txt");
        let mut edit = TextEdit::new("see ");
        edit.set_caret(edit.text.len());
        edit.insert_pill(a.key.to_string(), "a.txt");
        edit.insert(" and ");
        edit.insert_pill(b.key.to_string(), "b.txt");
        let staged = vec![a.clone(), b.clone()];
        let (text, files) = outgoing(&edit, &staged);
        assert_eq!(text, format!("see {} and {}", attachment_anchor(0), attachment_anchor(1)));
        assert_eq!(files.len(), 2);

        let mut edit = TextEdit::new("see ");
        edit.set_caret(edit.text.len());
        edit.insert_pill(a.key.to_string(), "a.txt");
        let mut staged = vec![a, b];
        prune(&edit, &mut staged);
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].filename, "a.txt");
    }

    /// A draft's pills come back on the same ranges.
    #[test]
    fn set_content_restores_pills() {
        let mut edit = TextEdit::new("");
        set_content(&mut edit, "hi menu.rs there", &[(3..10, "m1".into())]);
        assert_eq!(edit.text, "hi menu.rs there");
        assert_eq!(pills(&edit), vec![(3..10, "m1".to_string())]);
    }
}
