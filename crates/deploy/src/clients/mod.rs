pub mod cloud;
pub mod hippo;

use anyhow::Context;
use bindle::Id;
use semver::BuildMetadata;
use spin_publish::BindleConnectionInfo;
use std::path::Path;
use std::path::PathBuf;

use crate::deploy::DeploymentError;

pub async fn create_and_push_bindle(
    app: &Path,
    staging_dir: Option<PathBuf>,
    buildinfo: Option<BuildMetadata>,
    redeploy: bool,
    registry_connection: BindleConnectionInfo,
) -> Result<Id, DeploymentError> {
    tracing::trace!(
        "Pushing application to the registry: path: {}, redeploy: {}",
        app.display(),
        redeploy
    );
    let source_dir = app_dir(&app)?;

    let temp_dir = tempfile::tempdir().map_err(DeploymentError::IO)?;
    let dest_dir = match &staging_dir {
        None => temp_dir.path(),
        Some(p) => p.as_path(),
    };
    let (invoice, sources) = spin_publish::expand_manifest(&app, buildinfo, &dest_dir)
        .await
        .with_context(|| format!("Failed to expand '{}' to a bindle", app.display()))
        .map_err(DeploymentError::RegistryError)?;

    let bindle_id = &invoice.bindle.id;

    spin_publish::write(&source_dir, &dest_dir, &invoice, &sources)
        .await
        .with_context(|| write_failed_msg(bindle_id, dest_dir))
        .map_err(DeploymentError::RegistryError)?;

    // TODO: there used to be a sloth warning here.

    let publish_result =
        spin_publish::push_all(&dest_dir, bindle_id, registry_connection.clone()).await;

    if let Err(publish_err) = publish_result {
        // TODO: maybe use `thiserror` to return type errors.
        let already_exists = publish_err
            .to_string()
            .contains("already exists on the server");
        if already_exists {
            if redeploy {
                return Ok(bindle_id.clone());
            } else {
                return Err(DeploymentError::RegistryError(anyhow::anyhow!(
                    "Failed to push bindle to server.\n{}\nTry using the --deploy-existing-bindle flag",
                    publish_err
                )));
            }
        } else {
            println!("{:?}", publish_err);
            return Err(publish_err)
                .with_context(|| {
                    format!(
                        "Failed to push bindle {} to server {}",
                        bindle_id,
                        registry_connection.base_url(),
                    )
                })
                .map_err(DeploymentError::RegistryError);
        }
    }

    Ok(bindle_id.clone())
}

fn app_dir(app_file: impl AsRef<Path>) -> Result<PathBuf, DeploymentError> {
    let path_buf = app_file
        .as_ref()
        .parent()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Failed to get containing directory for app file '{}'",
                app_file.as_ref().display()
            )
        })
        .map_err(DeploymentError::RegistryError)?
        .to_owned();
    Ok(path_buf)
}

fn write_failed_msg(bindle_id: &bindle::Id, dest_dir: &Path) -> String {
    format!(
        "Failed to write bindle '{}' to {}",
        bindle_id,
        dest_dir.display()
    )
}
