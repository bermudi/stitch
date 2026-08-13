//! End-to-end tests for the `stitch` CLI binary (modular).
//!
//! This crate root re-exports the split test modules under `tests/cli/`.
//! Each sub-module covers a command or concern (apply, add, config, hooks,
//! plans, security, etc.) with shared fixtures in `support.rs`.
//!
//! The original monolithic `tests/cli.rs` (~13k lines, 375 tests) was split
//! for navigation and ownership. Behavior is identical — `cargo test --test cli`
//! still runs all 375 tests.

#[path = "cli/support.rs"]
mod support;

#[path = "cli/add.rs"]
mod add;

#[path = "cli/apply.rs"]
mod apply;

#[path = "cli/config.rs"]
mod config;

#[path = "cli/hooks.rs"]
mod hooks;

#[path = "cli/init.rs"]
mod init;

#[path = "cli/inspect.rs"]
mod inspect;

#[path = "cli/plans.rs"]
mod plans;

#[path = "cli/remove.rs"]
mod remove;

#[path = "cli/routing.rs"]
mod routing;

#[path = "cli/security.rs"]
mod security;

#[path = "cli/template.rs"]
mod template;
