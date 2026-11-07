//! A multi-line editor in the mono font (GPUI's memory and SKILL.md
//! editors: `Input` with `font_family(FONT_MONO)`). rune-kit's `TextArea`
//! fixes its text style, so this builds the same field on Rune's core
//! `text_input`: the kit field's fill, radius, resting shadow, hover wash
//! and focus ring, grown between `min_rows` and `max_rows` lines, then
//! scrolling with the caret kept in view.

use rune::{El, TextEdit, TextStyle, col, text_input};
use rune_kit::Kit;

use crate::state::State;

type Hook = Box<dyn FnOnce(El) -> El>;

/// Builder: `mono_editor(&edit).label(..).rows(8, 30).on_edit(..).build(k)`.
pub struct MonoEditor {
    edit: TextEdit,
    label: Option<String>,
    rows: (usize, usize),
    on_edit: Option<Hook>,
}

pub fn mono_editor(edit: &TextEdit) -> MonoEditor {
    MonoEditor {
        edit: edit.clone(),
        label: None,
        rows: (3, 3),
        on_edit: None,
    }
}

impl MonoEditor {
    /// The accessible name.
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// At least `min` lines tall, growing with the text up to `max`.
    pub fn rows(mut self, min: usize, max: usize) -> Self {
        let min = min.max(1);
        self.rows = (min, max.max(min));
        self
    }

    /// Where edits land, as `TextArea::on_edit`.
    pub fn on_edit(
        mut self,
        f: impl for<'a> Fn(&'a mut State) -> &'a mut TextEdit + 'static,
    ) -> Self {
        self.on_edit = Some(Box::new(move |e: El| e.on_edit(f)));
        self
    }

    pub fn build(self, k: &Kit) -> El {
        let c = &k.c;
        // The kit field's size and leading, in the mono family.
        let style = TextStyle {
            line_height: (13.0f32 * 1.35 * 2.0).round() / 2.0,
            ..k.ty.ui
        }
        .mono();
        let mut field = text_input(&self.edit, style)
            .key("input")
            .w_full()
            .color(c.text_base)
            .caret(c.text_base, k.selection());
        if let Some(name) = self.label.filter(|l| !l.is_empty()) {
            field = field.label(name);
        }
        if let Some(hook) = self.on_edit {
            field = hook(field);
        }
        let (min, max) = self.rows;
        let mut e = col()
            .role("textbox")
            .padding(8.)
            .w_full()
            .child(rune::auto_grow(field, min, max))
            .radius(k.radius.md)
            .fill(c.bg_base)
            .token_motion(k.motion.control);
        if self.edit.focused {
            e = e.outline(1., -1., c.border_focus);
        } else {
            for s in k.shadow_layers(&k.elevation.button_neutral) {
                e = e.box_shadow(s);
            }
            e = e.hover(c.hover);
        }
        e
    }
}
