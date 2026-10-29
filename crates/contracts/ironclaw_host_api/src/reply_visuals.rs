//! How a reply carries visuals the desktop app draws natively: a fenced
//! `visual` block holding a JSON chart, table or stats spec. The text teaches
//! agents the format; both the agent loop and the CLI providers append it to
//! their system prompts. The app's parser (`tools/ironclaw_gpui/client/src/
//! visual.rs`) is the other half: change both together.

pub const REPLY_VISUALS_PROMPT: &str = include_str!("../prompts/visuals.md");
