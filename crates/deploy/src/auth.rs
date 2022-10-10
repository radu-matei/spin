use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::log;
use uuid::Uuid;

use crate::clients::cloud::Client;

pub use cloud_openapi::models::DeviceCodeItem;

/// Determines whether to login to a server that supports a device code flow,
/// or to supply a username and password pair.
#[derive(Clone, Debug)]
pub enum AuthMethod {
    DeviceCode,
    UsernameAndPassword,
}

/// Authentication error returned by the server.
#[derive(Debug, Error)]
pub enum AuthError {
    #[error("invalid credentials")]
    InvalidCredentials,
    #[error("waiting for device authorization")]
    WaitingAuthorization,
    #[error("device code not authorized: {0}")]
    DeviceCodeNotAuthorized(String),
    #[error("timed out waiting for authorization")]
    Timeout,
    #[error("cannot parse timestamp {0}")]
    TimeError(chrono::ParseError),
    #[error("authentication error")]
    Core(anyhow::Error),
}

impl From<anyhow::Error> for AuthError {
    fn from(err: anyhow::Error) -> Self {
        Self::Core(err)
    }
}

impl From<uuid::Error> for AuthError {
    fn from(err: uuid::Error) -> Self {
        Self::Core(err.into())
    }
}

/// Token information returned by the server when attempting to authenticate.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TokenInfo {
    pub token: String,
    pub expiration: String,
}

impl From<cloud_openapi::models::TokenInfo> for TokenInfo {
    fn from(t: cloud_openapi::models::TokenInfo) -> Self {
        TokenInfo {
            token: t.token.unwrap_or_default(),
            expiration: t.expiration.unwrap_or_default(),
        }
    }
}

impl From<hippo_openapi::models::TokenInfo> for TokenInfo {
    fn from(t: hippo_openapi::models::TokenInfo) -> Self {
        TokenInfo {
            token: t.token.unwrap_or_default(),
            expiration: t.expiration.unwrap_or_default(),
        }
    }
}

/// Credentials for interacting with a Bindle registry.
/// Note that an instance of the Fermyon Platform can have a built-in registry,
/// in which case a separate credential set for Bindle is no longer needed.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BindleConnection {
    pub url: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub username: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub password: Option<String>,

    pub insecure: bool,
}

/// Credentials for interacting with an instance of the Fermyon Platform.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PlatformConnection {
    /// URL of the Fermyon Platform instance.
    pub url: String,
    /// Token information for communicating with the instance.
    pub token_info: TokenInfo,

    pub insecure: bool,
}

impl PlatformConnection {
    pub fn is_token_valid(&self) -> Result<bool, AuthError> {
        let expiration_date = DateTime::parse_from_rfc3339(&self.token_info.expiration)
            .map_err(AuthError::TimeError)?;
        let now = Utc::now();
        if now > expiration_date {
            Ok(false)
        } else {
            Ok(true)
        }
    }
}

/// Credentials for deploying a Spin application.
/// Distributing an application can be done either by pushing to a standalone
/// Bindle registry, or the Fermyon Platform instance has a built-in registry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AuthConnection {
    StandaloneRegistry(PlatformConnection, BindleConnection),
    ProxiedRegistry(PlatformConnection),
}

impl Default for AuthConnection {
    fn default() -> Self {
        Self::ProxiedRegistry(PlatformConnection::default())
    }
}

impl AuthConnection {
    pub fn is_token_valid(&self) -> Result<bool, AuthError> {
        self.platform_connection().is_token_valid()
    }

    pub fn platform_connection(&self) -> PlatformConnection {
        match self {
            Self::StandaloneRegistry(p, _) => p.clone(),
            Self::ProxiedRegistry(p) => p.clone(),
        }
    }
}

pub struct DeviceFlowAuthenticator {
    /// Client ID defined by the server.
    client_id: String,
    /// Client for performing the device flow authentication.
    client: Client,
}

impl DeviceFlowAuthenticator {
    /// Create a new instance of the device flow authenticator.
    pub fn new(url: String, insecure: bool, client_id: String) -> Self {
        log::trace!("Creating device flow authenticator for {}", url);
        let client = Client::new(PlatformConnection {
            url,
            insecure,
            ..Default::default()
        });
        Self { client_id, client }
    }

    /// Get a device code.
    pub async fn get_device_code(&self) -> Result<DeviceCodeItem, AuthError> {
        log::trace!("Getting device code");
        Ok(self
            .client
            .create_device_code(Uuid::parse_str(&self.client_id)?)
            .await?)
    }

    /// Check whether a device code has been authenticated.
    pub async fn check_device_code(&self, code: String) -> Result<TokenInfo, AuthError> {
        log::trace!("Checking device code");
        match self.client.login(code).await {
            Ok(token_info) => match token_info.token {
                Some(_) => Ok(token_info.into()),
                None => Err(AuthError::DeviceCodeNotAuthorized(
                    "unauthorized".to_string(),
                )),
            },
            Err(err) => Err(AuthError::DeviceCodeNotAuthorized(err.to_string())),
        }
    }
}
