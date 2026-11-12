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
    /// Explains why a listener reachable from other machines is risky, or `None` for loopback.
    /// Such binds stay allowed (gateway use is legitimate) but must be shown to the user.
    pub fn exposure_warning(&self) -> Option<&'static str> {
        exposure_warning(self.mode, self.bind)
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

/// Warning for a listener at `bind` that is not limited to this machine's loopback interface.
pub fn exposure_warning(mode: Mode, bind: SocketAddr) -> Option<&'static str> {
    if bind.ip().is_loopback() {
        return None;
    }
    Some(match mode {
        Mode::Dynamic => {
            "监听地址不是 127.0.0.1：同一网络中的任何人都可以不经认证使用此 SOCKS5 代理进入远端网络。仅在可信网络中使用，并配合防火墙限制来源。"
        }
        Mode::Local => {
            "监听地址不是 127.0.0.1：同一网络中的其他设备也能通过本机访问转发目标。仅在可信网络中使用。"
        }
        Mode::Remote => {
            "远端监听地址不是 127.0.0.1：服务器允许 GatewayPorts 时，远端网络中的设备可以访问本机的转发目标。"
        }
    })
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

#[cfg(test)]
mod exposure_tests {
    use super::*;

    #[test]
    fn listeners_beyond_loopback_are_flagged_for_every_mode() {
        for bind in ["127.0.0.1:1080", "127.8.0.1:1080", "[::1]:1080"] {
            for mode in [Mode::Local, Mode::Dynamic, Mode::Remote] {
                assert!(exposure_warning(mode, bind.parse().expect("bind")).is_none());
            }
        }
        for bind in ["0.0.0.0:1080", "[::]:1080", "192.168.1.5:1080"] {
            let bind: SocketAddr = bind.parse().expect("bind");
            let warnings: Vec<_> = [Mode::Local, Mode::Dynamic, Mode::Remote]
                .into_iter()
                .map(|mode| exposure_warning(mode, bind).expect("warning"))
                .collect();
            assert!(
                warnings[1].contains("SOCKS5"),
                "dynamic warns about the open proxy"
            );
            assert_ne!(warnings[0], warnings[2]);
        }
        let rule = Rule {
            id: Uuid::new_v4(),
            connection: Uuid::new_v4(),
            mode: Mode::Dynamic,
            bind: "0.0.0.0:1080".parse().expect("bind"),
            target: None,
            on_connect: false,
        };
        assert!(rule.validate().is_ok(), "exposed listeners stay allowed");
        assert!(rule.exposure_warning().is_some());
    }
}
