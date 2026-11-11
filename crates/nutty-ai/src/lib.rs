//! Pane-owned AI, ACP, MCP, model catalogs and Skills runtimes.
//!
//! Private application workspace crate; not published as a third-party API.

use nutty_core::{command, config, i18n, network, provider, session_environment};

pub mod acp;
pub mod ai;
pub mod assistant_session;
pub mod integrations;
pub mod provider_catalog;
