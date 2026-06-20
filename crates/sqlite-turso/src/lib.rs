//! A Spin SQLite backend that uses the new [Turso] engine's local-first sync.
//!
//! Each database is a local SQLite file that the Turso engine keeps in sync with a
//! remote ("hosted") database: all reads and writes hit the local file, and changes
//! are pushed/pulled in the background (and on suspend). For **stateful
//! components**, the creator is scoped per `(component, instance)` so every
//! long-lived instance gets its own local file *and* its own remote database — the
//! SQLite analog of the per-instance key-value "instance-store".
//!
//! The per-instance remote database is obtained through a [`RemoteProvisioner`]:
//! either the hosted engine creates it automatically on first sync
//! ([`AutoCreateProvisioner`], the default) or it is created via the Turso Platform
//! API ([`TursoPlatformProvisioner`]).
//!
//! NOTE: Turso offline sync is BETA. There are no durability guarantees today and
//! conflict *resolution* is not yet implemented (only detection). This backend is
//! opt-in and not built into Spin by default.
//!
//! [Turso]: https://github.com/tursodatabase/turso

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use spin_factor_sqlite::{Connection, ConnectionCreator, QueryAsyncResult};
use spin_world::spin::sqlite3_1_0::sqlite as v3;
use spin_world::spin::sqlite3_1_0::sqlite::{self, RowResult};
use tokio::sync::{Mutex, OnceCell};

// -----------------------------------------------------------------------------
// Remote provisioning
// -----------------------------------------------------------------------------

/// How to reach a (per-instance) remote database.
pub struct RemoteTarget {
    pub url: String,
    pub token: Option<String>,
}

/// Ensures a per-instance remote database exists and returns how to sync to it.
///
/// Called once per instance, lazily, before the synced database is built.
#[async_trait]
pub trait RemoteProvisioner: Send + Sync {
    async fn ensure(&self, db_name: &str) -> anyhow::Result<RemoteTarget>;
}

/// Assumes the hosted engine creates the database automatically on first sync
/// (e.g. a `turso-auto`-style server, or the user's own sync server). Derives the
/// per-instance URL by appending the database name as a path segment to a base URL.
///
/// This is the default and matches "creation is automatic on sync".
pub struct AutoCreateProvisioner {
    pub base_url: String,
    pub token: Option<String>,
}

#[async_trait]
impl RemoteProvisioner for AutoCreateProvisioner {
    async fn ensure(&self, db_name: &str) -> anyhow::Result<RemoteTarget> {
        let url = if db_name.is_empty() {
            self.base_url.clone()
        } else {
            format!("{}/{}", self.base_url.trim_end_matches('/'), db_name)
        };
        Ok(RemoteTarget {
            url,
            token: self.token.clone(),
        })
    }
}

/// Provisions a database per instance via the Turso **Platform API** (the same
/// control plane `turso-auto` uses), for Turso Cloud.
///
/// The create call is idempotent (an existing database is treated as success). The
/// resulting sync URL is built from `url_template` (with `{db}`/`{org}`
/// placeholders) rather than parsed from the response, since the create-response
/// shape is still beta.
pub struct TursoPlatformProvisioner {
    api_url: String,
    org: String,
    group: String,
    api_token: String,
    url_template: String,
    db_token: Option<String>,
    http: reqwest::Client,
    provisioned: Mutex<HashSet<String>>,
}

impl TursoPlatformProvisioner {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        api_url: String,
        org: String,
        group: String,
        api_token: String,
        url_template: String,
        db_token: Option<String>,
    ) -> Self {
        Self {
            api_url,
            org,
            group,
            api_token,
            url_template,
            db_token,
            http: reqwest::Client::new(),
            provisioned: Mutex::new(HashSet::new()),
        }
    }
}

#[async_trait]
impl RemoteProvisioner for TursoPlatformProvisioner {
    async fn ensure(&self, db_name: &str) -> anyhow::Result<RemoteTarget> {
        let name = db_name.to_owned();
        let already = self.provisioned.lock().await.contains(&name);
        if !already {
            let endpoint = format!(
                "{}/v1/organizations/{}/databases",
                self.api_url.trim_end_matches('/'),
                self.org
            );
            let resp = self
                .http
                .post(&endpoint)
                .bearer_auth(&self.api_token)
                .json(&serde_json::json!({ "name": name, "group": self.group }))
                .send()
                .await
                .context("Turso Platform API request failed")?;
            let status = resp.status();
            // 409 == already exists, which is fine (idempotent).
            if !status.is_success() && status.as_u16() != 409 {
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("Turso Platform API: creating database '{name}' failed ({status}): {body}");
            }
            self.provisioned.lock().await.insert(name.clone());
        }
        let url = self
            .url_template
            .replace("{db}", &name)
            .replace("{org}", &self.org);
        Ok(RemoteTarget {
            url,
            token: self.db_token.clone(),
        })
    }
}

// -----------------------------------------------------------------------------
// Connection creator
// -----------------------------------------------------------------------------

/// Creates connections to Turso-synced SQLite databases.
///
/// When [`scoped_to_instance`](ConnectionCreator::scoped_to_instance) is called
/// (by a stateful-component worker), a clone is returned with `instance_id` set,
/// which derives a per-instance local file path and remote database name.
#[derive(Clone)]
pub struct TursoConnectionCreator {
    /// Base directory under which (per-instance) local database files are created.
    local_dir: PathBuf,
    /// How the per-instance remote database is obtained.
    provisioner: Arc<dyn RemoteProvisioner>,
    /// Background sync interval. `None` disables periodic sync (sync still happens
    /// once on open and on suspend).
    sync_interval: Option<Duration>,
    /// `Some("{component}/{instance}")` for an instance-scoped creator.
    instance_id: Option<String>,
}

impl TursoConnectionCreator {
    pub fn new(
        local_dir: PathBuf,
        provisioner: Arc<dyn RemoteProvisioner>,
        sync_interval: Option<Duration>,
    ) -> Self {
        Self {
            local_dir,
            provisioner,
            sync_interval,
            instance_id: None,
        }
    }

    /// The (sanitized) database name for this creator: the instance id for an
    /// instance-scoped creator, else `"shared"`.
    fn db_name(&self) -> String {
        match &self.instance_id {
            Some(id) => sanitize(id),
            None => "shared".to_owned(),
        }
    }

    fn local_path(&self) -> PathBuf {
        self.local_dir.join(format!("{}.db", self.db_name()))
    }
}

#[async_trait]
impl ConnectionCreator for TursoConnectionCreator {
    async fn create_connection(
        &self,
        _label: &str,
    ) -> Result<Arc<dyn Connection + 'static>, v3::Error> {
        Ok(Arc::new(LazyTursoConnection::new(
            self.local_path(),
            Arc::clone(&self.provisioner),
            self.db_name(),
            self.sync_interval,
        )))
    }

    fn scoped_to_instance(&self, instance_id: &str) -> Option<Arc<dyn ConnectionCreator>> {
        let mut scoped = self.clone();
        scoped.instance_id = Some(instance_id.to_owned());
        Some(Arc::new(scoped))
    }
}

/// Replace path separators and other awkward characters so an instance id is safe
/// to use as a file name / database name.
fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => c,
            _ => '_',
        })
        .collect()
}

// -----------------------------------------------------------------------------
// Connection
// -----------------------------------------------------------------------------

/// A lazily-initialized [`Connection`] backed by a Turso synced database.
///
/// The synced database can only be built asynchronously, so (like the libSQL
/// backend) we defer creation — and provisioning — to the first use via a
/// [`OnceCell`].
pub struct LazyTursoConnection {
    local_path: PathBuf,
    provisioner: Arc<dyn RemoteProvisioner>,
    db_name: String,
    sync_interval: Option<Duration>,
    inner: OnceCell<TursoConnection>,
}

impl LazyTursoConnection {
    pub fn new(
        local_path: PathBuf,
        provisioner: Arc<dyn RemoteProvisioner>,
        db_name: String,
        sync_interval: Option<Duration>,
    ) -> Self {
        Self {
            local_path,
            provisioner,
            db_name,
            sync_interval,
            inner: OnceCell::new(),
        }
    }

    async fn get_or_create_connection(&self) -> Result<&TursoConnection, v3::Error> {
        self.inner
            .get_or_try_init(|| async {
                let target = self
                    .provisioner
                    .ensure(&self.db_name)
                    .await
                    .context("failed to provision remote Turso database")?;
                TursoConnection::create(
                    self.local_path.clone(),
                    target.url,
                    target.token,
                    self.sync_interval,
                )
                .await
                .context("failed to create Turso synced database")
            })
            .await
            .map_err(|_| v3::Error::InvalidConnection)
    }
}

#[async_trait]
impl Connection for LazyTursoConnection {
    async fn query(
        &self,
        query: &str,
        parameters: Vec<v3::Value>,
        max_result_bytes: usize,
    ) -> Result<v3::QueryResult, v3::Error> {
        self.get_or_create_connection()
            .await?
            .query(query, parameters, max_result_bytes)
            .await
    }

    async fn query_async(
        &self,
        query: &str,
        parameters: Vec<v3::Value>,
        max_result_bytes: usize,
    ) -> Result<QueryAsyncResult, v3::Error> {
        self.get_or_create_connection()
            .await?
            .query_async(query, parameters, max_result_bytes)
            .await
    }

    async fn execute_batch(&self, statements: &str) -> anyhow::Result<()> {
        self.get_or_create_connection()
            .await?
            .execute_batch(statements)
            .await
    }

    async fn changes(&self) -> Result<u64, sqlite::Error> {
        Ok(self.get_or_create_connection().await?.changes().await)
    }

    async fn last_insert_rowid(&self) -> Result<i64, sqlite::Error> {
        Ok(self.get_or_create_connection().await?.last_insert_rowid())
    }

    fn summary(&self) -> Option<String> {
        Some(format!(
            "Turso (local {}, remote db {})",
            self.local_path.display(),
            self.db_name
        ))
    }

    async fn sync(&self) -> anyhow::Result<()> {
        // Only push if the database was actually opened (queried); there is
        // nothing to flush otherwise, and we don't want to connect just to sync.
        if let Some(conn) = self.inner.get() {
            conn.push().await?;
        }
        Ok(())
    }
}

/// An open connection to a Turso synced database.
///
/// Holds both the [`turso::sync::Database`] (used to drive push/pull) and a
/// [`turso::Connection`] (used for queries). All queries run locally; sync happens
/// once on open, periodically in the background, and on suspend (via [`push`]).
pub struct TursoConnection {
    /// Retained so we can `push`/`pull`. Shared with the background sync task.
    db: Arc<turso::sync::Database>,
    conn: turso::Connection,
}

impl TursoConnection {
    pub async fn create(
        local_path: PathBuf,
        remote_url: String,
        token: Option<String>,
        sync_interval: Option<Duration>,
    ) -> anyhow::Result<Self> {
        if let Some(parent) = local_path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }

        let mut builder =
            turso::sync::Builder::new_remote(&local_path.to_string_lossy()).with_remote_url(&remote_url);
        if let Some(token) = &token {
            builder = builder.with_auth_token(token);
        }
        let db = Arc::new(builder.build().await?);

        // Warm the local replica from the remote on open (pull on "instantiate").
        // Best-effort: a brand-new remote may be empty/just-created.
        let _ = db.pull().await;

        let conn = db.connect().await?;

        // Periodic background sync while this connection is alive. A `Weak` ref so
        // the task stops (and the database is freed) once the connection is
        // dropped — e.g. when the stateful instance is suspended/evicted.
        if let Some(interval) = sync_interval {
            let db = Arc::downgrade(&db);
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(interval);
                ticker.tick().await; // consume the immediate first tick
                loop {
                    ticker.tick().await;
                    let Some(db) = db.upgrade() else { break };
                    if let Err(e) = db.push().await {
                        tracing::debug!("Turso background push failed: {e}");
                    }
                    if let Err(e) = db.pull().await {
                        tracing::debug!("Turso background pull failed: {e}");
                    }
                }
            });
        }

        Ok(Self { db, conn })
    }

    /// Push local changes to the remote. Called by the host on suspend.
    pub async fn push(&self) -> anyhow::Result<()> {
        self.db.push().await?;
        Ok(())
    }

    async fn query(
        &self,
        query: &str,
        parameters: Vec<sqlite::Value>,
        max_result_bytes: usize,
    ) -> Result<sqlite::QueryResult, sqlite::Error> {
        let rows = self
            .conn
            .query(query, convert_parameters(&parameters))
            .await
            .map_err(io_error)?;

        Ok(sqlite::QueryResult {
            columns: columns(&rows),
            rows: convert_rows(rows, max_result_bytes)
                .await
                .map_err(|e| sqlite::Error::Io(e.to_string()))?,
        })
    }

    async fn query_async(
        &self,
        query: &str,
        parameters: Vec<v3::Value>,
        max_result_bytes: usize,
    ) -> Result<QueryAsyncResult, v3::Error> {
        let (cols_tx, cols_rx) = tokio::sync::oneshot::channel();
        let (rows_tx, rows_rx) = tokio::sync::mpsc::channel(4);
        let (err_tx, err_rx) = tokio::sync::oneshot::channel();

        let result = self.conn.query(query, convert_parameters(&parameters)).await;

        let mut rows = match result {
            Ok(r) => r,
            Err(e) => {
                let _ = cols_tx.send(Default::default());
                let _ = err_tx.send(Err(v3::Error::Io(e.to_string())));
                return Ok(QueryAsyncResult {
                    columns: Vec::new(),
                    rows: rows_rx,
                    error: err_rx,
                });
            }
        };

        let cols = columns(&rows);
        let _ = cols_tx.send(cols.clone());
        let col_count = cols.len();

        tokio::spawn(async move {
            let work = async {
                let mut byte_count = 0usize;
                while let Some(row) = rows.next().await.map_err(io_error_v3)? {
                    let row = convert_row(&row, col_count);
                    byte_count += row.values.iter().map(|v| v.memory_size()).sum::<usize>();
                    if byte_count > max_result_bytes {
                        return Err(v3::Error::Io(format!(
                            "query result exceeds limit of {max_result_bytes} bytes"
                        )));
                    }
                    rows_tx
                        .send(row)
                        .await
                        .map_err(|_| v3::Error::Io("row send error".into()))?;
                }
                Ok(())
            };
            let _ = err_tx.send(work.await);
        });

        let columns = cols_rx.await.map_err(|e| v3::Error::Io(e.to_string()))?;
        Ok(QueryAsyncResult {
            columns,
            rows: rows_rx,
            error: err_rx,
        })
    }

    async fn execute_batch(&self, statements: &str) -> anyhow::Result<()> {
        self.conn.execute_batch(statements).await?;
        Ok(())
    }

    async fn changes(&self) -> u64 {
        // turso::Connection does not expose a `changes()` accessor, so use the
        // SQLite builtin (a `SELECT` does not reset it).
        self.scalar_i64("SELECT changes()")
            .await
            .unwrap_or(0)
            .max(0) as u64
    }

    fn last_insert_rowid(&self) -> i64 {
        self.conn.last_insert_rowid()
    }

    /// Run a query and return the first column of the first row as an integer.
    async fn scalar_i64(&self, sql: &str) -> Option<i64> {
        let mut rows = self.conn.query(sql, ()).await.ok()?;
        let row = rows.next().await.ok()??;
        match row.get_value(0).ok()? {
            turso::Value::Integer(i) => Some(i),
            _ => None,
        }
    }
}

fn columns(rows: &turso::Rows) -> Vec<String> {
    (0..rows.column_count())
        .map(|i| rows.column_name(i).unwrap_or_default())
        .collect()
}

async fn convert_rows(
    mut rows: turso::Rows,
    max_result_bytes: usize,
) -> anyhow::Result<Vec<RowResult>> {
    let mut result_rows = vec![];
    let column_count = rows.column_count();
    let mut byte_count = 0;
    while let Some(row) = rows.next().await? {
        let row = convert_row(&row, column_count);
        byte_count += row.values.iter().map(|v| v.memory_size()).sum::<usize>();
        if byte_count > max_result_bytes {
            anyhow::bail!("query result exceeds limit of {max_result_bytes} bytes")
        }
        result_rows.push(row);
    }
    Ok(result_rows)
}

fn convert_row(row: &turso::Row, column_count: usize) -> RowResult {
    let values = (0..column_count)
        .map(|i| convert_value(row.get_value(i).unwrap()))
        .collect();
    RowResult { values }
}

fn convert_value(v: turso::Value) -> sqlite::Value {
    match v {
        turso::Value::Null => sqlite::Value::Null,
        turso::Value::Integer(value) => sqlite::Value::Integer(value),
        turso::Value::Real(value) => sqlite::Value::Real(value),
        turso::Value::Text(value) => sqlite::Value::Text(value),
        turso::Value::Blob(value) => sqlite::Value::Blob(value),
    }
}

fn convert_parameters(parameters: &[sqlite::Value]) -> Vec<turso::Value> {
    parameters
        .iter()
        .map(|v| match v {
            sqlite::Value::Integer(value) => turso::Value::Integer(*value),
            sqlite::Value::Real(value) => turso::Value::Real(*value),
            sqlite::Value::Text(t) => turso::Value::Text(t.clone()),
            sqlite::Value::Blob(b) => turso::Value::Blob(b.clone()),
            sqlite::Value::Null => turso::Value::Null,
        })
        .collect()
}

fn io_error(err: turso::Error) -> sqlite::Error {
    sqlite::Error::Io(err.to_string())
}

fn io_error_v3(err: turso::Error) -> v3::Error {
    v3::Error::Io(err.to_string())
}
