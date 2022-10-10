use std::path::{Path, PathBuf};

use bindle::Id;
use semver::BuildMetadata;
use spin_publish::BindleConnectionInfo;
use thiserror::Error;
use tracing::log;

use crate::auth::{AuthConnection, AuthError};
use crate::clients::cloud::Client;

const REGISTRY_URL_PATH: &str = "api/registry";

#[derive(Debug, Error)]
pub enum DeploymentError {
    #[error("registry error: {0}")]
    RegistryError(anyhow::Error),
    #[error("application error: {0}")]
    ApplicationError(anyhow::Error),
    #[error("loader error: {0}")]
    LoaderError(anyhow::Error),
    #[error("IO error: {0}")]
    IO(std::io::Error),
    #[error("credentials error: {0}")]
    CredentialsError(AuthError),
    #[error("deployment error: {0}")]
    DeploymentError(anyhow::Error),
}

#[derive(Clone, Debug)]
pub struct ApplicationInfo {
    pub name: String,
    pub version: String,
    pub url: String,
}

pub struct DeploymentClient {
    pub auth: AuthConnection,
}

impl DeploymentClient {
    pub async fn deploy(
        &self,
        app: &Path,
        staging_dir: Option<PathBuf>,
        buildinfo: Option<BuildMetadata>,
        redeploy: bool,
    ) -> Result<ApplicationInfo, DeploymentError> {
        match self.auth {
            AuthConnection::StandaloneRegistry(_, _) => todo!(),
            AuthConnection::ProxiedRegistry(_) => {
                let p = ProxiedRegistryDeploymentProvider {
                    auth: self.auth.clone(),
                };
                let id = p
                    .push_to_registry(app, staging_dir, buildinfo, redeploy)
                    .await?;

                p.create_or_update_application(app, &id, redeploy).await
            }
        }
    }
}

#[derive(Debug)]
pub struct ProxiedRegistryDeploymentProvider {
    pub auth: AuthConnection,
}

impl ProxiedRegistryDeploymentProvider {
    pub async fn push_to_registry(
        &self,
        app: &Path,
        staging_dir: Option<PathBuf>,
        buildinfo: Option<BuildMetadata>,
        redeploy: bool,
    ) -> Result<Id, DeploymentError> {
        let registry_url = format!(
            "{}/{}",
            &self.auth.platform_connection().url,
            REGISTRY_URL_PATH
        );
        log::trace!("Publishing to registry at {}", registry_url);
        let registry_connection = BindleConnectionInfo::from_token(
            registry_url,
            false,
            self.auth.platform_connection().token_info.token,
        );

        crate::clients::create_and_push_bindle(
            app,
            staging_dir,
            buildinfo,
            redeploy,
            registry_connection,
        )
        .await
    }

    // TODO: in the future, the two functions should be eqvuielent, regardless
    // of whether the registry is proxied or not.
    //
    // For now, we keep them separate to ensure they are working as expected.
    async fn create_or_update_application(
        &self,
        _path: &Path,
        id: &Id,
        _redeploy: bool,
    ) -> Result<ApplicationInfo, DeploymentError> {
        let client = Client::new(self.auth.platform_connection());
        let channel = client
            .create_or_update_app(id.name().to_string(), id.version_string())
            .await?;
        Ok(ApplicationInfo {
            name: id.name().to_string(),
            version: id.version_string(),
            url: channel.domain,
        })
    }
}

#[derive(Debug)]
pub struct StandaloneRegistryDeploymentProvider {
    pub auth: AuthConnection,
}

impl StandaloneRegistryDeploymentProvider {
    pub async fn push_to_registry(
        &self,
        app: &Path,
        staging_dir: Option<PathBuf>,
        buildinfo: Option<BuildMetadata>,
        redeploy: bool,
    ) -> Result<Id, DeploymentError> {
        let registry_connection = match &self.auth {
            AuthConnection::StandaloneRegistry(_, bc) => bc,
            AuthConnection::ProxiedRegistry(_) => {
                log::debug!("Attempting to use a proxied registry connection for a standalone registry deployment");
                return Err(DeploymentError::CredentialsError(
                    AuthError::InvalidCredentials,
                ));
            }
        };

        log::trace!("Publishing to registry at {}", registry_connection.url);
        let registry_connection = BindleConnectionInfo::new(
            &registry_connection.url,
            registry_connection.insecure,
            registry_connection.username.clone(),
            registry_connection.password.clone(),
        );

        crate::clients::create_and_push_bindle(
            app,
            staging_dir,
            buildinfo,
            redeploy,
            registry_connection,
        )
        .await
    }
}
