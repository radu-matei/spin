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

use std::collections::HashMap;
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
#[derive(Clone)]
pub struct RemoteTarget {
    pub url: String,
    pub token: Option<String>,
    /// `true` if this call *just created* the remote database (so it is empty and
    /// the local replica is the source of truth — push-first, never bootstrap);
    /// `false` if it already existed (a cold local replica should bootstrap from it).
    pub created: bool,
}

/// Ensures a per-instance remote database exists and returns how to sync to it.
///
/// Called once per instance, lazily, before the synced database is built.
#[async_trait]
pub trait RemoteProvisioner: Send + Sync {
    async fn ensure(&self, db_name: &str) -> anyhow::Result<RemoteTarget>;
}

/// Targets a **single** remote database at `base_url`.
///
/// This is the `provision = "auto"` mode, for one local `tursodb --sync-server`
/// (or any single-database server). It is NOT per-instance: every instance shares
/// the one remote, because Turso addresses a database by its URL *host*, not a path
/// — a single-db server has no way to route per-instance. For a remote database
/// *per* instance, use `provision = "platform"` (Turso Cloud). Local files stay
/// per-instance regardless.
pub struct AutoCreateProvisioner {
    pub base_url: String,
    pub token: Option<String>,
}

#[async_trait]
impl RemoteProvisioner for AutoCreateProvisioner {
    async fn ensure(&self, _db_name: &str) -> anyhow::Result<RemoteTarget> {
        Ok(RemoteTarget {
            url: self.base_url.clone(),
            token: self.token.clone(),
            // A single-db server is pre-existing infrastructure, not created here;
            // a cold local replica should bootstrap from whatever it holds.
            created: false,
        })
    }
}

/// Provisions a database **per instance** via the Turso **Platform API**, for Turso
/// Cloud. This is the multi-database path: each instance gets its own Cloud
/// database, addressed by its own hostname.
///
/// `ensure` creates the database (idempotent), reads its real `Hostname` from the
/// API response (Cloud hostnames include a region, so a template is unreliable),
/// and resolves a sync token — a configured group token if set, else a freshly
/// minted db-scoped token. Results are cached per instance.
pub struct TursoPlatformProvisioner {
    api_url: String,
    org: String,
    group: String,
    /// Org/platform token, used to create databases and mint tokens.
    api_token: String,
    /// Group token used for syncing (authenticates every db in the group). If
    /// `None`, a db-scoped token is minted per database.
    db_token: Option<String>,
    /// Prepended to each derived Cloud database name (to namespace within the org).
    name_prefix: String,
    http: reqwest::Client,
    /// instance db name -> resolved sync target.
    cache: Mutex<HashMap<String, RemoteTarget>>,
}

impl TursoPlatformProvisioner {
    pub fn new(
        api_url: String,
        org: String,
        group: String,
        api_token: String,
        db_token: Option<String>,
        name_prefix: String,
    ) -> Self {
        Self {
            api_url,
            org,
            group,
            api_token,
            db_token,
            name_prefix,
            http: reqwest::Client::new(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn api(&self) -> &str {
        self.api_url.trim_end_matches('/')
    }

    /// Create the database if needed (idempotent) and return its hostname plus
    /// whether this call actually created it (`true`) or it already existed
    /// (`false`). The created flag decides whether a cold local replica should
    /// bootstrap from the remote (existing) or push-first (just created, empty).
    async fn ensure_database(&self, name: &str) -> anyhow::Result<(String, bool)> {
        let resp = self
            .http
            .post(format!("{}/v1/organizations/{}/databases", self.api(), self.org))
            .bearer_auth(&self.api_token)
            .json(&serde_json::json!({ "name": name, "group": self.group }))
            .send()
            .await
            .context("Turso Platform API: create database request failed")?;
        let status = resp.status();
        if status.is_success() {
            return Ok((parse_hostname(resp).await?, true));
        }
        // 409 (and some deployments 400) == already exists: fetch it instead.
        if status.as_u16() == 409 || status.as_u16() == 400 {
            return Ok((self.get_database_hostname(name).await?, false));
        }
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Turso Platform API: creating database '{name}' failed ({status}): {body}");
    }

    async fn get_database_hostname(&self, name: &str) -> anyhow::Result<String> {
        let resp = self
            .http
            .get(format!("{}/v1/organizations/{}/databases/{}", self.api(), self.org, name))
            .bearer_auth(&self.api_token)
            .send()
            .await
            .context("Turso Platform API: get database request failed")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Turso Platform API: getting database '{name}' failed ({status}): {body}");
        }
        parse_hostname(resp).await
    }

    async fn mint_token(&self, name: &str) -> anyhow::Result<String> {
        let resp = self
            .http
            .post(format!(
                "{}/v1/organizations/{}/databases/{}/auth/tokens",
                self.api(),
                self.org,
                name
            ))
            .bearer_auth(&self.api_token)
            .send()
            .await
            .context("Turso Platform API: mint token request failed")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Turso Platform API: minting token for '{name}' failed ({status}): {body}");
        }
        let v: serde_json::Value = resp.json().await.context("parse token response")?;
        v.get("jwt")
            .and_then(|j| j.as_str())
            .map(|s| s.to_owned())
            .context("Turso Platform API: token response missing `jwt`")
    }
}

#[async_trait]
impl RemoteProvisioner for TursoPlatformProvisioner {
    async fn ensure(&self, db_name: &str) -> anyhow::Result<RemoteTarget> {
        if let Some(target) = self.cache.lock().await.get(db_name) {
            return Ok(target.clone());
        }
        let cloud_name = cloud_db_name(&self.name_prefix, db_name);
        let (hostname, created) = self.ensure_database(&cloud_name).await?;
        let token = match &self.db_token {
            Some(t) => Some(t.clone()),
            None => Some(self.mint_token(&cloud_name).await?),
        };
        // The crate normalizes `libsql://` to `https://`; the database is addressed
        // by this hostname.
        let target = RemoteTarget {
            url: format!("libsql://{hostname}"),
            token,
            created,
        };
        self.cache
            .lock()
            .await
            .insert(db_name.to_owned(), target.clone());
        Ok(target)
    }
}

/// Extract the database hostname from a Turso Platform API response, which looks
/// like `{ "database": { "Name": ..., "Hostname": ... } }`.
async fn parse_hostname(resp: reqwest::Response) -> anyhow::Result<String> {
    let v: serde_json::Value = resp.json().await.context("parse database response")?;
    hostname_from_json(&v).context("Turso Platform API: response missing database hostname")
}

/// Pull the database hostname out of a (possibly `database`-wrapped) JSON value,
/// tolerating `Hostname`/`hostname` casing.
fn hostname_from_json(v: &serde_json::Value) -> Option<String> {
    let db = v.get("database").unwrap_or(v);
    ["Hostname", "hostname"]
        .iter()
        .find_map(|k| db.get(k).and_then(|h| h.as_str()))
        .map(|s| s.to_owned())
}

/// Derive a Turso-Cloud-safe database name from a stateful instance id.
///
/// Cloud names are lowercase `[a-z0-9-]`, bounded length, and unique per org. We
/// build `"{prefix}{slug}-{hash}"`: a lowercased/hyphenated, length-bounded slug of
/// the id plus a stable hash of the *full* id, so distinct instances never collide
/// even when the slug is truncated.
fn cloud_db_name(prefix: &str, id: &str) -> String {
    const MAX_LEN: usize = 54;
    const HASH_HEX: usize = 16;

    let hash = stable_hash_hex(id);
    let slug: String = id
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() {
                c
            } else {
                '-'
            }
        })
        .collect();
    let mut slug = collapse_dashes(&slug);
    let max_slug = MAX_LEN.saturating_sub(prefix.len() + 1 + HASH_HEX);
    if slug.len() > max_slug {
        slug.truncate(max_slug);
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        format!("{prefix}{hash}")
    } else {
        format!("{prefix}{slug}-{hash}")
    }
}

/// Collapse runs of `-` into one.
fn collapse_dashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for c in s.chars() {
        if c == '-' {
            if !prev_dash {
                out.push(c);
            }
            prev_dash = true;
        } else {
            out.push(c);
            prev_dash = false;
        }
    }
    out
}

/// FNV-1a 64-bit, hex-encoded. Stable across runs/versions/platforms (unlike
/// `DefaultHasher`), so an instance always maps to the same Cloud database.
fn stable_hash_hex(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
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
    /// For an instance-scoped creator, the shared cell that the activation-time
    /// warm-up populates and that every connection from this creator reads, so the
    /// first request joins an in-flight open. `None` for the base creator.
    warm: Option<Arc<OnceCell<Arc<TursoConnection>>>>,
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
            warm: None,
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
        // An instance-scoped creator shares one warm-up cell across all its
        // connections (so they join the activation-time open); the base creator
        // gives each connection its own cell (lazy on first use).
        let inner = self
            .warm
            .clone()
            .unwrap_or_else(|| Arc::new(OnceCell::new()));
        Ok(Arc::new(LazyTursoConnection::new(
            self.local_path(),
            Arc::clone(&self.provisioner),
            self.db_name(),
            self.sync_interval,
            inner,
        )))
    }

    fn scoped_to_instance(&self, instance_id: &str) -> Option<Arc<dyn ConnectionCreator>> {
        let mut scoped = self.clone();
        scoped.instance_id = Some(instance_id.to_owned());
        let cell: Arc<OnceCell<Arc<TursoConnection>>> = Arc::new(OnceCell::new());
        scoped.warm = Some(cell.clone());

        // Warm up at activation: provisioning the Cloud database is a one-time
        // control-plane round-trip that must finish before the local synced db can
        // be built (the crate needs the remote URL at build time). Kicking it off
        // here — when the stateful instance is activated, before its first HTTP
        // request — overlaps it with Wasm instantiation, so by the time the guest
        // first opens `instance-db` the connection is ready (or the request simply
        // joins the in-flight open via the shared cell). Best-effort: on failure the
        // first real use retries through the same cell.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let provisioner = Arc::clone(&scoped.provisioner);
            let local_path = scoped.local_path();
            let db_name = scoped.db_name();
            let sync_interval = scoped.sync_interval;
            handle.spawn(async move {
                if let Err(e) = cell
                    .get_or_try_init(|| {
                        build_connection(provisioner, local_path, db_name, sync_interval)
                    })
                    .await
                {
                    tracing::debug!(
                        "Turso instance database warm-up failed (will retry on first use): {e:#}"
                    );
                }
            });
        }
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
    /// The built connection. Shared with the creator's activation-time warm-up
    /// task (see [`TursoConnectionCreator::scoped_to_instance`]) so the first
    /// request joins an already-in-flight open instead of starting it cold.
    inner: Arc<OnceCell<Arc<TursoConnection>>>,
}

impl LazyTursoConnection {
    pub fn new(
        local_path: PathBuf,
        provisioner: Arc<dyn RemoteProvisioner>,
        db_name: String,
        sync_interval: Option<Duration>,
        inner: Arc<OnceCell<Arc<TursoConnection>>>,
    ) -> Self {
        Self {
            local_path,
            provisioner,
            db_name,
            sync_interval,
            inner,
        }
    }

    async fn get_or_create_connection(&self) -> Result<Arc<TursoConnection>, v3::Error> {
        self.inner
            .get_or_try_init(|| {
                build_connection(
                    self.provisioner.clone(),
                    self.local_path.clone(),
                    self.db_name.clone(),
                    self.sync_interval,
                )
            })
            .await
            .map(Arc::clone)
            .map_err(|e| {
                // The guest only sees `InvalidConnection`; log the real cause
                // (provisioning / Platform API / sync errors) so it is diagnosable.
                tracing::error!("Turso instance database setup failed: {e:#}");
                v3::Error::InvalidConnection
            })
    }
}

/// Provision the remote (if needed) and open the per-instance synced database.
/// Shared by the lazy first-use path and the activation-time warm-up.
async fn build_connection(
    provisioner: Arc<dyn RemoteProvisioner>,
    local_path: PathBuf,
    db_name: String,
    sync_interval: Option<Duration>,
) -> anyhow::Result<Arc<TursoConnection>> {
    let local_absent = !local_path.exists();
    let target = provisioner
        .ensure(&db_name)
        .await
        .context("failed to provision remote Turso database")?;
    // Bootstrap (download) from the remote only when restoring a COLD local replica
    // of a PRE-EXISTING database. A just-created remote is empty (push-first), and a
    // present local file is already the source of truth — bootstrapping either would
    // block on the network and, against a not-yet-ready new remote, can even drop the
    // first write (it rebases the fresh local WAL onto the empty remote).
    let bootstrap = local_absent && !target.created;
    let conn = TursoConnection::create(
        local_path,
        target.url,
        target.token,
        sync_interval,
        bootstrap,
    )
    .await
    .context("failed to create Turso synced database")?;
    Ok(Arc::new(conn))
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
        Ok(self
            .get_or_create_connection()
            .await?
            .last_insert_rowid()
            .await)
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
    /// Serializes all access to the turso database. A turso connection/database
    /// cannot be used concurrently ("concurrent use forbidden"), so every query,
    /// execute, and push must hold this gate — otherwise the background push task
    /// races foreground queries and can revert in-flight writes.
    gate: Arc<tokio::sync::Mutex<()>>,
}

impl TursoConnection {
    pub async fn create(
        local_path: PathBuf,
        remote_url: String,
        token: Option<String>,
        sync_interval: Option<Duration>,
        bootstrap: bool,
    ) -> anyhow::Result<Self> {
        if let Some(parent) = local_path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }

        // `bootstrap_if_empty(false)` makes `build()` do **zero network I/O** — it
        // writes only local sync metadata and opens the local file, so the first
        // write hits the local WAL with no dependency on the remote being reachable
        // or ready. We enable bootstrap only to restore a cold local replica from a
        // pre-existing remote (decided by the caller via `build_connection`). All
        // other syncing is explicit (`push` periodically / on suspend); the crate
        // does no implicit background sync.
        let mut builder = turso::sync::Builder::new_remote(&local_path.to_string_lossy())
            .with_remote_url(&remote_url)
            .bootstrap_if_empty(bootstrap);
        if let Some(token) = &token {
            builder = builder.with_auth_token(token);
        }
        let db = Arc::new(builder.build().await?);

        let conn = db.connect().await?;

        let gate = Arc::new(tokio::sync::Mutex::new(()));

        // Periodic background sync while this connection is alive. A `Weak` ref so
        // the task stops (and the database is freed) once the connection is
        // dropped — e.g. when the stateful instance is suspended/evicted. This is
        // push-only: the instance is the single writer, so there is nothing to
        // pull, and pulling would revert local writes not yet pushed. The push
        // holds `gate` so it never overlaps a foreground query.
        if let Some(interval) = sync_interval {
            let db = Arc::downgrade(&db);
            let gate = gate.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(interval);
                ticker.tick().await; // consume the immediate first tick
                loop {
                    ticker.tick().await;
                    let Some(db) = db.upgrade() else { break };
                    let _guard = gate.lock().await;
                    if let Err(e) = db.push().await {
                        tracing::debug!("Turso background push failed: {e}");
                    }
                }
            });
        }

        Ok(Self { db, conn, gate })
    }

    /// Push local changes to the remote. Called by the host on suspend.
    pub async fn push(&self) -> anyhow::Result<()> {
        let _guard = self.gate.lock().await;
        self.db.push().await?;
        Ok(())
    }

    async fn query(
        &self,
        query: &str,
        parameters: Vec<sqlite::Value>,
        max_result_bytes: usize,
    ) -> Result<sqlite::QueryResult, sqlite::Error> {
        let _guard = self.gate.lock().await;
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

        // Held for the whole streaming lifetime (moved into the spawned task
        // below) so no push/other query runs while rows are being drained.
        let guard = self.gate.clone().lock_owned().await;
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
            let _guard = guard; // release the gate only when streaming completes
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
        let _guard = self.gate.lock().await;
        self.conn.execute_batch(statements).await?;
        Ok(())
    }

    async fn changes(&self) -> u64 {
        let _guard = self.gate.lock().await;
        // turso::Connection does not expose a `changes()` accessor, so use the
        // SQLite builtin (a `SELECT` does not reset it).
        self.scalar_i64("SELECT changes()")
            .await
            .unwrap_or(0)
            .max(0) as u64
    }

    async fn last_insert_rowid(&self) -> i64 {
        let _guard = self.gate.lock().await;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn is_valid_cloud_name(s: &str) -> bool {
        !s.is_empty()
            && s.len() <= 54
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !s.starts_with('-')
            && !s.ends_with('-')
            && !s.contains("--")
    }

    #[test]
    fn cloud_name_is_valid_and_stable() {
        let a = cloud_db_name("spin-", "todo/groceries");
        let b = cloud_db_name("spin-", "todo/groceries");
        assert_eq!(a, b, "must be stable across calls");
        assert!(is_valid_cloud_name(&a), "invalid name: {a}");
        assert!(a.starts_with("spin-todo-groceries-"));
    }

    #[test]
    fn distinct_ids_give_distinct_names() {
        let a = cloud_db_name("spin-", "todo/groceries");
        let b = cloud_db_name("spin-", "todo/work");
        assert_ne!(a, b);
    }

    #[test]
    fn long_ids_are_bounded_and_collision_free() {
        let a = cloud_db_name("spin-", &format!("comp/{}", "x".repeat(200)));
        let b = cloud_db_name("spin-", &format!("comp/{}", "y".repeat(200)));
        assert!(a.len() <= 54, "too long: {} ({})", a, a.len());
        assert!(is_valid_cloud_name(&a), "invalid: {a}");
        // Same truncated slug, but the hash of the full id differs → no collision.
        assert_ne!(a, b);
    }

    #[test]
    fn sanitizes_invalid_chars() {
        let n = cloud_db_name("spin-", "Foo_Bar/Baz!");
        assert!(is_valid_cloud_name(&n), "invalid: {n}");
    }

    #[test]
    fn collapse_dashes_collapses_runs() {
        assert_eq!(collapse_dashes("a--b---c"), "a-b-c");
        assert_eq!(collapse_dashes("--x--"), "-x-");
    }

    #[test]
    fn parses_hostname_from_platform_response() {
        let wrapped = serde_json::json!({
            "database": { "Name": "x", "Hostname": "x-org.aws-us-east-1.turso.io" }
        });
        assert_eq!(
            hostname_from_json(&wrapped).as_deref(),
            Some("x-org.aws-us-east-1.turso.io")
        );
        let flat = serde_json::json!({ "hostname": "y.turso.io" });
        assert_eq!(hostname_from_json(&flat).as_deref(), Some("y.turso.io"));
        let missing = serde_json::json!({ "database": { "Name": "x" } });
        assert_eq!(hostname_from_json(&missing), None);
    }
}
