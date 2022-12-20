use anyhow::Result;
use clap::{Parser, Subcommand};
use semver::BuildMetadata;
use spin_loader::local::config::RawAppManifestAnyVersion;

use std::path::PathBuf;

use crate::{opts::*, parse_buildinfo};

#[derive(Subcommand, Debug)]
pub enum OciCommands {
    Push(Push),
}

impl OciCommands {
    pub async fn run(self) -> Result<()> {
        match self {
            OciCommands::Push(cmd) => cmd.run().await,
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

    /// Build metadata to append to the bindle version
    #[clap(
        name = BUILDINFO_OPT,
        long = "buildinfo",
        parse(try_from_str = parse_buildinfo),
    )]
    pub buildinfo: Option<BuildMetadata>,

    /// Ignore server certificate errors
    #[clap(
        name = INSECURE_OPT,
        short = 'k',
        long = "insecure",
        takes_value = false,
    )]
    pub insecure: bool,
}

impl Push {
    pub async fn run(self) -> Result<()> {
        let app_file = self
            .app
            .as_deref()
            .unwrap_or_else(|| DEFAULT_MANIFEST_FILE.as_ref());

        let dir = tempfile::tempdir()?;
        let app = spin_loader::local::from_file(&app_file, &dir, &None).await?;
        let locked_app = spin_trigger::locked::build_locked_app(app, dir.path())?;

        println!("{:?}", locked_app);

        Ok(())
    }
}
