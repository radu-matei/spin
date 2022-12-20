use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use reqwest::Url;
use spin_trigger::cli::{SPIN_LOCKED_URL, SPIN_WORKING_DIR};

use std::path::PathBuf;

use crate::opts::*;

/// Commands for working with OCI registries to distribute applications.
#[derive(Subcommand, Debug)]
pub enum OciCommands {
    /// Push a Spin application to an OCI registry.
    Push(Push),
    /// Pull a Spin application from an OCI registry.
    Pull(Pull),
    /// Run a Spin application from an OCI registry.
    Run(Run),
}

impl OciCommands {
    pub async fn run(self) -> Result<()> {
        match self {
            OciCommands::Push(cmd) => cmd.run().await,
            OciCommands::Pull(cmd) => cmd.run().await,
            OciCommands::Run(cmd) => cmd.run().await,
        }
    }
}

#[derive(Parser, Debug)]
pub struct Push {
    /// Path to spin.toml
    #[clap(
        name = APP_CONFIG_FILE_OPT,
        short = 'f',
        long = "file",
    )]
    pub app: Option<PathBuf>,

    /// Ignore server certificate errors
    #[clap(
        name = INSECURE_OPT,
        short = 'k',
        long = "insecure",
        takes_value = false,
    )]
    pub insecure: bool,

    /// Reference of the Spin application
    #[clap()]
    pub reference: String,
}

impl Push {
    pub async fn run(self) -> Result<()> {
        let app_file = self
            .app
            .as_deref()
            .unwrap_or_else(|| DEFAULT_MANIFEST_FILE.as_ref());

        let dir = tempfile::tempdir()?;
        let app = spin_loader::local::from_file(&app_file, Some(dir.path()), &None).await?;

        let mut client = spin_publish::oci::client::Client::new(self.insecure, None).await?;
        client.push(&app, &self.reference).await?;
        Ok(())
    }
}

#[derive(Parser, Debug)]
pub struct Pull {
    /// Ignore server certificate errors
    #[clap(
        name = INSECURE_OPT,
        short = 'k',
        long = "insecure",
        takes_value = false,
    )]
    pub insecure: bool,

    /// Reference of the Spin application
    #[clap()]
    pub reference: String,
}

impl Pull {
    /// Pull a Spin application from an OCI registry
    pub async fn run(self) -> Result<()> {
        let mut client = spin_publish::oci::client::Client::new(self.insecure, None).await?;
        client.pull(&self.reference).await?;

        Ok(())
    }
}

#[derive(Parser, Debug)]
pub struct Run {
    /// Ignore server certificate errors
    #[clap(
        name = INSECURE_OPT,
        short = 'k',
        long = "insecure",
        takes_value = false,
    )]
    pub insecure: bool,

    /// Reference of the Spin application
    #[clap()]
    pub reference: String,
}

impl Run {
    /// Run a Spin application from an OCI registry
    pub async fn run(self) -> Result<()> {
        let mut client = spin_publish::oci::client::Client::new(self.insecure, None).await?;
        client.pull(&self.reference).await?;

        let app = client.cache.config_for_reference(&self.reference).await?;
        let working_dir = tempfile::tempdir()?;

        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.arg("trigger")
            .arg("http")
            .arg("--oci")
            .env(SPIN_WORKING_DIR, &working_dir.path());

        let url = Url::from_file_path(&app)
            .expect("cannot parse URL from locked app file")
            .to_string();
        cmd.env(SPIN_LOCKED_URL, &url);

        tracing::trace!("Running trigger executor: {:?}", cmd);

        let mut child = cmd.spawn().context("Failed to execute trigger")?;

        // Terminate trigger executor if `spin up` itself receives a termination signal
        #[cfg(not(windows))]
        {
            // https://github.com/nix-rust/nix/issues/656
            let pid = nix::unistd::Pid::from_raw(child.id() as i32);
            ctrlc::set_handler(move || {
                if let Err(err) = nix::sys::signal::kill(pid, nix::sys::signal::SIGTERM) {
                    tracing::warn!("Failed to kill trigger handler process: {:?}", err)
                }
            })?;
        }

        let status = child.wait()?;
        if status.success() {
            Ok(())
        } else {
            bail!(status);
        }
    }
}
