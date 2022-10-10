use anyhow::{anyhow, Context, Result};
use cloud_openapi::{
    apis::{
        apps_api::{api_apps_get, api_apps_post},
        channels_api::{api_channels_get, api_channels_id_get, api_channels_post},
        configuration::{ApiKey, Configuration},
        device_codes_api::api_device_codes_post,
        revisions_api::{api_revisions_get, api_revisions_post},
        Error,
    },
    models::{
        AppItemPage, ChannelItem, ChannelItemPage, ChannelRevisionSelectionStrategy,
        CreateAppCommand, CreateChannelCommand, CreateDeviceCodeCommand, DeviceCodeItem,
        RegisterRevisionCommand, RevisionItemPage, TokenInfo,
    },
};
use reqwest::header;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use tracing::log;
use uuid::Uuid;

use crate::{auth::PlatformConnection, deploy::DeploymentError};

const JSON_MIME_TYPE: &str = "application/json";

pub struct Client {
    configuration: Configuration,
}

impl Client {
    pub fn new(conn_info: PlatformConnection) -> Self {
        let mut headers = header::HeaderMap::new();
        headers.insert(header::ACCEPT, JSON_MIME_TYPE.parse().unwrap());
        headers.insert(header::CONTENT_TYPE, JSON_MIME_TYPE.parse().unwrap());

        let base_path = match conn_info.url.strip_suffix('/') {
            Some(s) => s.to_owned(),
            None => conn_info.url,
        };

        let configuration = Configuration {
            base_path,
            user_agent: Some(format!(
                "{}/{}",
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION")
            )),
            client: reqwest::Client::builder()
                .danger_accept_invalid_certs(conn_info.insecure)
                .default_headers(headers)
                .build()
                .unwrap(),
            basic_auth: None,
            oauth_access_token: None,
            bearer_access_token: None,
            api_key: Some(ApiKey {
                prefix: Some("Bearer".to_owned()),
                key: conn_info.token_info.token,
            }),
        };

        Self { configuration }
    }

    pub async fn create_or_update_app(
        &self,
        name: String,
        version: String,
    ) -> Result<ChannelItem, DeploymentError> {
        match self.get_app(&name).await {
            Ok(app_id) => {
                log::trace!("App {} already exists, updating", name);
                self.add_revision(name.clone(), version.clone())
                    .await
                    .map_err(DeploymentError::DeploymentError)?;
                self.update_app(app_id, name.clone(), version.clone())
                    .await
                    .map_err(DeploymentError::DeploymentError)
            }
            Err(_) => {
                log::trace!("App {} does not exist, creating", name);
                let range_rule = Some(version.to_string());
                let app_id = self
                    .add_app(&name, &name)
                    .await
                    .context("Unable to create app")
                    .map_err(DeploymentError::DeploymentError)?;
                self.add_channel(
                    app_id,
                    // This is using the app name as the channel name.
                    name.to_string(),
                    ChannelRevisionSelectionStrategy::UseRangeRule,
                    range_rule,
                    None,
                )
                .await
                .context("Problem creating a channel")
                .map_err(DeploymentError::DeploymentError)
            }
        }
    }

    pub(crate) async fn update_app(
        &self,
        app_id: Uuid,
        name: String,
        version: String,
    ) -> Result<ChannelItem> {
        let channels = self.list_channels().await?;
        let channel = channels
            .items
            .iter()
            .find(|&x| x.app_id == app_id && x.name == name.clone())
            .ok_or_else(|| DeploymentError::DeploymentError(anyhow!("Channel not found")))?;

        let revision_id = self.get_revision_id(app_id, version).await?;
        let body = json!({
                "channelId": channel.id,
                "revisionSelectionStrategy": "UseSpecifiedRevision",
                "activeRevisionId": revision_id,
        })
        .to_string();

        api_channels_id_patch_fixed(&self.configuration, &channel.id.to_string(), body)
            .await
            .context("cannot patch channel")?;

        Ok(channel.clone())
    }

    pub(crate) async fn get_revision_id(&self, app: Uuid, version: String) -> Result<Uuid> {
        let revisions = api_revisions_get(&self.configuration, None, None).await?;
        let revision = revisions
            .items
            .iter()
            .find(|&x| x.revision_number == version && x.app_id == app);

        Ok(revision
            .ok_or_else(|| {
                anyhow::anyhow!("No revision with version {} and app id {}", version, app)
            })?
            .id)
    }

    pub(crate) async fn get_app(&self, name: &str) -> Result<Uuid> {
        match self
            .list_apps()
            .await?
            .items
            .iter()
            .find(|a| a.name == name)
        {
            Some(app) => Ok(app.id),
            None => Err(anyhow::anyhow!("Application {} not found", name)),
        }
    }

    pub async fn create_device_code(&self, client_id: Uuid) -> Result<DeviceCodeItem> {
        api_device_codes_post(
            &self.configuration,
            Some(CreateDeviceCodeCommand { client_id }),
        )
        .await
        .map_err(format_response_error)
    }

    pub async fn login(&self, token: String) -> Result<TokenInfo> {
        // When the new OpenAPI specification is released, manually crafting
        // the request should no longer be necessary.
        let response = self
            .configuration
            .client
            .post(format!("{}/api/auth-tokens", self.configuration.base_path))
            .body(
                serde_json::json!(
                    {
                        "provider": "DeviceFlow",
                        "clientId": "583e63e9-461f-4fbe-a246-23e0fb1cad10",
                        "providerCode": token,
                    }
                )
                .to_string(),
            )
            .send()
            .await?;

        serde_json::from_reader(response.bytes().await?.as_ref())
            .context("Failed to parse response")
    }

    pub async fn add_app(&self, name: &str, storage_id: &str) -> Result<Uuid> {
        api_apps_post(
            &self.configuration,
            Some(CreateAppCommand {
                name: name.to_string(),
                storage_id: storage_id.to_string(),
            }),
        )
        .await
        .map_err(format_response_error)
    }

    pub async fn list_apps(&self) -> Result<AppItemPage> {
        api_apps_get(&self.configuration, None, None, None, None, None)
            .await
            .map_err(format_response_error)
    }

    pub async fn get_channel_by_id(&self, id: &str) -> Result<ChannelItem> {
        api_channels_id_get(&self.configuration, id)
            .await
            .map_err(format_response_error)
    }

    pub async fn list_channels(&self) -> Result<ChannelItemPage> {
        api_channels_get(
            &self.configuration,
            Some(""),
            None,
            None,
            Some("Name"),
            None,
        )
        .await
        .map_err(format_response_error)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn add_channel(
        &self,
        app_id: Uuid,
        name: String,
        revision_selection_strategy: ChannelRevisionSelectionStrategy,
        range_rule: Option<String>,
        active_revision_id: Option<Uuid>,
    ) -> anyhow::Result<ChannelItem> {
        let command = CreateChannelCommand {
            app_id,
            name,
            revision_selection_strategy,
            range_rule,
            active_revision_id,
        };
        let id = api_channels_post(&self.configuration, Some(command))
            .await
            .map_err(format_response_error)?;

        self.get_channel_by_id(&id.to_string()).await
    }

    pub async fn add_revision(
        &self,
        app_storage_id: String,
        revision_number: String,
    ) -> anyhow::Result<()> {
        api_revisions_post(
            &self.configuration,
            Some(RegisterRevisionCommand {
                app_storage_id,
                revision_number,
            }),
        )
        .await
        .map_err(format_response_error)
    }

    pub async fn list_revisions(&self) -> anyhow::Result<RevisionItemPage> {
        api_revisions_get(&self.configuration, None, None)
            .await
            .map_err(format_response_error)
    }
}

#[derive(Deserialize, Debug)]
struct ValidationExceptionMessage {
    title: String,
    errors: HashMap<String, Vec<String>>,
}

fn format_response_error<T>(e: Error<T>) -> anyhow::Error {
    match e {
        Error::ResponseError(r) => {
            match serde_json::from_str::<ValidationExceptionMessage>(&r.content) {
                Ok(m) => anyhow::anyhow!("{} {:?}", m.title, m.errors),
                _ => anyhow::anyhow!(r.content),
            }
        }
        Error::Serde(err) => {
            anyhow::anyhow!(format!("could not parse JSON object: {}", err))
        }
        _ => anyhow::anyhow!(e.to_string()),
    }
}

// TODO: Currently, the generated OpenAPI specification is incorrect.
// Once it is updated, remove this patch.
pub async fn api_channels_id_patch_fixed(
    configuration: &Configuration,
    id: &str,
    body: String,
) -> Result<(), Error<cloud_openapi::apis::channels_api::ApiChannelsIdPatchError>> {
    let local_var_configuration = configuration;

    let local_var_client = &local_var_configuration.client;

    let local_var_uri_str = format!(
        "{}/api/channels/{id}",
        local_var_configuration.base_path,
        id = cloud_openapi::apis::urlencode(id)
    );
    let mut local_var_req_builder =
        local_var_client.request(reqwest::Method::PATCH, local_var_uri_str.as_str());

    if let Some(ref local_var_user_agent) = local_var_configuration.user_agent {
        local_var_req_builder =
            local_var_req_builder.header(reqwest::header::USER_AGENT, local_var_user_agent.clone());
    }
    if let Some(ref local_var_apikey) = local_var_configuration.api_key {
        let local_var_key = local_var_apikey.key.clone();
        let local_var_value = match local_var_apikey.prefix {
            Some(ref local_var_prefix) => format!("{} {}", local_var_prefix, local_var_key),
            None => local_var_key,
        };
        local_var_req_builder = local_var_req_builder.header("Authorization", local_var_value);
    };
    local_var_req_builder = local_var_req_builder.body(body);

    let local_var_req = local_var_req_builder.build()?;
    let local_var_resp = local_var_client.execute(local_var_req).await?;

    let local_var_status = local_var_resp.status();
    let local_var_content = local_var_resp.text().await?;

    if !local_var_status.is_client_error() && !local_var_status.is_server_error() {
        Ok(())
    } else {
        let local_var_entity: Option<cloud_openapi::apis::channels_api::ApiChannelsIdPatchError> =
            serde_json::from_str(&local_var_content).ok();
        let local_var_error = cloud_openapi::apis::ResponseContent {
            status: local_var_status,
            content: local_var_content,
            entity: local_var_entity,
        };
        Err(Error::ResponseError(local_var_error))
    }
}
