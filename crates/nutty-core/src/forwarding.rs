//! Saved SSH forwarding rules independent of active listeners and credentials.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    Local,
    Dynamic,
    Remote,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub id: Uuid,
    pub connection: Uuid,
    pub mode: Mode,
    pub bind: SocketAddr,
    pub target: Option<(String, u16)>,
    /// Starts only when this SSH connection successfully opens or reconnects.
    pub on_connect: bool,
}
impl Rule {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.bind.port() > 0, "保存的转发规则必须指定固定端口");
        match (&self.mode, &self.target) {
            (Mode::Dynamic, None) => {}
            (Mode::Local | Mode::Remote, Some((host, port))) => {
                ensure!(
                    !host.is_empty()
                        && host.len() <= 253
                        && !host.chars().any(|c| c.is_whitespace() || c.is_control())
                        && *port > 0,
                    "转发目标无效"
                );
            }
            _ => anyhow::bail!("转发模式与目标不匹配"),
        }
        Ok(())
    }
    pub fn same_listener(&self, other: &Self) -> bool {
        self.bind == other.bind
            && if self.mode == Mode::Remote {
                other.mode == Mode::Remote && self.connection == other.connection
            } else {
                other.mode != Mode::Remote
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rules_require_fixed_ports_and_mode_appropriate_targets() {
        let mut rule = Rule {
            id: Uuid::new_v4(),
            connection: Uuid::new_v4(),
            mode: Mode::Dynamic,
            bind: "127.0.0.1:8080".parse().expect("address"),
            target: None,
            on_connect: false,
        };
        assert!(rule.validate().is_ok());
        rule.mode = Mode::Local;
        assert!(rule.validate().is_err());
        rule.target = Some(("localhost".into(), 80));
        assert!(rule.validate().is_ok());
        let mut other = rule.clone();
        other.mode = Mode::Dynamic;
        other.target = None;
        assert!(rule.same_listener(&other));
        other.mode = Mode::Remote;
        assert!(!rule.same_listener(&other));
        rule.bind.set_port(0);
        assert!(rule.validate().is_err());
    }
}
