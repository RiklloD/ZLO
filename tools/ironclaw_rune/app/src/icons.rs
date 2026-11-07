//! Lucide icons (ISC), the exact SVGs the GPUI app drew through gpui-kit
//! (`gpui-kit-assets` 0.8.0), as Rune `Icon`s. GPUI `IconName::ChevronDown`
//! is `icons::CHEVRON_DOWN` here. Draw with `rune::icon(&icons::X, size)`
//! and colour with `.color(..)`. Generated once by the port; add more from
//! the same catalog in the same shape.
#![allow(dead_code)]

use rune::Icon;

/// GPUI `IconName::Activity`.
pub static ACTIVITY: Icon = Icon {
    name: "activity",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 12h-2.48a2 2 0 0 0-1.93 1.46l-2.35 8.36a.25.25 0 0 1-.48 0L9.24 2.18a.25.25 0 0 0-.48 0l-2.35 8.36A2 2 0 0 1 4.49 12H2" /></g>"##,
};
/// GPUI `IconName::AlarmClock`.
pub static ALARM_CLOCK: Icon = Icon {
    name: "alarm-clock",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="13" r="8" /> <path d="M12 9v4l2 2" /> <path d="M5 3 2 6" /> <path d="m22 6-3-3" /> <path d="M6.38 18.7 4 21" /> <path d="M17.64 18.67 20 21" /></g>"##,
};
/// GPUI `IconName::ArrowLeft`.
pub static ARROW_LEFT: Icon = Icon {
    name: "arrow-left",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m12 19-7-7 7-7" /> <path d="M19 12H5" /></g>"##,
};
/// GPUI `IconName::ArrowRight`.
pub static ARROW_RIGHT: Icon = Icon {
    name: "arrow-right",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12h14" /> <path d="m12 5 7 7-7 7" /></g>"##,
};
/// GPUI `IconName::ArrowUp`.
pub static ARROW_UP: Icon = Icon {
    name: "arrow-up",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m5 12 7-7 7 7" /> <path d="M12 19V5" /></g>"##,
};
/// GPUI `IconName::AudioLines`.
pub static AUDIO_LINES: Icon = Icon {
    name: "audio-lines",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M2 10v3" /> <path d="M6 6v11" /> <path d="M10 3v18" /> <path d="M14 8v7" /> <path d="M18 5v13" /> <path d="M22 10v3" /></g>"##,
};
/// GPUI `IconName::Ban`.
pub static BAN: Icon = Icon {
    name: "ban",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10" /> <path d="M4.929 4.929 19.07 19.071" /></g>"##,
};
/// GPUI `IconName::Bell`.
pub static BELL: Icon = Icon {
    name: "bell",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M10.268 21a2 2 0 0 0 3.464 0" /> <path d="M3.262 15.326A1 1 0 0 0 4 17h16a1 1 0 0 0 .74-1.673C19.41 13.956 18 12.499 18 8A6 6 0 0 0 6 8c0 4.499-1.411 5.956-2.738 7.326" /></g>"##,
};
/// GPUI `IconName::BookOpen`.
pub static BOOK_OPEN: Icon = Icon {
    name: "book-open",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5v16" /> <path d="M20.001 19A2 2 0 0022 17V5a2 2 0 00-1.999-2L16 3.002A5 5 0 0012 5a5 5 0 00-4-2H4a2 2 0 00-2 2v12a2 2 0 001.999 2H8a5 5 0 014 2 5 5 0 014-2z" /></g>"##,
};
/// GPUI `IconName::Bot`.
pub static BOT: Icon = Icon {
    name: "bot",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 8V4H8" /> <rect width="16" height="12" x="4" y="8" rx="2" /> <path d="M2 14h2" /> <path d="M20 14h2" /> <path d="M15 13v2" /> <path d="M9 13v2" /></g>"##,
};
/// GPUI `IconName::Calendar`.
pub static CALENDAR: Icon = Icon {
    name: "calendar",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M8 2v3" /> <path d="M16 2v3" /> <rect x="3" y="3" width="18" height="18" rx="2" /> <path d="M3 9h18" /></g>"##,
};
/// GPUI `IconName::ChartPie`.
pub static CHART_PIE: Icon = Icon {
    name: "chart-pie",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12c.552 0 1.005-.449.95-.998a10 10 0 0 0-8.953-8.951c-.55-.055-.998.398-.998.95v8a1 1 0 0 0 1 1z" /> <path d="M21.21 15.89A10 10 0 1 1 8 2.83" /></g>"##,
};
/// GPUI `IconName::Check`.
pub static CHECK: Icon = Icon {
    name: "check",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5" /></g>"##,
};
/// GPUI `IconName::ChevronDown`.
pub static CHEVRON_DOWN: Icon = Icon {
    name: "chevron-down",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m6 9 6 6 6-6" /></g>"##,
};
/// GPUI `IconName::ChevronLeft`.
pub static CHEVRON_LEFT: Icon = Icon {
    name: "chevron-left",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m15 18-6-6 6-6" /></g>"##,
};
/// GPUI `IconName::ChevronRight`.
pub static CHEVRON_RIGHT: Icon = Icon {
    name: "chevron-right",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m9 18 6-6-6-6" /></g>"##,
};
/// GPUI `IconName::ChevronUp`.
pub static CHEVRON_UP: Icon = Icon {
    name: "chevron-up",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m18 15-6-6-6 6" /></g>"##,
};
/// GPUI `IconName::ChevronsUpDown`.
pub static CHEVRONS_UP_DOWN: Icon = Icon {
    name: "chevrons-up-down",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m7 15 5 5 5-5" /> <path d="m7 9 5-5 5 5" /></g>"##,
};
/// GPUI `IconName::CircleAlert`.
pub static CIRCLE_ALERT: Icon = Icon {
    name: "circle-alert",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10" /> <line x1="12" x2="12" y1="8" y2="12" /> <line x1="12" x2="12.01" y1="16" y2="16" /></g>"##,
};
/// GPUI `IconName::CircleCheck`.
pub static CIRCLE_CHECK: Icon = Icon {
    name: "circle-check",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10" /> <path d="m16 9-5.5 5.5L8 12" /></g>"##,
};
/// GPUI `IconName::CircleDot`.
pub static CIRCLE_DOT: Icon = Icon {
    name: "circle-dot",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="1" /> <circle cx="12" cy="12" r="10" /></g>"##,
};
/// GPUI `IconName::CircleUser`.
pub static CIRCLE_USER: Icon = Icon {
    name: "circle-user",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10" /> <circle cx="12" cy="10" r="3" /> <path d="M7 20.662V19a2 2 0 0 1 2-2h6a2 2 0 0 1 2 2v1.662" /></g>"##,
};
/// GPUI `IconName::CircleX`.
pub static CIRCLE_X: Icon = Icon {
    name: "circle-x",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10" /> <path d="m15 9-6 6" /> <path d="m9 9 6 6" /></g>"##,
};
/// GPUI `IconName::Close`.
pub static CLOSE: Icon = Icon {
    name: "close",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M18 6 6 18"/><path d="m6 6 12 12"/></g>"##,
};
/// GPUI `IconName::Copy`.
pub static COPY: Icon = Icon {
    name: "copy",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect width="14" height="14" x="8" y="8" rx="2" ry="2" /> <path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2" /></g>"##,
};
/// GPUI `IconName::Dash`.
pub static DASH: Icon = Icon {
    name: "dash",
    view: 24.0,
    body: r##"<g fill="none" stroke="black" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12h14"/></g>"##,
};
/// GPUI `IconName::Delete`.
pub static DELETE: Icon = Icon {
    name: "delete",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M10 5a2 2 0 0 0-1.344.519l-6.328 5.74a1 1 0 0 0 0 1.481l6.328 5.741A2 2 0 0 0 10 19h10a2 2 0 0 0 2-2V7a2 2 0 0 0-2-2z" /> <path d="m12 9 6 6" /> <path d="m18 9-6 6" /></g>"##,
};
/// GPUI `IconName::Ellipsis`.
pub static ELLIPSIS: Icon = Icon {
    name: "ellipsis",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="1" /> <circle cx="19" cy="12" r="1" /> <circle cx="5" cy="12" r="1" /></g>"##,
};
/// GPUI `IconName::ExternalLink`.
pub static EXTERNAL_LINK: Icon = Icon {
    name: "external-link",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M15 3h6v6" /> <path d="M10 14 21 3" /> <path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6" /></g>"##,
};
/// GPUI `IconName::Eye`.
pub static EYE: Icon = Icon {
    name: "eye",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M2.062 12.348a1 1 0 0 1 0-.696 10.75 10.75 0 0 1 19.876 0 1 1 0 0 1 0 .696 10.75 10.75 0 0 1-19.876 0" /> <circle cx="12" cy="12" r="3" /></g>"##,
};
/// GPUI `IconName::EyeOff`.
pub static EYE_OFF: Icon = Icon {
    name: "eye-off",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M10.733 5.076a10.744 10.744 0 0 1 11.205 6.575 1 1 0 0 1 0 .696 10.747 10.747 0 0 1-1.444 2.49" /> <path d="M14.084 14.158a3 3 0 0 1-4.242-4.242" /> <path d="M17.479 17.499a10.75 10.75 0 0 1-15.417-5.151 1 1 0 0 1 0-.696 10.75 10.75 0 0 1 4.446-5.143" /> <path d="m2 2 20 20" /></g>"##,
};
/// GPUI `IconName::File`.
pub static FILE: Icon = Icon {
    name: "file",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M6 22a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h8a2.4 2.4 0 0 1 1.704.706l3.588 3.588A2.4 2.4 0 0 1 20 8v12a2 2 0 0 1-2 2z" /> <path d="M14 2v5a1 1 0 0 0 1 1h5" /></g>"##,
};
/// GPUI `IconName::FileCode`.
pub static FILE_CODE: Icon = Icon {
    name: "file-code",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M6 22a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h8a2.4 2.4 0 0 1 1.704.706l3.588 3.588A2.4 2.4 0 0 1 20 8v12a2 2 0 0 1-2 2z" /> <path d="M14 2v5a1 1 0 0 0 1 1h5" /> <path d="M10 12.5 8 15l2 2.5" /> <path d="m14 12.5 2 2.5-2 2.5" /></g>"##,
};
/// GPUI `IconName::FileDiff`.
pub static FILE_DIFF: Icon = Icon {
    name: "file-diff",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M6 22a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h8a2.4 2.4 0 0 1 1.704.706l3.588 3.588A2.4 2.4 0 0 1 20 8v12a2 2 0 0 1-2 2z" /> <path d="M9 10h6" /> <path d="M12 13V7" /> <path d="M9 17h6" /></g>"##,
};
/// GPUI `IconName::FileText`.
pub static FILE_TEXT: Icon = Icon {
    name: "file-text",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M6 22a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h8a2.4 2.4 0 0 1 1.704.706l3.588 3.588A2.4 2.4 0 0 1 20 8v12a2 2 0 0 1-2 2z" /> <path d="M14 2v5a1 1 0 0 0 1 1h5" /> <path d="M10 9H8" /> <path d="M16 13H8" /> <path d="M16 17H8" /></g>"##,
};
/// GPUI `IconName::Folder`.
pub static FOLDER: Icon = Icon {
    name: "folder",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z" /></g>"##,
};
/// GPUI `IconName::FolderGit`.
pub static FOLDER_GIT: Icon = Icon {
    name: "folder-git",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="13" r="2" /> <path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z" /> <path d="M14 13h3" /> <path d="M7 13h3" /></g>"##,
};
/// GPUI `IconName::FolderOpen`.
pub static FOLDER_OPEN: Icon = Icon {
    name: "folder-open",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m6 14 1.5-2.9A2 2 0 0 1 9.24 10H20a2 2 0 0 1 1.94 2.5l-1.54 6a2 2 0 0 1-1.95 1.5H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h3.9a2 2 0 0 1 1.69.9l.81 1.2a2 2 0 0 0 1.67.9H18a2 2 0 0 1 2 2v2" /></g>"##,
};
/// GPUI `IconName::FolderPlus`.
pub static FOLDER_PLUS: Icon = Icon {
    name: "folder-plus",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 10v6" /> <path d="M9 13h6" /> <path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z" /></g>"##,
};
/// GPUI `IconName::FolderTree`.
pub static FOLDER_TREE: Icon = Icon {
    name: "folder-tree",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 10a1 1 0 0 0 1-1V6a1 1 0 0 0-1-1h-2.5a1 1 0 0 1-.8-.4l-.9-1.2A1 1 0 0 0 15 3h-2a1 1 0 0 0-1 1v5a1 1 0 0 0 1 1Z" /> <path d="M20 21a1 1 0 0 0 1-1v-3a1 1 0 0 0-1-1h-2.9a1 1 0 0 1-.88-.55l-.42-.85a1 1 0 0 0-.92-.6H13a1 1 0 0 0-1 1v5a1 1 0 0 0 1 1Z" /> <path d="M3 5a2 2 0 0 0 2 2h3" /> <path d="M3 3v13a2 2 0 0 0 2 2h3" /></g>"##,
};
/// GPUI `IconName::Frame`.
pub static FRAME: Icon = Icon {
    name: "frame",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="22" x2="2" y1="6" y2="6" /> <line x1="22" x2="2" y1="18" y2="18" /> <line x1="6" x2="6" y1="2" y2="22" /> <line x1="18" x2="18" y1="2" y2="22" /></g>"##,
};
/// GPUI `IconName::GitBranch`.
pub static GIT_BRANCH: Icon = Icon {
    name: "git-branch",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M15 6a9 9 0 0 0-9 9V3" /> <circle cx="18" cy="6" r="3" /> <circle cx="6" cy="18" r="3" /></g>"##,
};
/// GPUI `IconName::GitCommitHorizontal`.
pub static GIT_COMMIT_HORIZONTAL: Icon = Icon {
    name: "git-commit-horizontal",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3" /> <line x1="3" x2="9" y1="12" y2="12" /> <line x1="15" x2="21" y1="12" y2="12" /></g>"##,
};
/// GPUI `IconName::GitCompare`.
pub static GIT_COMPARE: Icon = Icon {
    name: "git-compare",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="18" cy="18" r="3" /> <circle cx="6" cy="6" r="3" /> <path d="M13 6h3a2 2 0 0 1 2 2v7" /> <path d="M11 18H8a2 2 0 0 1-2-2V9" /></g>"##,
};
/// GPUI `IconName::GitFork`.
pub static GIT_FORK: Icon = Icon {
    name: "git-fork",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="18" r="3" /> <circle cx="6" cy="6" r="3" /> <circle cx="18" cy="6" r="3" /> <path d="M18 9v2c0 .6-.4 1-1 1H7c-.6 0-1-.4-1-1V9" /> <path d="M12 12v3" /></g>"##,
};
/// GPUI `IconName::GitPullRequest`.
pub static GIT_PULL_REQUEST: Icon = Icon {
    name: "git-pull-request",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="18" cy="18" r="3" /> <circle cx="6" cy="6" r="3" /> <path d="M13 6h3a2 2 0 0 1 2 2v7" /> <line x1="6" x2="6" y1="9" y2="21" /></g>"##,
};
/// GPUI `IconName::Globe`.
pub static GLOBE: Icon = Icon {
    name: "globe",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10" /> <path d="M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20" /> <path d="M2 12h20" /></g>"##,
};
/// GPUI `IconName::House`.
pub static HOUSE: Icon = Icon {
    name: "house",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M15 21v-8a1 1 0 0 0-1-1h-4a1 1 0 0 0-1 1v8" /> <path d="M3 10a2 2 0 0 1 .709-1.528l7-6a2 2 0 0 1 2.582 0l7 6A2 2 0 0 1 21 10v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" /></g>"##,
};
/// GPUI `IconName::Image`.
pub static IMAGE: Icon = Icon {
    name: "image",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect width="18" height="18" x="3" y="3" rx="2" ry="2" /> <circle cx="9" cy="9" r="2" /> <path d="m21 15-3.086-3.086a2 2 0 0 0-2.828 0L6 21" /></g>"##,
};
/// GPUI `IconName::Inbox`.
pub static INBOX: Icon = Icon {
    name: "inbox",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="22 12 16 12 14 15 10 15 8 12 2 12" /> <path d="M5.45 5.11 2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.45-6.89A2 2 0 0 0 16.76 4H7.24a2 2 0 0 0-1.79 1.11z" /></g>"##,
};
/// GPUI `IconName::Info`.
pub static INFO: Icon = Icon {
    name: "info",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10" /> <path d="M12 16v-4" /> <path d="M12 8h.01" /></g>"##,
};
/// GPUI `IconName::Laptop`.
pub static LAPTOP: Icon = Icon {
    name: "laptop",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M18 5a2 2 0 0 1 2 2v8.526a2 2 0 0 0 .212.897l1.068 2.127a1 1 0 0 1-.9 1.45H3.62a1 1 0 0 1-.9-1.45l1.068-2.127A2 2 0 0 0 4 15.526V7a2 2 0 0 1 2-2z" /> <path d="M20.054 15.987H3.946" /></g>"##,
};
/// GPUI `IconName::LayoutDashboard`.
pub static LAYOUT_DASHBOARD: Icon = Icon {
    name: "layout-dashboard",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect width="7" height="9" x="3" y="3" rx="1" /> <rect width="7" height="5" x="14" y="3" rx="1" /> <rect width="7" height="9" x="14" y="12" rx="1" /> <rect width="7" height="5" x="3" y="16" rx="1" /></g>"##,
};
/// GPUI `IconName::LayoutGrid`.
pub static LAYOUT_GRID: Icon = Icon {
    name: "layout-grid",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect width="7" height="7" x="3" y="3" rx="1" /> <rect width="7" height="7" x="14" y="3" rx="1" /> <rect width="7" height="7" x="14" y="14" rx="1" /> <rect width="7" height="7" x="3" y="14" rx="1" /></g>"##,
};
/// GPUI `IconName::ListFilter`.
pub static LIST_FILTER: Icon = Icon {
    name: "list-filter",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M2 5h20" /> <path d="M6 12h12" /> <path d="M9 19h6" /></g>"##,
};
/// GPUI `IconName::ListTodo`.
pub static LIST_TODO: Icon = Icon {
    name: "list-todo",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M13 5h8" /> <path d="M13 12h8" /> <path d="M13 19h8" /> <path d="m3 17 2 2 4-4" /> <rect x="3" y="4" width="6" height="6" rx="1" /></g>"##,
};
/// GPUI `IconName::Loader`.
pub static LOADER: Icon = Icon {
    name: "loader",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 2v4" /> <path d="m16.2 7.8 2.9-2.9" /> <path d="M18 12h4" /> <path d="m16.2 16.2 2.9 2.9" /> <path d="M12 18v4" /> <path d="m4.9 19.1 2.9-2.9" /> <path d="M2 12h4" /> <path d="m4.9 4.9 2.9 2.9" /></g>"##,
};
/// GPUI `IconName::LoaderCircle`.
pub static LOADER_CIRCLE: Icon = Icon {
    name: "loader-circle",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12a9 9 0 1 1-6.219-8.56" /></g>"##,
};
/// GPUI `IconName::Maximize`.
pub static MAXIMIZE: Icon = Icon {
    name: "maximize",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M8 3H5a2 2 0 0 0-2 2v3" /> <path d="M21 8V5a2 2 0 0 0-2-2h-3" /> <path d="M3 16v3a2 2 0 0 0 2 2h3" /> <path d="M16 21h3a2 2 0 0 0 2-2v-3" /></g>"##,
};
/// GPUI `IconName::MessageSquarePlus`.
pub static MESSAGE_SQUARE_PLUS: Icon = Icon {
    name: "message-square-plus",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 17a2 2 0 0 1-2 2H6.828a2 2 0 0 0-1.414.586l-2.202 2.202A.71.71 0 0 1 2 21.286V5a2 2 0 0 1 2-2h16a2 2 0 0 1 2 2z" /> <path d="M12 8v6" /> <path d="M9 11h6" /></g>"##,
};
/// GPUI `IconName::MessagesSquare`.
pub static MESSAGES_SQUARE: Icon = Icon {
    name: "messages-square",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M16 10a2 2 0 0 1-2 2H6.828a2 2 0 0 0-1.414.586l-2.202 2.202A.71.71 0 0 1 2 14.286V4a2 2 0 0 1 2-2h10a2 2 0 0 1 2 2z" /> <path d="M20 9a2 2 0 0 1 2 2v10.286a.71.71 0 0 1-1.212.502l-2.202-2.202A2 2 0 0 0 17.172 19H10a2 2 0 0 1-2-2v-1" /></g>"##,
};
/// GPUI `IconName::Mic`.
pub static MIC: Icon = Icon {
    name: "mic",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 19v3" /> <path d="M19 10v2a7 7 0 0 1-14 0v-2" /> <rect x="9" y="2" width="6" height="13" rx="3" /></g>"##,
};
/// GPUI `IconName::Minimize`.
pub static MINIMIZE: Icon = Icon {
    name: "minimize",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M8 3v3a2 2 0 0 1-2 2H3" /> <path d="M21 8h-3a2 2 0 0 1-2-2V3" /> <path d="M3 16h3a2 2 0 0 1 2 2v3" /> <path d="M16 21v-3a2 2 0 0 1 2-2h3" /></g>"##,
};
/// GPUI `IconName::Minus`.
pub static MINUS: Icon = Icon {
    name: "minus",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12h14" /></g>"##,
};
/// GPUI `IconName::Monitor`.
pub static MONITOR: Icon = Icon {
    name: "monitor",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect width="20" height="14" x="2" y="3" rx="2" /> <line x1="8" x2="16" y1="21" y2="21" /> <line x1="12" x2="12" y1="17" y2="21" /></g>"##,
};
/// GPUI `IconName::Moon`.
pub static MOON: Icon = Icon {
    name: "moon",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M20.985 12.486a9 9 0 1 1-9.473-9.472c.405-.022.617.46.402.803a6 6 0 0 0 8.268 8.268c.344-.215.825-.004.803.401" /></g>"##,
};
/// GPUI `IconName::Network`.
pub static NETWORK: Icon = Icon {
    name: "network",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="16" y="16" width="6" height="6" rx="1" /> <rect x="2" y="16" width="6" height="6" rx="1" /> <rect x="9" y="2" width="6" height="6" rx="1" /> <path d="M5 16v-3a1 1 0 0 1 1-1h12a1 1 0 0 1 1 1v3" /> <path d="M12 12V8" /></g>"##,
};
/// GPUI `IconName::Palette`.
pub static PALETTE: Icon = Icon {
    name: "palette",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 22a1 1 0 0 1 0-20 10 9 0 0 1 10 9 5 5 0 0 1-5 5h-2.25a1.75 1.75 0 0 0-1.4 2.8l.3.4a1.75 1.75 0 0 1-1.4 2.8z" /> <circle cx="13.5" cy="6.5" r=".5" fill="currentColor" /> <circle cx="17.5" cy="10.5" r=".5" fill="currentColor" /> <circle cx="6.5" cy="12.5" r=".5" fill="currentColor" /> <circle cx="8.5" cy="7.5" r=".5" fill="currentColor" /></g>"##,
};
/// GPUI `IconName::PanelBottom`.
pub static PANEL_BOTTOM: Icon = Icon {
    name: "panel-bottom",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect width="18" height="18" x="3" y="3" rx="2" /> <path d="M3 15h18" /></g>"##,
};
/// GPUI `IconName::Paperclip`.
pub static PAPERCLIP: Icon = Icon {
    name: "paperclip",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m16 6-8.414 8.586a2 2 0 0 0 2.829 2.829l8.414-8.586a4 4 0 1 0-5.657-5.657l-8.379 8.551a6 6 0 1 0 8.485 8.485l8.379-8.551" /></g>"##,
};
/// GPUI `IconName::Pause`.
pub static PAUSE: Icon = Icon {
    name: "pause",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="14" y="3" width="5" height="18" rx="1" /> <rect x="5" y="3" width="5" height="18" rx="1" /></g>"##,
};
/// GPUI `IconName::Pencil`.
pub static PENCIL: Icon = Icon {
    name: "pencil",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21.174 6.812a1 1 0 0 0-3.986-3.987L3.842 16.174a2 2 0 0 0-.5.83l-1.321 4.352a.5.5 0 0 0 .623.622l4.353-1.32a2 2 0 0 0 .83-.497z" /> <path d="m15 5 4 4" /></g>"##,
};
/// GPUI `IconName::Pin`.
pub static PIN: Icon = Icon {
    name: "pin",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 17v5" /> <path d="M9 10.76a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24V16a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1v-.76a2 2 0 0 0-1.11-1.79l-1.78-.9A2 2 0 0 1 15 10.76V7a1 1 0 0 1 1-1 2 2 0 0 0 0-4H8a2 2 0 0 0 0 4 1 1 0 0 1 1 1z" /></g>"##,
};
/// GPUI `IconName::PinOff`.
pub static PIN_OFF: Icon = Icon {
    name: "pin-off",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 17v5" /> <path d="M15 9.34V7a1 1 0 0 1 1-1 2 2 0 0 0 0-4H7.89" /> <path d="m2 2 20 20" /> <path d="M9 9v1.76a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24V16a1 1 0 0 0 1 1h11" /></g>"##,
};
/// GPUI `IconName::Play`.
pub static PLAY: Icon = Icon {
    name: "play",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M5 5a2 2 0 0 1 3.008-1.728l11.997 6.998a2 2 0 0 1 .003 3.458l-12 7A2 2 0 0 1 5 19z" /></g>"##,
};
/// GPUI `IconName::Plus`.
pub static PLUS: Icon = Icon {
    name: "plus",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12h14" /> <path d="M12 5v14" /></g>"##,
};
/// GPUI `IconName::Puzzle`.
pub static PUZZLE: Icon = Icon {
    name: "puzzle",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M15.39 4.39a1 1 0 0 0 1.68-.474 2.5 2.5 0 1 1 3.014 3.015 1 1 0 0 0-.474 1.68l1.683 1.682a2.414 2.414 0 0 1 0 3.414L19.61 15.39a1 1 0 0 1-1.68-.474 2.5 2.5 0 1 0-3.014 3.015 1 1 0 0 1 .474 1.68l-1.683 1.682a2.414 2.414 0 0 1-3.414 0L8.61 19.61a1 1 0 0 0-1.68.474 2.5 2.5 0 1 1-3.014-3.015 1 1 0 0 0 .474-1.68l-1.683-1.682a2.414 2.414 0 0 1 0-3.414L4.39 8.61a1 1 0 0 1 1.68.474 2.5 2.5 0 1 0 3.014-3.015 1 1 0 0 1-.474-1.68l1.683-1.682a2.414 2.414 0 0 1 3.414 0z" /></g>"##,
};
/// GPUI `IconName::RefreshCw`.
pub static REFRESH_CW: Icon = Icon {
    name: "refresh-cw",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8" /> <path d="M21 3v5h-5" /> <path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16" /> <path d="M8 16H3v5" /></g>"##,
};
/// GPUI `IconName::Replace`.
pub static REPLACE: Icon = Icon {
    name: "replace",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M14 4a1 1 0 0 1 1-1" /> <path d="M15 10a1 1 0 0 1-1-1" /> <path d="M21 4a1 1 0 0 0-1-1" /> <path d="M21 9a1 1 0 0 1-1 1" /> <path d="m3 7 3 3 3-3" /> <path d="M6 10V5a2 2 0 0 1 2-2h2" /> <rect x="3" y="14" width="7" height="7" rx="1" /></g>"##,
};
/// GPUI `IconName::Search`.
pub static SEARCH: Icon = Icon {
    name: "search",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m21 21-4.34-4.34" /> <circle cx="11" cy="11" r="8" /></g>"##,
};
/// GPUI `IconName::Settings`.
pub static SETTINGS: Icon = Icon {
    name: "settings",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M9.671 4.136a2.34 2.34 0 0 1 4.659 0 2.34 2.34 0 0 0 3.319 1.915 2.34 2.34 0 0 1 2.33 4.033 2.34 2.34 0 0 0 0 3.831 2.34 2.34 0 0 1-2.33 4.033 2.34 2.34 0 0 0-3.319 1.915 2.34 2.34 0 0 1-4.659 0 2.34 2.34 0 0 0-3.32-1.915 2.34 2.34 0 0 1-2.33-4.033 2.34 2.34 0 0 0 0-3.831A2.34 2.34 0 0 1 6.35 6.051a2.34 2.34 0 0 0 3.319-1.915" /> <circle cx="12" cy="12" r="3" /></g>"##,
};
/// GPUI `IconName::Settings2`.
pub static SETTINGS_2: Icon = Icon {
    name: "settings-2",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M14 17H5" /> <path d="M19 7h-9" /> <circle cx="17" cy="17" r="3" /> <circle cx="7" cy="7" r="3" /></g>"##,
};
/// GPUI `IconName::ShieldCheck`.
pub static SHIELD_CHECK: Icon = Icon {
    name: "shield-check",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z" /> <path d="m9 12 2 2 4-4" /></g>"##,
};
/// GPUI `IconName::SlidersHorizontal`.
pub static SLIDERS_HORIZONTAL: Icon = Icon {
    name: "sliders-horizontal",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M10 5H3" /> <path d="M12 19H3" /> <path d="M14 3v4" /> <path d="M16 17v4" /> <path d="M21 12h-9" /> <path d="M21 19h-5" /> <path d="M21 5h-7" /> <path d="M8 10v4" /> <path d="M8 12H3" /></g>"##,
};
/// GPUI `IconName::Sparkles`.
pub static SPARKLES: Icon = Icon {
    name: "sparkles",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M11.017 2.814a1 1 0 0 1 1.966 0l1.051 5.558a2 2 0 0 0 1.594 1.594l5.558 1.051a1 1 0 0 1 0 1.966l-5.558 1.051a2 2 0 0 0-1.594 1.594l-1.051 5.558a1 1 0 0 1-1.966 0l-1.051-5.558a2 2 0 0 0-1.594-1.594l-5.558-1.051a1 1 0 0 1 0-1.966l5.558-1.051a2 2 0 0 0 1.594-1.594z" /> <path d="M20 2v4" /> <path d="M22 4h-4" /> <circle cx="4" cy="20" r="2" /></g>"##,
};
/// GPUI `IconName::Square`.
pub static SQUARE: Icon = Icon {
    name: "square",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect width="18" height="18" x="3" y="3" rx="2" /></g>"##,
};
/// GPUI `IconName::SquarePen`.
pub static SQUARE_PEN: Icon = Icon {
    name: "square-pen",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7" /> <path d="M18.375 2.625a1 1 0 0 1 3 3l-9.013 9.014a2 2 0 0 1-.853.505l-2.873.84a.5.5 0 0 1-.62-.62l.84-2.873a2 2 0 0 1 .506-.852z" /></g>"##,
};
/// GPUI `IconName::SquareTerminal`.
pub static SQUARE_TERMINAL: Icon = Icon {
    name: "square-terminal",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m7 11 2-2-2-2" /> <path d="M11 13h4" /> <rect width="18" height="18" x="3" y="3" rx="2" ry="2" /></g>"##,
};
/// GPUI `IconName::Star`.
pub static STAR: Icon = Icon {
    name: "star",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M11.525 2.295a.53.53 0 0 1 .95 0l2.31 4.679a2.123 2.123 0 0 0 1.595 1.16l5.166.756a.53.53 0 0 1 .294.904l-3.736 3.638a2.123 2.123 0 0 0-.611 1.878l.882 5.14a.53.53 0 0 1-.771.56l-4.618-2.428a2.122 2.122 0 0 0-1.973 0L6.396 21.01a.53.53 0 0 1-.77-.56l.881-5.139a2.122 2.122 0 0 0-.611-1.879L2.16 9.795a.53.53 0 0 1 .294-.906l5.165-.755a2.122 2.122 0 0 0 1.597-1.16z" /></g>"##,
};
/// GPUI `IconName::StarFill`.
pub static STAR_FILL: Icon = Icon {
    name: "star-fill",
    view: 24.0,
    body: r##"<g fill="currentColor" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polygon points="12 2 15.09 8.26 22 9.27 17 14.14 18.18 21.02 12 17.77 5.82 21.02 7 14.14 2 9.27 8.91 8.26 12 2"/></g>"##,
};
/// GPUI `IconName::Sun`.
pub static SUN: Icon = Icon {
    name: "sun",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="4" /> <path d="M12 2v2" /> <path d="M12 20v2" /> <path d="m4.93 4.93 1.41 1.41" /> <path d="m17.66 17.66 1.41 1.41" /> <path d="M2 12h2" /> <path d="M20 12h2" /> <path d="m6.34 17.66-1.41 1.41" /> <path d="m19.07 4.93-1.41 1.41" /></g>"##,
};
/// GPUI `IconName::Target`.
pub static TARGET: Icon = Icon {
    name: "target",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10" /> <circle cx="12" cy="12" r="6" /> <circle cx="12" cy="12" r="2" /></g>"##,
};
/// GPUI `IconName::TriangleAlert`.
pub static TRIANGLE_ALERT: Icon = Icon {
    name: "triangle-alert",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3" /> <path d="M12 9v4" /> <path d="M12 17h.01" /></g>"##,
};
/// GPUI `IconName::Undo`.
pub static UNDO: Icon = Icon {
    name: "undo",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 7v6h6" /> <path d="M21 17a9 9 0 0 0-9-9 9 9 0 0 0-6 2.3L3 13" /></g>"##,
};
/// GPUI `IconName::Undo2`.
pub static UNDO_2: Icon = Icon {
    name: "undo-2",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M9 14 4 9l5-5" /> <path d="M4 9h10.5a5.5 5.5 0 0 1 5.5 5.5a5.5 5.5 0 0 1-5.5 5.5H11" /></g>"##,
};
/// GPUI `IconName::Upload`.
pub static UPLOAD: Icon = Icon {
    name: "upload",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3v12" /> <path d="m17 8-5-5-5 5" /> <path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" /></g>"##,
};
/// GPUI `IconName::User`.
pub static USER: Icon = Icon {
    name: "user",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M19 21v-2a4 4 0 0 0-4-4H9a4 4 0 0 0-4 4v2" /> <circle cx="12" cy="7" r="4" /></g>"##,
};
/// GPUI `IconName::Volume2`.
pub static VOLUME_2: Icon = Icon {
    name: "volume-2",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M11 4.702a.705.705 0 0 0-1.203-.498L6.413 7.587A1.4 1.4 0 0 1 5.416 8H3a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2.416a1.4 1.4 0 0 1 .997.413l3.383 3.384A.705.705 0 0 0 11 19.298z" /> <path d="M16 9a5 5 0 0 1 0 6" /> <path d="M19.364 18.364a9 9 0 0 0 0-12.728" /></g>"##,
};
/// GPUI `IconName::VolumeOff`.
pub static VOLUME_OFF: Icon = Icon {
    name: "volume-off",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M16 9a5 5 0 0 1 .95 2.293" /> <path d="M19.364 5.636a9 9 0 0 1 1.889 9.96" /> <path d="m2 2 20 20" /> <path d="m7 7-.587.587A1.4 1.4 0 0 1 5.416 8H3a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2.416a1.4 1.4 0 0 1 .997.413l3.383 3.384A.705.705 0 0 0 11 19.298V11" /> <path d="M9.828 4.172A.686.686 0 0 1 11 4.657v.686" /></g>"##,
};
/// GPUI `IconName::Zap`.
pub static ZAP: Icon = Icon {
    name: "zap",
    view: 24.0,
    body: r##"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M15.914 4a1.5 1.5 0 00-2.474-1.561l-9 9A1.5 1.5 0 005.5 14h4.002a.5.5 0 01.471.666L8.086 20a1.5 1.5 0 002.475 1.56l9-9A1.5 1.5 0 0018.5 10h-3.997a.5.5 0 01-.472-.667z" /></g>"##,
};
