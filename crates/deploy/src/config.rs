use std::{fs::OpenOptions, path::PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tracing::log;

use crate::auth::AuthConnection;

pub use cloud_openapi::models::DeviceCodeItem;

pub const DEFAULT_FERMYON_DIRECTORY: &str = "fermyon";
pub const DEFAULT_CONNECTION_CONFIGURATION_FILE: &str = "auth.json";

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConfigError {
    #[error("cannot find file or directory {0}")]
    FileNotFound(anyhow::Error),
    #[error("IO error {0}")]
    IO(std::io::Error),
    #[error("deserialization error {0}")]
    Serde(serde_json::Error),
    #[error("core error {0}")]
    Core(anyhow::Error),
}

impl From<anyhow::Error> for ConfigError {
    fn from(err: anyhow::Error) -> Self {
        Self::Core(err)
    }
}

impl From<std::io::Error> for ConfigError {
    fn from(err: std::io::Error) -> Self {
        Self::IO(err)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    /// Root directory for all Fermyon data and configuration.
    pub auth_path: PathBuf,

    /// Authentication configuration for the connection to the platform.
    pub auth: AuthConnection,
}

impl Config {
    pub async fn new(auth_path: Option<PathBuf>) -> Result<Self, ConfigError> {
        let auth_path = match auth_path {
            Some(p) => {
                log::trace!("Using existing configuration file {:?}", p);
                p
            }
            None => {
                let root = dirs::config_dir()
                    .expect("cannot open configuration directory")
                    .join(DEFAULT_FERMYON_DIRECTORY);
                ensure(&root).await?;
                root.join(DEFAULT_CONNECTION_CONFIGURATION_FILE)
            }
        };

        let auth = match auth_path.exists() {
            true => {
                log::trace!("Using configuration file {:?}", &auth_path);
                let mut auth_file = File::open(&auth_path).await?;
                let mut contents = vec![];
                auth_file.read_to_end(&mut contents).await?;
                serde_json::from_slice(&contents).map_err(ConfigError::Serde)?
            }
            false => AuthConnection::default(),
        };

        Ok(Self { auth_path, auth })
    }

    pub async fn new_with_auth(
        auth_path: Option<PathBuf>,
        auth: AuthConnection,
    ) -> Result<Self, ConfigError> {
        let mut cfg = Self::new(auth_path).await?;
        cfg.auth = auth;

        Ok(cfg)
    }

    /// Persist a configuration change.
    pub async fn commit(&self) -> Result<(), ConfigError> {
        let f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&self.auth_path)?;

        serde_json::to_writer_pretty(f, &self.auth).map_err(ConfigError::Serde)?;
        tracing::debug!("Configuration saved to {:?}", &self.auth_path);
        Ok(())
    }
}

/// Ensure the root directory exists, or else create it.
async fn ensure(root: &PathBuf) -> Result<(), ConfigError> {
    log::trace!("Ensuring root directory {:?}", root);
    if !root.exists() {
        log::trace!("Creating configuration root directory `{}`", root.display());
        tokio::fs::create_dir_all(root)
            .await
            .map_err(ConfigError::IO)?;
    } else if !root.is_dir() {
        return Err(ConfigError::Core(anyhow::anyhow!(
            "error creating configuration directory"
        )));
    } else {
        log::trace!(
            "Using existing configuration root directory `{}`",
            root.display()
        );
    }

    Ok(())
}
