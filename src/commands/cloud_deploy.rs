use anyhow::Result;
use clap::Parser;
use semver::BuildMetadata;
use spin_deploy::config::Config;
use spin_deploy::deploy::DeploymentClient;
use std::path::PathBuf;

use crate::{opts::*, parse_buildinfo};

/// Package and upload Spin artifacts, notifying Hippo
#[derive(Parser, Debug)]
#[clap(about = "Deploy a Spin application")]
pub struct DeployCommand {
    /// Path to spin.toml
    #[clap(
        name = APP_CONFIG_FILE_OPT,
        short = 'f',
        long = "file",
        default_value = "spin.toml"
    )]
    pub app: PathBuf,

    /// Path to assemble the bindle before pushing (defaults to
    /// a temporary directory)
    #[clap(
        name = STAGING_DIR_OPT,
        long = "staging-dir",
        short = 'd',
    )]
    pub staging_dir: Option<PathBuf>,

    /// Disable attaching buildinfo
    #[clap(
        long = "no-buildinfo",
        conflicts_with = BUILDINFO_OPT,
        env = "SPIN_DEPLOY_NO_BUILDINFO"
    )]
    pub no_buildinfo: bool,

    /// Build metadata to append to the bindle version
    #[clap(
        name = BUILDINFO_OPT,
        long = "buildinfo",
        parse(try_from_str = parse_buildinfo),
    )]
    pub buildinfo: Option<BuildMetadata>,

    /// Deploy existing bindle if it already exists on bindle server
    #[clap(short = 'e', long = "deploy-existing-bindle")]
    pub redeploy: bool,

    /// How long in seconds to wait for a deployed HTTP application to become
    /// ready. The default is 60 seconds. Set it to 0 to skip waiting
    /// for readiness.
    #[clap(long = "readiness-timeout", default_value = "60")]
    pub readiness_timeout_secs: u16,

    /// Configuration file to read the authentication token from.
    /// This lets users switch between multiple environments without having to
    /// re-authenticate.
    #[clap(long, env = "SPIN_AUTH")]
    pub config: Option<PathBuf>,
}

impl DeployCommand {
    pub async fn run(self) -> Result<()> {
        let cfg = Config::new(self.config).await?;
        let client = DeploymentClient { auth: cfg.auth };
        let details = client
            .deploy(&self.app, self.staging_dir, self.buildinfo, self.redeploy)
            .await?;

        // TODO: print available routes.
        println!(
            "Application {}/{} deployed, running at {}",
            details.name, details.version, details.url
        );

        Ok(())
    }
}
