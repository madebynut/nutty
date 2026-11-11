//! Isolated Unix Agent: never inspects or modifies the user's Agent or private keys.

use super::*;
use russh::keys::agent::server::{self, Agent, MessageType};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::net::UnixListener;

#[derive(Clone, Default)]
struct Policy {
    signatures: Arc<AtomicUsize>,
    deny_signing: Arc<AtomicBool>,
}
impl Agent for Policy {
    async fn confirm_request(&self, message: MessageType) -> bool {
        if matches!(message, MessageType::Sign) {
            self.signatures.fetch_add(1, Ordering::Relaxed);
            return !self.deny_signing.load(Ordering::Relaxed);
        }
        true
    }
}

pub struct TestAgent {
    _directory: tempfile::TempDir,
    pub socket: PathBuf,
    task: tokio::task::JoinHandle<Result<(), keys::Error>>,
    policy: Policy,
}
impl TestAgent {
    pub async fn start() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("s");
        let listener = UnixListener::bind(&socket)?;
        let streams = Box::pin(futures_util::stream::unfold(listener, |listener| async {
            let stream = listener.accept().await.map(|(stream, _)| stream);
            Some((stream, listener))
        }));
        let policy = Policy::default();
        let task = tokio::spawn(server::serve(streams, policy.clone()));
        Ok(Self {
            _directory: directory,
            socket,
            task,
            policy,
        })
    }
    pub async fn add(&self, key: &keys::PrivateKey) -> Result<()> {
        let (mut agent, _) = connect(Some(&self.socket)).await?;
        agent.add_identity(key, &[]).await?;
        Ok(())
    }
    pub fn signatures(&self) -> usize {
        self.policy.signatures.load(Ordering::Relaxed)
    }
    pub fn deny_signing(&self) {
        self.policy.deny_signing.store(true, Ordering::Relaxed);
    }
}
impl Drop for TestAgent {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn public_identity_probe_reports_empty_and_loaded_agents_without_signing() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(4), async {
        let agent = TestAgent::start().await?;
        let empty = probe(Some(&agent.socket)).await?;
        assert!(empty.identities.is_empty());
        assert_eq!(empty.socket, agent.socket);
        let key = keys::PrivateKey::random(&mut rand::rng(), keys::Algorithm::Ed25519)?;
        agent.add(&key).await?;
        let info = probe(Some(&agent.socket)).await?;
        assert_eq!(info.identities.len(), 1);
        assert_eq!(
            info.identities[0].fingerprint,
            key.public_key()
                .fingerprint(keys::HashAlg::Sha256)
                .to_string()
        );
        assert_eq!(
            agent.signatures(),
            0,
            "UI discovery must not request signatures"
        );
        let error = probe(Some(&agent.socket.with_extension("absent")))
            .await
            .err()
            .context("missing socket must fail")?;
        assert!(format!("{error:#}").contains("无法连接 SSH Agent"));
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("Agent identity fixture timed out")?
}

#[tokio::test]
async fn an_unresponsive_agent_has_a_bounded_identity_timeout() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("s");
    let listener = UnixListener::bind(&socket)?;
    let (result, accepted) = tokio::join!(probe(Some(&socket)), listener.accept());
    let _stream = accepted?;
    assert!(
        format!("{:#}", result.err().context("stalled Agent must fail")?).contains("获取密钥超时")
    );
    Ok(())
}
