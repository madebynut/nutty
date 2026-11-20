//! Bounded HTTP CONNECT and SOCKS5 tunnels with protected proxy credentials.

use anyhow::{Context as _, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpStream,
};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Http,
    Socks5,
    Socks5h,
    Https,
}

impl Protocol {
    pub const ALL: [Self; 4] = [Self::Http, Self::Socks5, Self::Socks5h, Self::Https];
    pub fn scheme(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Socks5 => "socks5",
            Self::Socks5h => "socks5h",
            Self::Https => "https",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Http => "HTTP CONNECT",
            Self::Socks5 => "SOCKS5",
            Self::Socks5h => "SOCKS5 · 远程 DNS",
            Self::Https => "HTTPS · AI / MCP",
        }
    }
    pub fn url(self, host: &str, port: u16) -> Result<String> {
        ensure!(port > 0, "代理端口无效");
        let host = host.trim();
        ensure!(
            !host.is_empty()
                && !host.contains(['/', '@', '?', '#', '\\'])
                && !host.chars().any(char::is_whitespace),
            "代理主机无效"
        );
        let mut url = url::Url::parse(&format!("{}://localhost", self.scheme()))?;
        let host = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
        } else {
            host.to_string()
        };
        url.set_host(Some(&host))
            .map_err(|_| anyhow::anyhow!("代理主机无效"))?;
        url.set_port(Some(port))
            .map_err(|()| anyhow::anyhow!("代理端口无效"))?;
        Ok(url.to_string())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Proxy {
    pub id: Uuid,
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub username: String,
}

impl Proxy {
    pub fn supports_ssh(&self) -> bool {
        url::Url::parse(&self.url)
            .is_ok_and(|url| matches!(url.scheme(), "http" | "socks5" | "socks5h"))
    }
    pub fn validate(&self) -> Result<()> {
        let url = url::Url::parse(&self.url)?;
        ensure!(
            !self.name.trim().is_empty() && self.name.len() <= 128,
            "代理名称无效"
        );
        ensure!(
            matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none(),
            "代理地址无效，凭据须单独保存"
        );
        ensure!(
            self.username.len() <= 255 && !self.username.contains(['\r', '\n', '\0']),
            "代理用户名无效"
        );
        Ok(())
    }
    pub async fn password(&self) -> Result<zeroize::Zeroizing<String>> {
        if self.username.is_empty() {
            return Ok(zeroize::Zeroizing::new(String::new()));
        }
        let name = format!("proxy-{}", self.id);
        tokio::task::spawn_blocking(move || crate::config::secret(&name)).await?
    }
    pub async fn http_client(&self) -> Result<reqwest::Client> {
        Ok(self
            .client_builder()
            .await?
            .timeout(Duration::from_secs(90))
            .build()?)
    }
    /// Like [`Self::http_client`] but without a total request limit, for streamed
    /// replies that legitimately run for minutes. The caller must bound the request
    /// itself (first-byte and idle timeouts).
    pub async fn streaming_http_client(&self) -> Result<reqwest::Client> {
        Ok(self.client_builder().await?.build()?)
    }
    async fn client_builder(&self) -> Result<reqwest::ClientBuilder> {
        self.validate()?;
        let mut proxy = reqwest::Proxy::all(&self.url)?;
        if !self.username.is_empty() {
            proxy = proxy.basic_auth(&self.username, &self.password().await?);
        }
        Ok(reqwest::Client::builder()
            .proxy(proxy)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15)))
    }
}

/// Whether `url` is plain `http://` to a host other than this machine. Requests and any
/// credentials sent to it (API keys, bearer tokens) cross the network unencrypted.
/// Loopback stays quiet because local services such as Ollama normally use http.
pub fn cleartext_remote(url: &str) -> bool {
    let Ok(url) = url::Url::parse(url.trim()) else {
        return false;
    };
    url.scheme() == "http"
        && match url.host() {
            Some(url::Host::Ipv4(address)) => !address.is_loopback(),
            Some(url::Host::Ipv6(address)) => !address.is_loopback(),
            Some(url::Host::Domain(domain)) => {
                let domain = domain.trim_end_matches('.').to_ascii_lowercase();
                domain != "localhost" && !domain.ends_with(".localhost")
            }
            None => false,
        }
}

/// An IP address written as a host, with or without IPv6 brackets.
fn ip_literal(host: &str) -> Option<std::net::IpAddr> {
    host.strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host)
        .parse()
        .ok()
}
pub async fn connect(host: &str, port: u16, proxy: Option<&Proxy>) -> Result<TcpStream> {
    tokio::time::timeout(Duration::from_secs(20), async {
        let Some(proxy) = proxy else {
            let socket = TcpStream::connect((host, port)).await?;
            // Interactive SSH sends small packets; match the proxied path.
            socket.set_nodelay(true)?;
            return Ok(socket);
        };
        proxy.validate()?;
        let url = url::Url::parse(&proxy.url)?;
        ensure!(
            url.scheme() != "https",
            "SSH 的 HTTPS 代理需配置 SOCKS5 或 HTTP CONNECT 端点"
        );
        let proxy_port = url.port_or_known_default().unwrap_or(1080);
        // `host_str` keeps IPv6 brackets, which name resolution rejects.
        let mut socket = match url.host().context("代理主机为空")? {
            url::Host::Ipv4(address) => TcpStream::connect((address, proxy_port)).await?,
            url::Host::Ipv6(address) => TcpStream::connect((address, proxy_port)).await?,
            url::Host::Domain(domain) => TcpStream::connect((domain, proxy_port)).await?,
        };
        let password = proxy.password().await?;
        if url.scheme() == "http" {
            let authority = if host.contains(':') && !host.starts_with('[') {
                format!("[{host}]:{port}")
            } else {
                format!("{host}:{port}")
            };
            ensure!(!authority.contains(['\r', '\n', '\0']), "目标主机无效");
            let credentials =
                zeroize::Zeroizing::new(format!("{}:{}", proxy.username, password.as_str()));
            let auth = zeroize::Zeroizing::new(if proxy.username.is_empty() {
                String::new()
            } else {
                format!(
                    "Proxy-Authorization: Basic {}\r\n",
                    STANDARD.encode(credentials.as_bytes())
                )
            });
            let request = zeroize::Zeroizing::new(format!(
                "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n{}\r\n",
                auth.as_str()
            ));
            socket.write_all(request.as_bytes()).await?;
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                ensure!(header.len() < 8192, "代理响应头过长");
                header.push(socket.read_u8().await?);
            }
            let first = std::str::from_utf8(&header)?
                .lines()
                .next()
                .context("代理响应为空")?;
            ensure!(
                first.split_whitespace().nth(1) == Some("200"),
                "代理拒绝 CONNECT 请求"
            );
        } else {
            socket
                .write_all(if proxy.username.is_empty() {
                    &[5, 1, 0]
                } else {
                    &[5, 2, 0, 2]
                })
                .await?;
            ensure!(socket.read_u8().await? == 5, "SOCKS 版本错误");
            match socket.read_u8().await? {
                0 => {}
                2 if !proxy.username.is_empty() => {
                    ensure!(password.len() <= 255, "SOCKS 密码过长");
                    let mut auth = zeroize::Zeroizing::new(vec![1, proxy.username.len() as u8]);
                    auth.extend_from_slice(proxy.username.as_bytes());
                    auth.push(password.len() as u8);
                    auth.extend_from_slice(password.as_bytes());
                    socket.write_all(&auth).await?;

                    ensure!(
                        socket.read_u8().await? == 1 && socket.read_u8().await? == 0,
                        "SOCKS 认证失败"
                    );
                }
                _ => bail!("SOCKS 代理不支持认证方式"),
            }
            ensure!(
                !host.is_empty() && host.len() <= 255 && !host.contains(['\0', '\r', '\n']),
                "SOCKS 目标无效"
            );
            // Only `socks5h` delegates name resolution to the proxy.
            let address = match ip_literal(host) {
                Some(address) => Some(address),
                None if url.scheme() == "socks5" => Some(
                    tokio::net::lookup_host((host, port))
                        .await?
                        .next()
                        .context("无法在本机解析 SOCKS 目标主机；如需由代理解析，请改用“SOCKS5 · 远程 DNS”")?
                        .ip(),
                ),
                None => None,
            };
            let mut request = vec![5, 1, 0];
            match address {
                Some(std::net::IpAddr::V4(address)) => {
                    request.push(1);
                    request.extend_from_slice(&address.octets());
                }
                Some(std::net::IpAddr::V6(address)) => {
                    request.push(4);
                    request.extend_from_slice(&address.octets());
                }
                None => {
                    request.extend_from_slice(&[3, host.len() as u8]);
                    request.extend_from_slice(host.as_bytes());
                }
            }
            request.extend_from_slice(&port.to_be_bytes());
            socket.write_all(&request).await?;
            ensure!(
                socket.read_u8().await? == 5
                    && socket.read_u8().await? == 0
                    && socket.read_u8().await? == 0,
                "SOCKS 连接失败"
            );
            let length = match socket.read_u8().await? {
                1 => 4,
                4 => 16,
                3 => usize::from(socket.read_u8().await?),
                _ => bail!("SOCKS 地址类型错误"),
            };
            let mut bound = vec![0; length + 2];
            socket.read_exact(&mut bound).await?;
        }
        socket.set_nodelay(true)?;
        Ok::<_, anyhow::Error>(socket)
    })
    .await
    .context("代理或目标连接超时")?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn proxy_fields_build_valid_ipv4_ipv6_and_dns_urls_and_reject_injection() -> Result<()> {
        for protocol in Protocol::ALL {
            for host in ["127.0.0.1", "localhost", "::1", "[::1]"] {
                let proxy = Proxy {
                    id: Uuid::new_v4(),
                    name: "fixture".into(),
                    url: protocol.url(host, 1080)?,
                    username: String::new(),
                };
                proxy.validate()?;
                assert_eq!(url::Url::parse(&proxy.url)?.port(), Some(1080));
                assert_eq!(proxy.supports_ssh(), protocol != Protocol::Https);
            }
        }
        for host in [
            "",
            "a/b",
            "user@host",
            "host?arg",
            "a#b",
            "host\r\nX: injected",
            "host:1080",
        ] {
            assert!(
                Protocol::Http.url(host, 8080).is_err(),
                "invalid host: {host:?}"
            );
        }
        assert!(Protocol::Socks5h.url("localhost", 0).is_err());
        Ok(())
    }
    #[tokio::test]
    async fn connect_and_socks_preserve_tunnel_payload_and_use_remote_dns() -> Result<()> {
        tokio::time::timeout(Duration::from_secs(4), async {
            for socks in [false, true] {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
                let address = listener.local_addr()?;
                let serving = tokio::spawn(async move {
                    let (mut socket, _) = listener.accept().await?;
                    if socks {
                        let mut hello = [0; 3];
                        socket.read_exact(&mut hello).await?;
                        assert_eq!(hello, [5, 1, 0]);
                        socket.write_all(&[5, 0]).await?;
                        let mut header = [0; 5];
                        socket.read_exact(&mut header).await?;
                        assert_eq!(&header[..4], &[5, 1, 0, 3]);
                        let mut host = vec![0; header[4] as usize];
                        socket.read_exact(&mut host).await?;
                        assert_eq!(host, b"target.example");
                        assert_eq!(socket.read_u16().await?, 2222);
                        socket.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0]).await?;
                    } else {
                        let mut header = Vec::new();
                        while !header.ends_with(b"\r\n\r\n") {
                            header.push(socket.read_u8().await?);
                        }
                        assert!(
                            std::str::from_utf8(&header)?
                                .starts_with("CONNECT target.example:2222 HTTP/1.1")
                        );
                        socket.write_all(b"HTTP/1.1 200 Connected\r\n\r\n").await?;
                    }
                    let mut data = [0; 5];
                    socket.read_exact(&mut data).await?;
                    assert_eq!(&data, b"hello");
                    socket.write_all(b"world").await?;
                    Ok::<_, anyhow::Error>(())
                });
                let proxy = Proxy {
                    id: Uuid::new_v4(),
                    name: "fixture".into(),
                    url: format!("{}://{address}", if socks { "socks5h" } else { "http" }),
                    username: String::new(),
                };
                let mut stream = connect("target.example", 2222, Some(&proxy)).await?;
                stream.write_all(b"hello").await?;
                let mut data = [0; 5];
                stream.read_exact(&mut data).await?;
                assert_eq!(&data, b"world");
                serving.await??;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("proxy workflow timed out")?
    }
}

#[cfg(test)]
mod target_address_tests {
    use super::*;
    /// Accepts one SOCKS5 CONNECT and returns the address type and address bytes sent.
    async fn socks_request(scheme: &str, host: &str) -> Result<(u8, Vec<u8>)> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let proxy = Proxy {
            id: Uuid::new_v4(),
            name: "fixture".into(),
            url: format!("{scheme}://{}", listener.local_addr()?),
            username: String::new(),
        };
        let serving = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await?;
            let mut hello = [0; 3];
            socket.read_exact(&mut hello).await?;
            socket.write_all(&[5, 0]).await?;
            let mut header = [0; 4];
            socket.read_exact(&mut header).await?;
            let length = match header[3] {
                1 => 4,
                4 => 16,
                3 => usize::from(socket.read_u8().await?),
                other => bail!("unexpected address type {other}"),
            };
            let mut address = vec![0; length];
            socket.read_exact(&mut address).await?;
            assert_eq!(socket.read_u16().await?, 22);
            socket.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0]).await?;
            Ok::<_, anyhow::Error>((header[3], address))
        });
        tokio::time::timeout(Duration::from_secs(5), connect(host, 22, Some(&proxy))).await??;
        serving.await?
    }
    #[tokio::test]
    async fn socks5_resolves_locally_and_ip_literals_are_sent_as_addresses() -> Result<()> {
        let (kind, address) = socks_request("socks5", "localhost").await?;
        let resolved = match kind {
            1 => std::net::IpAddr::from(<[u8; 4]>::try_from(address.as_slice())?),
            4 => std::net::IpAddr::from(<[u8; 16]>::try_from(address.as_slice())?),
            other => bail!("socks5 sent address type {other} instead of a resolved address"),
        };
        assert!(resolved.is_loopback());
        assert_eq!(
            socks_request("socks5h", "localhost").await?,
            (3, b"localhost".to_vec())
        );
        for scheme in ["socks5", "socks5h"] {
            assert_eq!(
                socks_request(scheme, "192.0.2.7").await?,
                (1, vec![192, 0, 2, 7])
            );
            for host in ["2001:db8::1", "[2001:db8::1]"] {
                let expected: std::net::Ipv6Addr = "2001:db8::1".parse()?;
                assert_eq!(
                    socks_request(scheme, host).await?,
                    (4, expected.octets().to_vec())
                );
            }
        }
        Ok(())
    }
    #[tokio::test]
    async fn http_connect_brackets_ipv6_targets_once() -> Result<()> {
        for host in ["::1", "[::1]"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let proxy = Proxy {
                id: Uuid::new_v4(),
                name: "fixture".into(),
                url: format!("http://{}", listener.local_addr()?),
                username: String::new(),
            };
            let serving = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await?;
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    header.push(socket.read_u8().await?);
                }
                socket.write_all(b"HTTP/1.1 200 Connected\r\n\r\n").await?;
                Ok::<_, anyhow::Error>(String::from_utf8(header)?)
            });
            tokio::time::timeout(Duration::from_secs(5), connect(host, 22, Some(&proxy))).await??;
            assert!(serving.await??.starts_with("CONNECT [::1]:22 HTTP/1.1\r\n"));
        }
        Ok(())
    }
    #[tokio::test]
    async fn direct_connections_disable_nagle() -> Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let stream = connect("127.0.0.1", port, None).await?;
        assert!(stream.nodelay()?);
        Ok(())
    }
}
#[cfg(test)]
mod transport_security_tests {
    use super::*;

    #[test]
    fn cleartext_warning_covers_remote_http_only() {
        for url in [
            "http://api.example.com/v1",
            "http://192.168.1.10:11434",
            "HTTP://Example.COM",
            "http://[2001:db8::1]/mcp",
        ] {
            assert!(cleartext_remote(url), "{url}");
        }
        for url in [
            "https://api.example.com",
            "http://localhost:11434",
            "http://LOCALHOST.:11434",
            "http://127.0.0.1:8080",
            "http://[::1]:11434",
            "http://ollama.localhost",
            "not a url",
            "",
        ] {
            assert!(!cleartext_remote(url), "{url}");
        }
    }

    #[tokio::test]
    async fn proxies_at_ipv6_literals_are_reachable() -> Result<()> {
        // Hosts without IPv6 loopback cannot run this check.
        let Ok(listener) = tokio::net::TcpListener::bind("[::1]:0").await else {
            return Ok(());
        };
        let port = listener.local_addr()?.port();
        let serving = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await?;
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                header.push(socket.read_u8().await?);
            }
            socket.write_all(b"HTTP/1.1 200 Connected\r\n\r\n").await?;
            Ok::<_, anyhow::Error>(())
        });
        let proxy = Proxy {
            id: Uuid::new_v4(),
            name: "ipv6".into(),
            url: Protocol::Http.url("::1", port)?,
            username: String::new(),
        };
        tokio::time::timeout(
            Duration::from_secs(5),
            connect("target.example", 22, Some(&proxy)),
        )
        .await??;
        serving.await??;
        Ok(())
    }
}
