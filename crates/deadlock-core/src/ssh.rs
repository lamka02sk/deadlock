//! The SSH tunnel: a connection to an SSH server that forwards a local port, on
//! 127.0.0.1 and chosen by the OS, to the database's host and port as the SSH server
//! sees them. Drivers connect to the local port. The tunnel closes when its last
//! owner (the driver) is dropped.
//!
//! The server's host key is always checked: against `~/.ssh/known_hosts` (read only,
//! never written) and the app's own `known_hosts` in its data folder. An unknown key
//! is refused with its fingerprint so the user can choose to trust it ([`trust`]); a
//! key that differs from a recorded one is refused, with no way past it here.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use russh::client;
use russh::keys::known_hosts::{check_known_hosts_path, learn_known_hosts_path};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// How Deadlock signs in to the SSH server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SshAuth {
    /// A private key file, with an optional passphrase.
    #[default]
    Key,
    /// The keys the running SSH agent (`SSH_AUTH_SOCK`) holds.
    Agent,
    Password,
}

impl SshAuth {
    pub const ALL: [Self; 3] = [Self::Key, Self::Agent, Self::Password];

    pub fn label(self) -> &'static str {
        match self {
            Self::Key => "Key file",
            Self::Agent => "SSH agent",
            Self::Password => "Password",
        }
    }
}

/// The SSH hop of a connection. Never holds a password or passphrase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshConfig {
    #[serde(default)]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub auth: SshAuth,
    /// The private key file, for [`SshAuth::Key`]. `~/` is the home folder.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key_file: String,
}

fn default_port() -> u16 {
    22
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 22,
            user: String::new(),
            auth: SshAuth::default(),
            key_file: String::new(),
        }
    }
}

impl SshConfig {
    fn key_path(&self) -> PathBuf {
        match self.key_file.strip_prefix("~/") {
            Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
            None => PathBuf::from(&self.key_file),
        }
    }
}

/// A host key the user has to decide on, or that was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKey {
    pub host: String,
    pub port: u16,
    /// `SHA256:…`, as `ssh-keygen -l` shows it.
    pub fingerprint: String,
    /// The key's algorithm, e.g. `ssh-ed25519`.
    pub algorithm: String,
    key: PublicKey,
}

/// Why a tunnel could not open. The text says which hop failed.
#[derive(Debug, Clone, thiserror::Error)]
pub enum TunnelError {
    #[error(
        "SSH server {}:{} is not known: its {} key has fingerprint {}",
        .0.host, .0.port, .0.algorithm, .0.fingerprint
    )]
    UnknownHost(Box<HostKey>),
    #[error(
        "WARNING: the host key of SSH server {}:{} has CHANGED (now {} {}). Someone may be \
         intercepting the connection, or the server was reinstalled. Deadlock refuses to \
         connect. If the change is expected, remove the old key from ~/.ssh/known_hosts or \
         from {} and try again.",
        .0.host, .0.port, .0.algorithm, .0.fingerprint, app_known_hosts().display()
    )]
    ChangedHost(Box<HostKey>),
    #[error("SSH server {0}")]
    Ssh(String),
}

/// The app's own known-hosts file, in its data folder.
pub fn app_known_hosts() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_default()
        .join("deadlock")
        .join("known_hosts")
}

/// The known-hosts files checked, in order.
#[derive(Debug, Clone)]
pub struct KnownHosts {
    /// `~/.ssh/known_hosts`: read only.
    pub user: PathBuf,
    /// The app's file, where [`trust`] writes.
    pub app: PathBuf,
}

impl Default for KnownHosts {
    fn default() -> Self {
        Self {
            user: dirs::home_dir()
                .unwrap_or_default()
                .join(".ssh")
                .join("known_hosts"),
            app: app_known_hosts(),
        }
    }
}

enum Verdict {
    Known,
    Unknown,
    Changed,
}

impl KnownHosts {
    fn check(&self, host: &str, port: u16, key: &PublicKey) -> Verdict {
        let mut known = false;
        for path in [&self.user, &self.app] {
            match check_known_hosts_path(host, port, key, path) {
                Ok(true) => known = true,
                Ok(false) => {}
                Err(russh::keys::Error::KeyChanged { .. }) => return Verdict::Changed,
                // An unreadable file holds nothing we can trust.
                Err(_) => {}
            }
        }
        if known {
            Verdict::Known
        } else {
            Verdict::Unknown
        }
    }
}

/// Records `key` as trusted in the app's known-hosts file.
///
/// # Errors
/// The file could not be written.
pub fn trust(key: &HostKey, known: &KnownHosts) -> Result<(), String> {
    learn_known_hosts_path(&key.host, key.port, &key.key, &known.app)
        .map_err(|e| format!("{}: {e}", known.app.display()))
}

struct Client {
    host: String,
    port: u16,
    known: KnownHosts,
    /// What the host-key check decided, read after a failed handshake.
    refused: Arc<Mutex<Option<TunnelError>>>,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            // Host certificates need a trusted CA, which Deadlock does not configure.
            return Ok(false);
        };
        let host_key = || {
            Box::new(HostKey {
                host: self.host.clone(),
                port: self.port,
                fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
                algorithm: key.algorithm().to_string(),
                key: key.clone(),
            })
        };
        let refusal = match self.known.check(&self.host, self.port, key) {
            Verdict::Known => return Ok(true),
            Verdict::Unknown => TunnelError::UnknownHost(host_key()),
            Verdict::Changed => TunnelError::ChangedHost(host_key()),
        };
        *self.refused.lock().unwrap_or_else(PoisonError::into_inner) = Some(refusal);
        Ok(false)
    }
}

/// An open tunnel. Dropping it stops forwarding and closes the SSH connection.
pub struct Tunnel {
    pub local: SocketAddr,
    accept: JoinHandle<()>,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        // The accept loop owns the SSH session handle and the forwarded connections:
        // aborting it ends them all.
        self.accept.abort();
    }
}

/// Keepalives every 15 s; three unanswered ones end the session.
const KEEPALIVE: Duration = Duration::from_secs(15);

/// Opens the tunnel to `target_host:target_port`, as the SSH server resolves them.
/// `secret` is the password, or the key's passphrase (empty for none).
///
/// # Errors
/// The SSH server is unreachable, its host key is unknown or changed, or it refused
/// the sign-in.
pub async fn open(
    cfg: &SshConfig,
    secret: &str,
    target_host: &str,
    target_port: u16,
    known: KnownHosts,
) -> Result<Tunnel, TunnelError> {
    let at = format!("{}:{}", cfg.host, cfg.port);
    let fail = |what: &str, e: &dyn std::fmt::Display| TunnelError::Ssh(format!("{at}: {what}{e}"));
    let refused = Arc::new(Mutex::new(None));
    let handler = Client {
        host: cfg.host.clone(),
        port: cfg.port,
        known,
        refused: Arc::clone(&refused),
    };
    let config = Arc::new(client::Config {
        keepalive_interval: Some(KEEPALIVE),
        keepalive_max: 3,
        ..client::Config::default()
    });
    let connecting = client::connect(config, (cfg.host.as_str(), cfg.port), handler);
    let mut session = match tokio::time::timeout(Duration::from_secs(20), connecting).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            if let Some(r) = refused
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
            {
                return Err(r);
            }
            return Err(fail("", &e));
        }
        Err(_) => return Err(fail("", &"no answer within 20 s")),
    };
    let signed_in = match cfg.auth {
        SshAuth::Password => session
            .authenticate_password(&cfg.user, secret)
            .await
            .map_err(|e| fail("", &e))?
            .success(),
        SshAuth::Key => {
            let pass = (!secret.is_empty()).then_some(secret);
            let key = russh::keys::load_secret_key(cfg.key_path(), pass).map_err(|e| {
                TunnelError::Ssh(format!(
                    "{at}: the key file {} could not be read ({e}); a wrong or missing \
                     passphrase looks the same",
                    cfg.key_file
                ))
            })?;
            let hash = session
                .best_supported_rsa_hash()
                .await
                .map_err(|e| fail("", &e))?
                .flatten();
            session
                .authenticate_publickey(&cfg.user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await
                .map_err(|e| fail("", &e))?
                .success()
        }
        SshAuth::Agent => agent_sign_in(&mut session, &cfg.user)
            .await
            .map_err(|e| fail("", &e))?,
    };
    if !signed_in {
        return Err(fail(
            "",
            &format!(
                "refused the sign-in of user {} ({})",
                cfg.user,
                cfg.auth.label()
            ),
        ));
    }
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| fail("local port: ", &e))?;
    let local = listener
        .local_addr()
        .map_err(|e| fail("local port: ", &e))?;
    let (host, port) = (target_host.to_owned(), target_port);
    let accept = tokio::spawn(async move {
        let session = Arc::new(session);
        // Forwarded connections end with this task: aborting it drops the set.
        let mut conns = tokio::task::JoinSet::new();
        while let Ok((mut tcp, from)) = listener.accept().await {
            while conns.try_join_next().is_some() {}
            let (session, host) = (Arc::clone(&session), host.clone());
            conns.spawn(async move {
                let channel = session
                    .channel_open_direct_tcpip(
                        host,
                        u32::from(port),
                        from.ip().to_string(),
                        u32::from(from.port()),
                    )
                    .await;
                // A refused channel closes the local socket: the driver reports it.
                if let Ok(channel) = channel {
                    let mut remote = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut tcp, &mut remote).await;
                }
            });
        }
    });
    Ok(Tunnel { local, accept })
}

async fn agent_sign_in(session: &mut client::Handle<Client>, user: &str) -> Result<bool, String> {
    let mut agent = russh::keys::agent::client::AgentClient::connect_env()
        .await
        .map_err(|e| format!("no SSH agent ({e})"))?;
    let ids = agent
        .request_identities()
        .await
        .map_err(|e| e.to_string())?;
    if ids.is_empty() {
        return Err("the SSH agent holds no keys".into());
    }
    let hash = session
        .best_supported_rsa_hash()
        .await
        .map_err(|e| e.to_string())?
        .flatten();
    for id in ids {
        let key = id.public_key().into_owned();
        let ok = session
            .authenticate_publickey_with(user, key, hash, &mut agent)
            .await
            .map_err(|e| e.to_string())?
            .success();
        if ok {
            return Ok(true);
        }
    }
    Ok(false)
}

/// SSH passwords and passphrases given this session, by SSH account and key file, so
/// every session of a connection (tree, tabs) can open its tunnel without asking again.
static SECRETS: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

fn secret_key(cfg: &SshConfig) -> String {
    format!("{}@{}:{}#{}", cfg.user, cfg.host, cfg.port, cfg.key_file)
}

/// Remembers `secret` for `cfg` until the app quits.
pub fn remember(cfg: &SshConfig, secret: String) {
    SECRETS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert_default()
        .insert(secret_key(cfg), secret);
}

/// The secret remembered for `cfg`, if any.
pub fn secret(cfg: &SshConfig) -> Option<String> {
    SECRETS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .and_then(|m| m.get(&secret_key(cfg)).cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIMvLi1znQqTpe2yQhuJOWAUPwAyGzP/SQ8DfP4voTxGl";
    const KEY_B: &str = "AAAAC3NzaC1lZDI1NTE5AAAAICFru5zMP1qFOlqDaKeDD0Cl39iOXzVn4f0/XZVqC4qv";

    fn key(b64: &str) -> PublicKey {
        russh::keys::parse_public_key_base64(b64).unwrap()
    }

    #[test]
    fn host_keys_are_known_unknown_or_changed() {
        let dir = tempfile::tempdir().unwrap();
        let known = KnownHosts {
            user: dir.path().join("user_known_hosts"),
            app: dir.path().join("app").join("known_hosts"),
        };
        std::fs::write(&known.user, format!("[bastion]:2222 ssh-ed25519 {KEY_A}\n")).unwrap();
        assert!(matches!(
            known.check("bastion", 2222, &key(KEY_A)),
            Verdict::Known
        ));
        assert!(matches!(
            known.check("bastion", 2222, &key(KEY_B)),
            Verdict::Changed
        ));
        assert!(matches!(
            known.check("other", 22, &key(KEY_B)),
            Verdict::Unknown
        ));
        let hk = HostKey {
            host: "other".into(),
            port: 22,
            fingerprint: String::new(),
            algorithm: String::new(),
            key: key(KEY_B),
        };
        trust(&hk, &known).unwrap();
        assert!(matches!(
            known.check("other", 22, &key(KEY_B)),
            Verdict::Known
        ));
        // The user's own file is never written.
        assert_eq!(
            std::fs::read_to_string(&known.user).unwrap(),
            format!("[bastion]:2222 ssh-ed25519 {KEY_A}\n")
        );
        // A changed key in the app's file is refused too.
        assert!(matches!(
            known.check("other", 22, &key(KEY_A)),
            Verdict::Changed
        ));
    }

    #[test]
    fn config_defaults_and_secrets() {
        let cfg: SshConfig = toml::from_str("host = \"b\"").unwrap();
        assert_eq!((cfg.port, cfg.auth), (22, SshAuth::Key));
        assert_eq!(secret(&cfg), None);
        remember(&cfg, "pw".into());
        assert_eq!(secret(&cfg).as_deref(), Some("pw"));
    }
}
