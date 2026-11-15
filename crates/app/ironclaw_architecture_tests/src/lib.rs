//! Workspace architecture contract tests for IronClaw Reborn.
//!
//! This crate intentionally has no production dependencies. Its tests inspect
//! workspace metadata and fail when Reborn crate dependency boundaries drift.

#![deny(clippy::let_underscore_must_use)]
