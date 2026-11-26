//! The profile badge, ported from t3code's project icon (`ProjectMonogram`
//! and its favicon fallback chain): the folder's own logo when it has one
//! (`workspace::profile_icons`), else two letters in a monospace face on a
//! 14 % wash of their colour, the colour one of Tailwind's 18 named hues
//! chosen from the name (the -600 shade on light, -400 on dark).

use rune::{Color, El, SurfaceFit, TextStyle, el, row, text};
use unicode_normalization::UnicodeNormalization;

use crate::design::Palette;
use crate::state::State;

/// The badge's side (t3code's `size-4`), also the slot rows reserve for it.
pub(in crate::workspace) const MARK_SIZE: f32 = 16.;
/// A logo draws a little smaller (t3code's `size-3.5`).
const LOGO_SIZE: f32 = 14.;

/// Tailwind v4's 18 named colours in t3code's order, as OKLCH
/// (lightness, chroma, hue°): the -600 shade, then the -400.
/// OKLCH: lightness (0–1), chroma, hue in degrees.
type Oklch = (f32, f32, f32);

const COLORS: [(Oklch, Oklch); 18] = [
    ((0.446, 0.030, 256.802), (0.707, 0.022, 261.325)), // gray
    ((0.577, 0.245, 27.325), (0.704, 0.191, 22.216)),   // red
    ((0.646, 0.222, 41.116), (0.750, 0.183, 55.934)),   // orange
    ((0.666, 0.179, 58.318), (0.828, 0.189, 84.429)),   // amber
    ((0.681, 0.162, 75.834), (0.852, 0.199, 91.936)),   // yellow
    ((0.648, 0.200, 131.684), (0.841, 0.238, 128.850)), // lime
    ((0.627, 0.194, 149.214), (0.792, 0.209, 151.711)), // green
    ((0.596, 0.145, 163.225), (0.765, 0.177, 163.223)), // emerald
    ((0.600, 0.118, 184.704), (0.777, 0.152, 181.912)), // teal
    ((0.609, 0.126, 221.723), (0.789, 0.154, 211.530)), // cyan
    ((0.588, 0.158, 241.966), (0.746, 0.160, 232.661)), // sky
    ((0.546, 0.245, 262.881), (0.707, 0.165, 254.624)), // blue
    ((0.511, 0.262, 276.966), (0.673, 0.182, 276.935)), // indigo
    ((0.541, 0.281, 293.009), (0.702, 0.183, 293.541)), // violet
    ((0.558, 0.288, 302.321), (0.714, 0.203, 305.504)), // purple
    ((0.591, 0.293, 322.896), (0.740, 0.238, 322.160)), // fuchsia
    ((0.592, 0.249, 0.584), (0.718, 0.202, 349.761)),   // pink
    ((0.586, 0.253, 17.585), (0.712, 0.194, 13.428)),   // rose
];

/// The badge for `profile_id` on `computer`: its logo, or its letters.
pub(in crate::workspace) fn profile_mark(
    s: &State,
    computer: &str,
    profile_id: &str,
    name: &str,
) -> El {
    match crate::workspace::profile_icons::image(s, computer, profile_id) {
        Some(image) => el()
            .size(MARK_SIZE, MARK_SIZE)
            .center()
            .a11y_hidden()
            .child(
                el().size(LOGO_SIZE, LOGO_SIZE)
                    .clip_rounded(LOGO_SIZE * 0.25)
                    .pixels_fit(image.pixels(), SurfaceFit::Fit),
            ),
        None => monogram(&s.palette, name),
    }
}

/// t3code's monogram: the letters in their colour on a 14 % wash of it,
/// corners at a quarter of the side.
fn monogram(p: &Palette, name: &str) -> El {
    let color = profile_color(p, name);
    row()
        .size(MARK_SIZE, MARK_SIZE)
        .radius(MARK_SIZE * 0.25)
        .fill(color.alpha(0.14))
        .center()
        .a11y_hidden()
        .child(
            text(profile_initials(name))
                // t3code's 8.25 px letters are stretched to 12 of the 16 px;
                // Rune draws glyphs unstretched, so a size whose two
                // monospace advances span about the same width.
                .font(TextStyle::new(9.5, 700.).mono())
                .color(color),
        )
}

/// A profile's colour from its name (t3code's `d`): the name, NFKC,
/// trimmed and lowercased (or "project"), hashed over its code points as
/// `h = (h * 31 + c) % 18`.
pub(in crate::workspace) fn profile_color(p: &Palette, name: &str) -> Color {
    let ((dl, dc, dh), (ll, lc, lh)) = COLORS[color_index(name)];
    let (l, c, h) = if p.light { (dl, dc, dh) } else { (ll, lc, lh) };
    let h = h.to_radians();
    Color::from_oklab([l, c * h.cos(), c * h.sin(), 1.])
}

fn color_index(name: &str) -> usize {
    let key = name.nfkc().collect::<String>().trim().to_lowercase();
    let key = if key.is_empty() {
        "project".into()
    } else {
        key
    };
    let n = COLORS.len() as u32;
    key.chars().fold(0u32, |h, c| (h * 31 + c as u32) % n) as usize
}

/// Two letters for a profile (t3code's `l`): the first letter of the
/// first word, then the first digit after it in that word, else the last
/// word's first letter, else the first word's last letter ("Web2" → "W2",
/// "Business analytics" → "BA", "Luft" → "LT"); one Han, kana or Hangul
/// character stands alone; no letters at all → "PR".
pub(in crate::workspace) fn profile_initials(name: &str) -> String {
    let normalized: String = name.nfkc().collect();
    let words: Vec<Vec<char>> = normalized
        .trim()
        .split(|c: char| !(c.is_alphabetic() || c.is_numeric()))
        .filter(|w| !w.is_empty())
        .map(|w| w.chars().collect())
        .collect();
    let Some(first) = words.first() else {
        return "PR".into();
    };
    let lead = first[0];
    if is_cjk(lead) {
        return lead.to_string();
    }
    let second = first[1..]
        .iter()
        .copied()
        .find(|c| c.is_numeric())
        .or_else(|| match words.last() {
            Some(last) if words.len() > 1 => last.first().copied(),
            _ => first.last().copied(),
        })
        .unwrap_or(lead);
    format!("{lead}{second}")
        .to_uppercase()
        .chars()
        .take(2)
        .collect()
}

/// Han, Hiragana, Katakana or Hangul (t3code's script test on the first
/// character).
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x2E80..=0x2FDF      // CJK and Kangxi radicals
        | 0x3005..=0x3007    // 々 〆 〇
        | 0x3021..=0x3029    // Hangzhou numerals
        | 0x3038..=0x303B
        | 0x3040..=0x30FF    // Hiragana, Katakana
        | 0x3130..=0x318F    // Hangul compatibility jamo
        | 0x31F0..=0x31FF    // Katakana phonetic extensions
        | 0x3400..=0x4DBF    // CJK extension A
        | 0x4E00..=0x9FFF    // CJK unified
        | 0xA960..=0xA97F    // Hangul jamo extended A
        | 0xAC00..=0xD7FF    // Hangul syllables, jamo extended B
        | 0xF900..=0xFAFF    // CJK compatibility
        | 0xFF66..=0xFF9F    // halfwidth Katakana
        | 0xFFA0..=0xFFDC    // halfwidth Hangul
        | 0x1100..=0x11FF    // Hangul jamo
        | 0x1B000..=0x1B16F  // kana supplement / extended
        | 0x20000..=0x3134F  // CJK extensions B–G
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_follow_t3codes_rule() {
        assert_eq!(profile_initials("Business analytics"), "BA");
        assert_eq!(profile_initials("IronClaw Builder"), "IB");
        assert_eq!(profile_initials("Luft"), "LT");
        assert_eq!(profile_initials("Default"), "DT");
        assert_eq!(profile_initials("Web2 auto-battler"), "W2");
        assert_eq!(profile_initials("Avallon Case Study"), "AS");
        assert_eq!(profile_initials("x"), "XX");
        assert_eq!(profile_initials("  —  "), "PR");
        assert_eq!(profile_initials("東京 project"), "東");
        assert_eq!(profile_initials("ｗｅｂ"), "WB", "NFKC folds fullwidth");
    }

    #[test]
    fn colour_hashes_the_trimmed_lowercased_name() {
        // t3code: h = (h*31 + codepoint) % 18 over "luft".
        let expect = "luft".chars().fold(0u32, |h, c| (h * 31 + c as u32) % 18) as usize;
        assert_eq!(color_index("  Luft "), expect);
        assert_eq!(color_index("LUFT"), expect);
        let project = "project"
            .chars()
            .fold(0u32, |h, c| (h * 31 + c as u32) % 18) as usize;
        assert_eq!(color_index("   "), project);
    }
}
