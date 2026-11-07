//! Lucide icons (ISC) the chat draws that the shared catalog
//! (`crate::icons`) lacks, in the same shape. GPUI drew them as
//! `ExtraIcon::{ListChecks, CircleDashed, Circle, CircleQuestionMark}`.
//! Rune's icon parser reads `fill`/`stroke` on each element, not on a
//! `<g>`, so every element carries them.

use rune::Icon;

macro_rules! lucide {
    ($name:ident, $id:literal, $body:literal) => {
        pub static $name: Icon = Icon {
            name: $id,
            view: 24.0,
            body: $body,
        };
    };
}

lucide!(
    LIST_CHECKS,
    "list-checks",
    r##"<path d="M13 5h8" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" /> <path d="M13 12h8" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" /> <path d="M13 19h8" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" /> <path d="m3 17 2 2 4-4" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" /> <path d="m3 7 2 2 4-4" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" />"##
);
lucide!(
    CIRCLE_DASHED,
    "circle-dashed",
    r##"<path d="M10.1 2.182a10 10 0 0 1 3.8 0" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" /> <path d="M13.9 21.818a10 10 0 0 1-3.8 0" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" /> <path d="M17.609 3.721a10 10 0 0 1 2.69 2.7" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" /> <path d="M2.182 13.9a10 10 0 0 1 0-3.8" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" /> <path d="M20.279 17.609a10 10 0 0 1-2.7 2.69" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" /> <path d="M21.818 10.1a10 10 0 0 1 0 3.8" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" /> <path d="M3.721 6.391a10 10 0 0 1 2.7-2.69" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" /> <path d="M6.391 20.279a10 10 0 0 1-2.69-2.7" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" />"##
);
lucide!(CIRCLE, "circle", r##"<circle cx="12" cy="12" r="10" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" />"##);
lucide!(
    CIRCLE_QUESTION_MARK,
    "circle-question-mark",
    r##"<circle cx="12" cy="12" r="10" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" /> <path d="M9.09 9a3 3 0 0 1 5.83 1c0 2-3 3-3 3" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" /> <path d="M12 17h.01" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" />"##
);
