//! Engine-neutral core of Deadlock. No UI dependencies.

pub mod cache;
pub mod classify;
pub mod complete;
pub mod config;
pub mod connection;
pub mod diagram;
pub mod driver;
pub mod edit;
pub mod export;
mod heal;
pub mod history;
pub mod import;
#[cfg(unix)]
pub mod instance;
mod mysql;
pub mod objects;
mod oracle;
pub mod plan;
pub mod project;
pub mod recent;
pub mod redis;
pub mod redis_driver;
mod redis_driver_admin;
pub mod schema;
pub mod search;
pub mod session_schema;
pub mod sessions;
pub mod settings;
pub mod split;
pub mod ssh;
pub mod structure;

use std::io;
use std::path::{Path, PathBuf};

/// Errors from reading and writing Deadlock's files.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{0:?} is not a valid connection id")]
    BadName(String),
    #[error("{} is not a folder", .0.display())]
    NotADirectory(PathBuf),
    #[error("{}: {source}", path.display())]
    Toml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("{}: {source}", path.display())]
    TomlWrite {
        path: PathBuf,
        #[source]
        source: toml::ser::Error,
    },
    #[error("{}: {source}", path.display())]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("keychain: {0}")]
    Keychain(#[from] keyring::Error),
    #[error(transparent)]
    Cache(#[from] cache::CacheError),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_owned(),
        source,
    }
}

/// Writes through a temporary file and a rename, so a crash never leaves a half-written file.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes).map_err(io_err(&tmp))?;
    std::fs::rename(&tmp, path).map_err(io_err(path))
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|source| Error::Json {
        path: path.to_owned(),
        source,
    })?;
    bytes.push(b'\n');
    write_atomic(path, &bytes)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = std::fs::read(path).map_err(io_err(path))?;
    serde_json::from_slice(&bytes).map_err(|source| Error::Json {
        path: path.to_owned(),
        source,
    })
}
