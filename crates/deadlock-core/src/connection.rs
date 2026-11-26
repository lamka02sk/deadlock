//! Saved connections: what `.deadlock/project.toml` holds, minus the password, which
//! lives in the OS keychain.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The database engines Deadlock talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    Mysql,
    Mariadb,
    Oracle,
    Redis,
}

impl Engine {
    pub const ALL: [Self; 4] = [Self::Mysql, Self::Mariadb, Self::Oracle, Self::Redis];

    pub fn label(self) -> &'static str {
        match self {
            Self::Mysql => "MySQL",
            Self::Mariadb => "MariaDB",
            Self::Oracle => "Oracle",
            Self::Redis => "Redis",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Self::Mysql | Self::Mariadb => 3306,
            Self::Oracle => 1521,
            Self::Redis => 6379,
        }
    }
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    #[default]
    Development,
    Staging,
    Production,
}

impl Environment {
    pub const ALL: [Self; 3] = [Self::Development, Self::Staging, Self::Production];

    pub fn label(self) -> &'static str {
        match self {
            Self::Development => "Development",
            Self::Staging => "Staging",
            Self::Production => "Production",
        }
    }
}

/// Transport encryption, as the design's Encryption menu.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Encryption {
    Off,
    /// TLS when the server offers it, plain otherwise.
    #[default]
    Prefer,
    /// TLS without checking the certificate.
    Require,
    /// TLS with the certificate checked against the host name.
    VerifyCertificate,
}

impl Encryption {
    pub const ALL: [Self; 4] = [
        Self::Off,
        Self::Prefer,
        Self::Require,
        Self::VerifyCertificate,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Prefer => "Prefer",
            Self::Require => "Require",
            Self::VerifyCertificate => "Verify certificate",
        }
    }
}

/// One saved connection. Never holds a password.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionConfig {
    /// Stable within the project; keys the keychain entry. Never changes after creation.
    pub id: String,
    pub name: String,
    pub engine: Engine,
    pub host: String,
    pub port: u16,
    /// MySQL and MariaDB: default schema. Oracle: service name. Redis: database index.
    // shortcut: one field for three meanings, split if an engine needs two of them.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub database: String,
    /// Oracle: `database` is a SID, not a service name.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sid: bool,
    /// Oracle: connect with the Thick driver (Oracle Instant Client, loaded at run time)
    /// instead of the built-in Thin one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub thick: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user: String,
    #[serde(default)]
    pub environment: Environment,
    /// Tint for the sidebar row and the tabs, as `#rrggbb`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default)]
    pub encryption: Encryption,
    /// Statements that change data are refused, client-side and (where the engine can)
    /// by the server.
    #[serde(default)]
    pub read_only: bool,
    /// The password is in the OS keychain.
    #[serde(default)]
    pub keychain: bool,
    /// Connect through an SSH tunnel, to `host:port` as the SSH server sees it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ssh_tunnel: bool,
    /// The SSH hop, used when `ssh_tunnel` is on.
    #[serde(default, skip_serializing_if = "ssh_unset")]
    pub ssh: crate::ssh::SshConfig,
    /// Schemas (databases) the user hid from the sidebar, search and pickers, by the
    /// exact name the tree shows. The server keeps them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden_schemas: Vec<String>,
    /// List object groups with nothing in them (Views 0…) in the sidebar.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub show_empty_groups: bool,
}

fn ssh_unset(s: &crate::ssh::SshConfig) -> bool {
    *s == crate::ssh::SshConfig::default()
}

impl ConnectionConfig {
    pub fn is_hidden(&self, schema: &str) -> bool {
        self.hidden_schemas.iter().any(|s| s == schema)
    }

    /// Hides `schema`; false when it already was.
    pub fn hide_schema(&mut self, schema: &str) -> bool {
        let new = !self.is_hidden(schema);
        if new {
            self.hidden_schemas.push(schema.to_owned());
        }
        new
    }

    /// Shows `schema` again; false when it was not hidden.
    pub fn unhide_schema(&mut self, schema: &str) -> bool {
        let before = self.hidden_schemas.len();
        self.hidden_schemas.retain(|s| s != schema);
        self.hidden_schemas.len() != before
    }

    /// A new connection for `engine` with its default port.
    pub fn new(engine: Engine) -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            engine,
            host: "localhost".into(),
            port: engine.default_port(),
            database: String::new(),
            sid: false,
            thick: false,
            user: String::new(),
            environment: Environment::default(),
            color: None,
            encryption: Encryption::default(),
            read_only: false,
            keychain: true,
            ssh_tunnel: false,
            ssh: crate::ssh::SshConfig::default(),
            hidden_schemas: Vec::new(),
            show_empty_groups: false,
        }
    }

    /// The connection address shown under the form, without the password, e.g.
    /// `mysql://readonly@replica-2.db.internal:3306/shop?ssl-mode=REQUIRED`.
    pub fn address(&self) -> String {
        let user = if self.user.is_empty() {
            String::new()
        } else {
            format!("{}@", self.user)
        };
        let path = if self.database.is_empty() {
            String::new()
        } else {
            format!("/{}", self.database)
        };
        let tls = matches!(
            self.encryption,
            Encryption::Require | Encryption::VerifyCertificate
        );
        if self.engine == Engine::Oracle {
            // Easy Connect, which has no way to name a SID.
            let scheme = if tls { "tcps://" } else { "" };
            return if self.sid {
                format!(
                    "{scheme}{user}{}:{} (SID {})",
                    self.host, self.port, self.database
                )
            } else {
                format!("{scheme}{user}{}:{}{path}", self.host, self.port)
            };
        }
        let (scheme, query) = match self.engine {
            Engine::Mysql | Engine::Mariadb => {
                let mode = match self.encryption {
                    Encryption::Off => "DISABLED",
                    Encryption::Prefer => "PREFERRED",
                    Encryption::Require => "REQUIRED",
                    Encryption::VerifyCertificate => "VERIFY_IDENTITY",
                };
                let scheme = if self.engine == Engine::Mysql {
                    "mysql"
                } else {
                    "mariadb"
                };
                (scheme, format!("?ssl-mode={mode}"))
            }
            Engine::Oracle | Engine::Redis => (if tls { "rediss" } else { "redis" }, String::new()),
        };
        format!("{scheme}://{user}{}:{}{path}{query}", self.host, self.port)
    }
}

/// A new id for `name`, unique among `existing`: the name in lower case with
/// non-alphanumerics as `_`, plus `-2`, `-3`… on collision.
pub fn new_id(name: &str, existing: &[ConnectionConfig]) -> String {
    let mut base: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if base.is_empty() {
        base = "connection".into();
    }
    let taken = |id: &str| existing.iter().any(|c| c.id == id);
    if !taken(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|id| !taken(id))
        .unwrap_or(base)
}

/// Passwords in the OS keychain, keyed by project folder and connection id.
pub mod keychain {
    use super::Path;

    const SERVICE: &str = "deadlock";

    /// The entry name of a connection's SSH password or passphrase.
    pub fn ssh_entry(id: &str) -> String {
        format!("{id}#ssh")
    }

    fn entry(project: &Path, id: &str) -> keyring::Result<keyring::Entry> {
        keyring::Entry::new(SERVICE, &format!("{}#{id}", project.display()))
    }

    /// # Errors
    /// Fails when the keychain is unavailable or refuses the write.
    pub fn save(project: &Path, id: &str, password: &str) -> keyring::Result<()> {
        entry(project, id)?.set_password(password)
    }

    /// The stored password, or `None` when there is none.
    ///
    /// # Errors
    /// Fails when the keychain is unavailable.
    pub fn load(project: &Path, id: &str) -> keyring::Result<Option<String>> {
        match entry(project, id)?.get_password() {
            Ok(p) => Ok(Some(p)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Removes the stored password. A missing entry is not an error.
    ///
    /// # Errors
    /// Fails when the keychain is unavailable.
    pub fn delete(project: &Path, id: &str) -> keyring::Result<()> {
        match entry(project, id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ConnectionConfig {
        ConnectionConfig {
            id: "analytics_replica".into(),
            name: "analytics_replica".into(),
            host: "replica-2.db.internal".into(),
            database: "shop".into(),
            user: "readonly".into(),
            environment: Environment::Production,
            encryption: Encryption::Require,
            read_only: true,
            ..ConnectionConfig::new(Engine::Mysql)
        }
    }

    #[test]
    fn address_matches_the_design() {
        assert_eq!(
            sample().address(),
            "mysql://readonly@replica-2.db.internal:3306/shop?ssl-mode=REQUIRED"
        );
    }

    #[test]
    fn address_per_engine() {
        let maria = ConnectionConfig {
            engine: Engine::Mariadb,
            encryption: Encryption::Off,
            database: String::new(),
            ..sample()
        };
        assert_eq!(
            maria.address(),
            "mariadb://readonly@replica-2.db.internal:3306?ssl-mode=DISABLED"
        );
        let oracle = ConnectionConfig {
            user: "app".into(),
            database: "FREEPDB1".into(),
            ..ConnectionConfig::new(Engine::Oracle)
        };
        assert_eq!(oracle.address(), "app@localhost:1521/FREEPDB1");
        let sid = ConnectionConfig {
            sid: true,
            database: "FREE".into(),
            encryption: Encryption::Require,
            ..oracle
        };
        assert_eq!(sid.address(), "tcps://app@localhost:1521 (SID FREE)");
        let redis = ConnectionConfig {
            database: "2".into(),
            encryption: Encryption::VerifyCertificate,
            ..ConnectionConfig::new(Engine::Redis)
        };
        assert_eq!(redis.address(), "rediss://localhost:6379/2");
    }

    #[test]
    fn ids_are_unique_slugs() {
        let mut list = vec![];
        let a = new_id("Shop prod", &list);
        assert_eq!(a, "shop_prod");
        list.push(ConnectionConfig { id: a, ..sample() });
        assert_eq!(new_id("shop prod", &list), "shop_prod-2");
        assert_eq!(new_id("  ", &list), "connection");
    }

    #[test]
    fn config_round_trips_through_toml() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct File {
            connections: Vec<ConnectionConfig>,
        }
        let file = File {
            connections: vec![
                sample(),
                ConnectionConfig {
                    id: "cache".into(),
                    name: "cache".into(),
                    ssh_tunnel: true,
                    ..ConnectionConfig::new(Engine::Redis)
                },
            ],
        };
        let text = toml::to_string(&file).unwrap();
        assert!(!text.contains("password"));
        assert!(text.contains("encryption = \"require\""));
        assert_eq!(toml::from_str::<File>(&text).unwrap(), file);
    }

    #[test]
    fn color_is_optional_in_toml() {
        let mut c = sample();
        let text = toml::to_string(&c).unwrap();
        assert!(!text.contains("color"));
        c.color = Some("#3b82f6".into());
        let text = toml::to_string(&c).unwrap();
        assert!(text.contains("color = \"#3b82f6\""));
        assert_eq!(toml::from_str::<ConnectionConfig>(&text).unwrap(), c);
        let old = text.replace("color = \"#3b82f6\"\n", "");
        assert_eq!(
            toml::from_str::<ConnectionConfig>(&old).unwrap().color,
            None
        );
    }

    #[test]
    fn hidden_schemas_hide_unhide_and_round_trip() {
        let mut c = sample();
        assert!(!toml::to_string(&c).unwrap().contains("hidden_schemas"));
        assert!(c.hide_schema("Sys"));
        assert!(!c.hide_schema("Sys"));
        assert!(c.is_hidden("Sys") && !c.is_hidden("sys"));
        let back: ConnectionConfig = toml::from_str(&toml::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
        assert!(c.unhide_schema("Sys"));
        assert!(!c.unhide_schema("Sys"));
        assert!(c.hidden_schemas.is_empty());
    }
}
