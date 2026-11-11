//! Headless terminal engine, graphics protocols, PTY lifecycle and shell tracking.
//!
//! Private application workspace crate; not published as a third-party API.

use nutty_core::{command, config, i18n, session_environment, theme};

pub mod journal;
pub mod shell_integration;
pub mod terminal;
