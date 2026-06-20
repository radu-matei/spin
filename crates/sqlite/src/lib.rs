//! Spin's default handling of the runtime configuration for SQLite databases.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::Deserialize;
use spin_factor_sqlite::ConnectionCreator;
use spin_factors::{
    anyhow::{self, Context as _},
    runtime_config::toml::GetTomlValue,
};
use spin_sqlite_inproc::InProcDatabaseLocation;
use spin_sqlite_libsql::LazyLibSqlConnection;

/// Spin's default resolution of runtime configuration for SQLite databases.
///
/// This type implements how Spin CLI's SQLite implementation is configured
/// through the runtime config toml as well as the behavior of the "default" label.
#[derive(Clone, Debug)]
pub struct RuntimeConfigResolver {
    default_database_dir: Option<PathBuf>,
    local_database_dir: PathBuf,
}

impl RuntimeConfigResolver {
    /// Create a new `SpinSqliteRuntimeConfig`
    ///
    /// This takes as arguments:
    /// * the directory to use as the default location for SQLite databases.
    ///   Usually this will be the path to the `.spin` state directory. If
    ///   `None`, the default database will be in-memory.
    /// * the path to the directory from which relative paths to
    ///   local SQLite databases are resolved.  (this should most likely be the
    ///   path to the runtime-config file or the current working dir).
    pub fn new(default_database_dir: Option<PathBuf>, local_database_dir: PathBuf) -> Self {
        Self {
            default_database_dir,
            local_database_dir,
        }
    }

    /// Get the runtime configuration for SQLite databases from a TOML table.
    ///
    /// Expects table to be in the format:
    /// ````toml
    /// [sqlite_database.$database-label]
    /// type = "$database-type"
    /// ... extra type specific configuration ...
    /// ```
    ///
    /// Configuration is automatically added for the 'default' label if it is not provided.
    pub fn resolve(
        &self,
        table: &impl GetTomlValue,
    ) -> anyhow::Result<spin_factor_sqlite::runtime_config::RuntimeConfig> {
        let mut runtime_config = self.resolve_from_toml(table)?.unwrap_or_default();
        // If the user did not provide configuration for the default label, add it.
        if !runtime_config.connection_creators.contains_key("default") {
            runtime_config
                .connection_creators
                .insert("default".to_owned(), self.default());
        }

        Ok(runtime_config)
    }

    /// Get the runtime configuration for SQLite databases from a TOML table.
    fn resolve_from_toml(
        &self,
        table: &impl GetTomlValue,
    ) -> anyhow::Result<Option<spin_factor_sqlite::runtime_config::RuntimeConfig>> {
        let Some(table) = table.get("sqlite_database") else {
            return Ok(None);
        };
        let config: std::collections::HashMap<String, TomlRuntimeConfig> =
            table.clone().try_into()?;
        let connection_creators = config
            .into_iter()
            .map(|(k, v)| Ok((k, self.get_connection_creator(v)?)))
            .collect::<anyhow::Result<HashMap<_, _>>>()?;

        Ok(Some(spin_factor_sqlite::runtime_config::RuntimeConfig {
            connection_creators,
        }))
    }

    /// Get a connection creator for a given runtime configuration.
    pub fn get_connection_creator(
        &self,
        config: TomlRuntimeConfig,
    ) -> anyhow::Result<Arc<dyn ConnectionCreator>> {
        let database_kind = config.type_.as_str();
        match database_kind {
            "spin" => {
                let config: InProcDatabase = config.config.try_into()?;
                Ok(Arc::new(
                    config.connection_creator(&self.local_database_dir)?,
                ))
            }
            "libsql" => {
                let config: LibSqlDatabase = config.config.try_into()?;
                Ok(Arc::new(config.connection_creator()?))
            }
            "turso" => {
                #[cfg(feature = "turso")]
                {
                    let config: TursoDatabase = config.config.try_into()?;
                    config.connection_creator(&self.local_database_dir)
                }
                #[cfg(not(feature = "turso"))]
                {
                    anyhow::bail!(
                        "the 'turso' SQLite backend is not enabled in this build of Spin; rebuild with the `turso` feature"
                    )
                }
            }
            _ => anyhow::bail!("Unknown database kind: {database_kind}"),
        }
    }
}

#[derive(Deserialize)]
pub struct TomlRuntimeConfig {
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(flatten)]
    pub config: toml::Table,
}

impl RuntimeConfigResolver {
    /// The [`ConnectionCreator`] for the 'default' label.
    pub fn default(&self) -> Arc<dyn ConnectionCreator> {
        let path = self
            .default_database_dir
            .as_deref()
            .map(|p| p.join(DEFAULT_SQLITE_DB_FILENAME));
        let factory = move || {
            let location = InProcDatabaseLocation::from_path(path.clone())?;
            let connection = spin_sqlite_inproc::InProcConnection::new(location, false)?;
            Ok(Arc::new(connection) as _)
        };
        Arc::new(factory)
    }
}

const DEFAULT_SQLITE_DB_FILENAME: &str = "sqlite_db.db";

/// Configuration for a local SQLite database.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InProcDatabase {
    pub path: Option<PathBuf>,

    /// If `false` (the default), disallows `ATTACH`ing an existing file to a
    /// database connection.
    ///
    /// Note: Attaching a new tempfile or `:memory:` database is always allowed.
    #[serde(default)]
    pub allow_attach_file: bool,
}

impl InProcDatabase {
    /// Get a new connection creator for a local database.
    ///
    /// `base_dir` is the base directory path from which `path` is resolved if it is a relative path.
    fn connection_creator(
        self,
        base_dir: &Path,
    ) -> anyhow::Result<impl ConnectionCreator + 'static> {
        let path = self
            .path
            .as_ref()
            .map(|p| resolve_relative_path(p, base_dir));
        let location = InProcDatabaseLocation::from_path(path)?;
        let factory = move || {
            let connection = spin_sqlite_inproc::InProcConnection::new(
                location.clone(),
                self.allow_attach_file,
            )?;
            Ok(Arc::new(connection) as _)
        };
        Ok(factory)
    }
}

/// Resolve a relative path against a base dir.
///
/// If the path is absolute, it is returned as is. Otherwise, it is resolved against the base dir.
fn resolve_relative_path(path: &Path, base_dir: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_owned();
    }
    base_dir.join(path)
}

/// Configuration for a libSQL database.
///
/// This is used to deserialize the specific runtime config toml for libSQL databases.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibSqlDatabase {
    url: String,
    token: String,
}

impl LibSqlDatabase {
    /// Get a new connection creator for a libSQL database.
    fn connection_creator(self) -> anyhow::Result<impl ConnectionCreator> {
        let url = check_url(&self.url)
            .with_context(|| {
                format!(
                    "unexpected libSQL URL '{}' in runtime config file ",
                    self.url
                )
            })?
            .to_owned();
        let factory = move || {
            let connection = LazyLibSqlConnection::new(url.clone(), self.token.clone());
            Ok(Arc::new(connection) as _)
        };
        Ok(factory)
    }
}

/// How a Turso database provisions its per-instance remote databases.
#[cfg(feature = "turso")]
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TursoProvision {
    /// The hosted engine creates the per-instance database automatically on first
    /// sync (e.g. a turso-auto-style server, or your own sync server). Requires
    /// `url`. This is the default.
    #[default]
    Auto,
    /// Create each per-instance database via the Turso Platform API (Turso Cloud).
    Platform,
}

/// Configuration for a Turso local-first synced database.
///
/// All reads/writes hit a local SQLite file; the Turso engine syncs it to a hosted
/// database. For stateful components, the connection creator is scoped per
/// `(component, instance)` (see
/// [`spin_factor_sqlite::ConnectionCreator::scoped_to_instance`]), giving each
/// instance its own local file and remote database, obtained via the configured
/// provisioner.
#[cfg(feature = "turso")]
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TursoDatabase {
    /// How per-instance remote databases are provisioned.
    #[serde(default)]
    provision: TursoProvision,
    /// Base remote URL of the hosted Turso engine (required for `provision = "auto"`;
    /// e.g. `http://127.0.0.1:8080` for a local `turso dev`).
    url: Option<String>,
    /// Auth token for the remote (optional; a local `turso dev` needs none).
    token: Option<String>,
    /// Directory (resolved relative to the runtime-config/state dir) under which
    /// per-instance local database files are created.
    #[serde(default = "default_turso_local_dir")]
    local_dir: PathBuf,
    /// Background sync interval, in seconds. `0` (the default) disables periodic
    /// sync (a sync still happens on open and on suspend).
    #[serde(default)]
    sync_interval_seconds: u64,
    /// Turso Platform API base URL (default `https://api.turso.tech`).
    api_url: Option<String>,
    /// Organization slug (for `provision = "platform"`).
    org: Option<String>,
    /// Group name (for `provision = "platform"`).
    group: Option<String>,
    /// Platform API token (for `provision = "platform"`).
    api_token: Option<String>,
    /// Sync-URL template with `{db}`/`{org}` placeholders (default
    /// `libsql://{db}-{org}.turso.io`).
    url_template: Option<String>,
    /// Token used to sync to a provisioned database (for `provision = "platform"`).
    db_token: Option<String>,
}

#[cfg(feature = "turso")]
fn default_turso_local_dir() -> PathBuf {
    PathBuf::from("turso-instance-dbs")
}

#[cfg(feature = "turso")]
impl TursoDatabase {
    fn connection_creator(self, base_dir: &Path) -> anyhow::Result<Arc<dyn ConnectionCreator>> {
        use spin_sqlite_turso::{
            AutoCreateProvisioner, RemoteProvisioner, TursoConnectionCreator,
            TursoPlatformProvisioner,
        };

        let local_dir = resolve_relative_path(&self.local_dir, base_dir);
        let sync_interval = (self.sync_interval_seconds > 0)
            .then(|| std::time::Duration::from_secs(self.sync_interval_seconds));

        let provisioner: Arc<dyn RemoteProvisioner> = match self.provision {
            TursoProvision::Auto => {
                let url = self
                    .url
                    .context("a Turso database with provision = \"auto\" requires a `url`")?;
                let url = check_url(&url)
                    .with_context(|| format!("unexpected Turso URL '{url}' in runtime config file"))?
                    .to_owned();
                Arc::new(AutoCreateProvisioner {
                    base_url: url,
                    token: self.token,
                })
            }
            TursoProvision::Platform => Arc::new(TursoPlatformProvisioner::new(
                self.api_url
                    .unwrap_or_else(|| "https://api.turso.tech".to_owned()),
                self.org
                    .context("provision = \"platform\" requires `org`")?,
                self.group
                    .context("provision = \"platform\" requires `group`")?,
                self.api_token
                    .context("provision = \"platform\" requires `api_token`")?,
                self.url_template
                    .unwrap_or_else(|| "libsql://{db}-{org}.turso.io".to_owned()),
                self.db_token,
            )),
        };

        Ok(Arc::new(TursoConnectionCreator::new(
            local_dir,
            provisioner,
            sync_interval,
        )))
    }
}

// Checks an incoming url is in the shape we expect
fn check_url(url: &str) -> anyhow::Result<&str> {
    if url.starts_with("https://") || url.starts_with("http://") {
        Ok(url)
    } else {
        Err(anyhow::anyhow!(
            "URL does not start with 'https://' or 'http://'. Spin currently only supports talking to libSQL databases over HTTP(S)"
        ))
    }
}
