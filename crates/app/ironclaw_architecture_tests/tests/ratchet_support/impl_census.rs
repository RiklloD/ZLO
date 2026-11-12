//! Implementor census: `impl … for …` header extraction, `use … as …` alias
//! resolution, and matching — the hardened matcher born in
//! `reborn_sealed_evidence_mint_ratchet.rs` (WS12 F1: a planted forgery passed
//! every line-based scan), shared since 2026-11-11 so
//! `reborn_authorized_seal_ratchet.rs` stops running the weaker line-based
//! `contains("CapabilityAuthorizer for")` scan. Callers strip comments and
//! string literals first (`strip_comments_and_strings`).

// Every arch-test binary compiles `ratchet_support` independently and each
// uses a subset of this census (the same reason every `ratchet_support` item
// carries `#[allow(dead_code)]`).
#![allow(dead_code)]

use std::collections::BTreeSet;

/// One `impl … for …` header, collapsed onto a single line.
pub(crate) struct ImplHeader {
    /// 1-based line of the `impl` keyword, for the offender report.
    pub line: usize,
    pub text: String,
}

/// Every `impl` header in already-stripped `source`, whitespace-normalized.
///
/// Line-oriented matching is what let a rogue impl hide behind a line break:
/// `impl ChannelIngressVerifier\n    for RogueAdapter` never puts the trait
/// name and `for` on one line, so a `contains("<Trait> for")` scan over
/// `.lines()` read it as absent. Collapsing the header first removes that
/// formatting degree of freedom entirely.
///
/// The header region runs from the `impl` keyword to the first `{`, `;`, or
/// `where`, or — at angle-bracket depth zero — `,` or `)`. The last two stop an
/// `impl Trait` in argument position from swallowing a function body; the depth
/// check keeps `impl T for HashMap<K, V>` intact. `impl` in return position
/// (`-> impl Iterator<…>`) yields a header with no `for`, which matches nothing.
pub(crate) fn impl_headers(source: &str) -> Vec<ImplHeader> {
    let chars: Vec<char> = source.chars().collect();
    let mut headers = Vec::new();
    let mut line = 1usize;
    let mut index = 0usize;
    while index < chars.len() {
        if chars[index] == '\n' {
            line += 1;
            index += 1;
            continue;
        }
        if !starts_word(&chars, index, "impl") {
            index += 1;
            continue;
        }
        let start_line = line;
        let mut cursor = index + "impl".len();
        let mut depth = 0usize;
        let mut text = String::from("impl");
        while cursor < chars.len() {
            let ch = chars[cursor];
            // `->` is an arrow, not a closing angle bracket. Without this a
            // return type inside generics (`Box<dyn Fn() -> u8>`) would close
            // the bracket early and truncate the header.
            if ch == '-' && chars.get(cursor + 1) == Some(&'>') {
                text.push_str("->");
                cursor += 2;
                continue;
            }
            match ch {
                '{' | ';' => break,
                '<' => depth += 1,
                '>' => depth = depth.saturating_sub(1),
                ',' | ')' if depth == 0 => break,
                '\n' => line += 1,
                _ => {}
            }
            if depth == 0 && starts_word(&chars, cursor, "where") {
                break;
            }
            text.push(ch);
            cursor += 1;
        }
        headers.push(ImplHeader {
            line: start_line,
            text: text.split_whitespace().collect::<Vec<_>>().join(" "),
        });
        index = cursor;
    }
    headers
}

/// The canonical trait name plus every name bound to it by a `use … as …` in
/// this file.
///
/// `use path::ChannelIngressVerifier as Verifier; impl Verifier for Rogue {}`
/// never names the trait on the impl line, so a census keyed only to the
/// canonical spelling cannot see it. Braced groups
/// (`use path::{Trait as V, Other}`) and raw-identifier aliases (`as r#type`)
/// bind the same way and are resolved the same way. Cross-file re-export
/// chains (`pub use … as …` in crate A, imported by crate B) stay out of reach
/// of a per-file census — recorded as residual, not closed.
pub(crate) fn implementor_names(source: &str, trait_name: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    names.insert(trait_name.to_string());
    let chars: Vec<char> = source.chars().collect();
    let mut index = 0usize;
    while index < chars.len() {
        if !starts_word(&chars, index, trait_name) {
            index += 1;
            continue;
        }
        let mut cursor = index + trait_name.chars().count();
        // ` as <ident>` — the only construct that can rename a trait in scope.
        // A cast (`x as u64`) cannot apply to a trait name, so no disambiguation
        // beyond the word boundary is needed.
        while chars.get(cursor).is_some_and(|c| c.is_whitespace()) {
            cursor += 1;
        }
        if starts_word(&chars, cursor, "as") {
            cursor += 2;
            while chars.get(cursor).is_some_and(|c| c.is_whitespace()) {
                cursor += 1;
            }
            if chars.get(cursor) == Some(&'r') && chars.get(cursor + 1) == Some(&'#') {
                cursor += 2;
            }
            let alias: String = chars[cursor..]
                .iter()
                .take_while(|c| is_ident_char(**c))
                .collect();
            if !alias.is_empty() {
                names.insert(alias);
            }
        }
        index += trait_name.chars().count();
    }
    names
}

/// Whether a collapsed header implements one of `names`.
///
/// Requires the name to appear on an identifier boundary — so a longer trait
/// that merely ends in the same text (`MyChannelIngressVerifier`) is not a hit
/// — followed, after any generic arguments, by the `for` keyword. Qualified
/// paths (`a::b::Trait for`) and generic impls (`impl<T> Trait<T> for`) still
/// match, which is what the pre-hardening scan did.
pub(crate) fn header_implements(header: &str, names: &BTreeSet<String>) -> bool {
    let chars: Vec<char> = header.chars().collect();
    for name in names {
        let mut index = 0usize;
        while index < chars.len() {
            if !starts_word(&chars, index, name) {
                index += 1;
                continue;
            }
            let mut cursor = index + name.chars().count();
            // Whitespace may separate the name from its generic arguments —
            // `impl Trait <> for X {}` compiles, and the header collapse *makes*
            // that space whenever a line break falls between the two. Skipping
            // it before looking for `<` is what keeps the normalization total;
            // without it the generic-argument branch is never entered, the
            // whitespace loop below lands on `<`, `for` is not found, and the
            // impl mints evidence undetected.
            while chars.get(cursor).is_some_and(|c| c.is_whitespace()) {
                cursor += 1;
            }
            // Skip the trait's own generic arguments: `Trait<T> for X`.
            if chars.get(cursor) == Some(&'<') {
                let mut depth = 0usize;
                while cursor < chars.len() {
                    // `->` inside the arguments is an arrow, not a closer.
                    if chars[cursor] == '-' && chars.get(cursor + 1) == Some(&'>') {
                        cursor += 2;
                        continue;
                    }
                    match chars[cursor] {
                        '<' => depth += 1,
                        '>' => {
                            depth = depth.saturating_sub(1);
                            if depth == 0 {
                                cursor += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    cursor += 1;
                }
            }
            while chars.get(cursor).is_some_and(|c| c.is_whitespace()) {
                cursor += 1;
            }
            if starts_word(&chars, cursor, "for") {
                return true;
            }
            index += name.chars().count();
        }
    }
    false
}

/// Whether `word` starts at `index` on both-side identifier boundaries.
pub(crate) fn starts_word(chars: &[char], index: usize, word: &str) -> bool {
    if index > 0 && is_ident_char(chars[index - 1]) {
        return false;
    }
    let mut cursor = index;
    for expected in word.chars() {
        if chars.get(cursor) != Some(&expected) {
            return false;
        }
        cursor += 1;
    }
    !chars.get(cursor).is_some_and(|c| is_ident_char(*c))
}

/// Identifier character for the census's word-boundary checks.
pub(crate) fn is_ident_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

/// Whether already-unstripped `source` implements `trait_name` under any
/// spelling the census resolves (qualified path, generic args, line-broken
/// header, in-file alias). The full per-file pipeline, so self-tests drive the
/// same logic the gates run.
pub(crate) fn file_implements(source: &str, trait_name: &str) -> bool {
    let stripped = super::strip_comments_and_strings(source);
    let names = implementor_names(&stripped, trait_name);
    impl_headers(&stripped)
        .iter()
        .any(|header| header_implements(&header.text, &names))
}
