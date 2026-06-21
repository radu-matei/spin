# Stateful Components and the SQLite‑database‑per‑instance model

> Status: design + implementation reference for the `feat/stateful-turso-sync` line of work.
> Covers the Wasm‑level execution model, per‑instance state isolation, the SQLite
> backend abstraction, and the Turso local‑first sync integration (conceptual and
> implementation). File references point at the Spin host repo unless prefixed with
> `spin-rust-sdk/`.

---

## 1. What a stateful component is

An ordinary Spin HTTP component is **stateless and request‑scoped**: the host
instantiates a fresh Wasm instance per request (or pulls one from a pool), runs the
handler, and tears it down. Nothing in linear memory survives between requests.

A **stateful component** is the opposite: a **long‑lived, addressable Wasm instance**.
The host keeps one live instance per `(component_id, instance_id)` pair, activates it
once, routes many requests to it over time, and only tears it down after an idle
timeout. In‑memory state (anything in the guest's linear memory — a cached database
handle, an in‑RAM index, a counter) persists across requests for the life of that
instance.

This is Spin's "**durable object**" primitive: a named, single‑writer, in‑memory actor
with a lifecycle, reachable by id. The canonical pairing is **one stateful instance =
one private database** (covered in Parts 3–4).

Three properties define the model:

| Property | Meaning |
|---|---|
| **Addressable by id** | Reached at `spin.alt/component/<component>/<instance>/<path>`. The instance id is arbitrary (`groceries`, `user-42`, a UUID). |
| **Long‑lived** | Activated once via `lifecycle.instantiate(id)`, kept warm, suspended via `lifecycle.suspend()` after `--stateful-idle-timeout`. |
| **Single‑writer** | At most one live instance per `(component, instance)` per process, so the guest never races itself. |

Stateful components are **not publicly routable**. They are only reachable from another
component via the `spin.alt` loopback (Part 2.4), so a normal component (a router /
front controller) is the public face.

---

## 2. How it works at the Wasm level

### 2.1 The WIT contract

A stateful component exports everything an ordinary wasip3 HTTP component exports **plus
two lifecycle hooks**. The hooks live in a vendored WIT package
`spin:stateful-component@0.1.0`:

```wit
// wit/deps/spin-stateful-component@0.1.0/lifecycle.wit
package spin:stateful-component@0.1.0;

interface lifecycle {
  instantiate: func(id: string);
  suspend: func();
}
```

- `instantiate(id)` — called **once**, when the instance is first activated, receiving
  its unique instance id so the guest can key/restore its state.
- `suspend()` — called **before** the instance is dropped (idle timeout or migration), so
  the guest can flush in‑memory state.

The dedicated guest world composes the lifecycle export with the wasip3 HTTP handler:

```wit
// wit/world.wit  (package spin:up@4.0.0)
world stateful-http-trigger {
  include platform;
  export spin:stateful-component/lifecycle@0.1.0;
  export wasi:http/handler@0.3.0-rc-2026-03-15;
}
```

So **a stateful component is exactly an `http-trigger` guest plus the two lifecycle
exports**. Note the HTTP surface is **wasip3** (`wasi:http/handler@0.3.0`, the async
`call_handle` `Service` model) — not the wasip2 `wasi:http/incoming-handler`. This
matters because the worker drives the guest inside `store.run_concurrent` (§2.5).

### 2.2 Declaring one in the manifest

A component opts in with a single field in the v2 manifest:

```toml
[component.todo]
source = "..."
stateful = true                       # <- opt in
sqlite_databases = ["instance-db"]    # the well-known per-instance DB label
```

`stateful` is a plain bool on the manifest `Component`
([crates/manifest/src/schema/v2.rs](crates/manifest/src/schema/v2.rs):539), defaulting to
`false` and omitted when false. The loader copies it into locked‑app metadata under
`STATEFUL_KEY = "stateful"`
([crates/loader/src/local.rs](crates/loader/src/local.rs):180,
[crates/locked-app/src/lib.rs](crates/locked-app/src/lib.rs):26). At request time the host
**hard‑rejects** any non‑stateful component from being addressed via `spin.alt`:

```rust
// crates/trigger-http/src/stateful.rs  (ensure_stateful)
let is_stateful = component.get_metadata(spin_locked_app::STATEFUL_KEY)?.unwrap_or(false);
anyhow::ensure!(is_stateful,
    "component {component_id:?} is not a stateful component and cannot be addressed via spin.alt");
```

### 2.3 Authoring with the Rust SDK

Two proc‑macros from `spin-sdk-macro` cooperate. A stateful component needs **both**.

**`#[stateful_component]`** annotates a struct and generates:

1. a private singleton `static __<STRUCT>_INSTANCE: async_lock::RwLock<Option<T>>` (the
   one place in‑memory state lives);
2. the `spin:stateful-component/lifecycle@0.1.0` guest export, wiring `instantiate`/
   `suspend` to your struct;
3. `T::get()` / `T::get_mut()` async accessors that lock the singleton.

The generated lifecycle glue:

```rust
// spin-rust-sdk/crates/spin-sdk-macro/src/lib.rs  (generated)
fn instantiate(id: ::std::string::String) {
    let instance = super::Counter::instantiate(id);
    let mut lock = super::__COUNTER_INSTANCE.try_write()
        .expect("stateful instance must not be concurrently instantiated");
    *lock = Some(instance);
}
fn suspend() {
    let mut lock = super::__COUNTER_INSTANCE.try_write()
        .expect("stateful instance must not be suspended while in use");
    if let Some(instance) = lock.take() { instance.suspend(); }
}
```

**`#[http_service]`** annotates an `async fn` and generates the wasip3
`wasi:http/handler` guest export (it requires `async`):

```rust
// generated handle(): convert wasip3 Request -> SDK Request, run the user fn, convert back
async fn handle(request: Request) -> Result<Response, ErrorCode> {
    let request = http::Request::from_request(request)?;
    http::IntoResponse::into_response(super::handle_todo(request).await)
}
```

**Holding state across requests.** The struct in the `RwLock` is the durable in‑memory
state. The cookbook pattern is to cache the per‑instance SQLite handle and lazily run the
schema once:

```rust
// spin-rust-sdk/examples/stateful-sqlite/counter/src/lib.rs
#[stateful_component]
struct Counter { db: Option<Connection> }

impl Counter {
    fn instantiate(_id: String) -> Self { Self { db: None } }
    fn suspend(&self) {}
    async fn connection(&mut self) -> Result<&Connection, spin_sdk::sqlite::Error> {
        if self.db.is_none() {
            let conn = Connection::open("instance-db").await?;  // the well-known label
            conn.execute(SCHEMA, []).await?;
            self.db = Some(conn);
        }
        Ok(self.db.as_ref().unwrap())
    }
}

#[http_service]
async fn handle(req: Request) -> Response { /* Counter::get_mut().await ... */ }
```

Because the instance is activated once and lives until suspend, the cached `Connection`
(and any other field) is reused by every request that calls `Counter::get_mut().await`.
The guest code is **identical across all instances** — the per‑instance database is host
magic (Part 2.6 / Part 3).

### 2.4 Addressing and the `spin.alt` loopback

A public component reaches a stateful one by sending an **outbound HTTP request** to a
special host:

```rust
// spin-rust-sdk/examples/stateful-sqlite/router/src/lib.rs
let url = format!("https://spin.alt/component/{component}/{instance}/{op}{query}");
let outgoing = Request::builder().method(req.method().clone()).uri(url).body(EmptyBody::new())?;
Ok(send(outgoing).await?)
```

The caller must allow the host: `allowed_outbound_hosts = ["https://spin.alt"]`.

`spin.alt` is **not a network destination** — it is intercepted host‑side and turned into
a loopback into the target instance. On the outbound path, `OutboundHttpInterceptor`
matches the magic host, parses the address, rewrites the URI so the guest sees only the
relative path, and routes into the stateful manager:

```rust
// crates/outbound-networking-config/src/allowed_hosts.rs  (STATEFUL_DOMAIN = "spin.alt")
let path = url.path().trim_start_matches('/').strip_prefix("component/")?;
let (component_id, rest) = path.split_once('/')?;
let (instance_id, remaining) = rest.split_once('/').unwrap_or((rest, ""));
Some((component_id.into(), instance_id.into(), format!("/{remaining}")))
```

```rust
// crates/trigger-http/src/outbound_http.rs  (intercept)
if let Some((component_id, instance_id, path)) = parse_stateful_target(request.uri()) {
    // ... rewrite URI to `path` (+ original query) ...
    let resp = self.server.handle_stateful_request(req, &component_id, &instance_id).await?;
    return Ok(InterceptOutcome::Complete(resp));
}
```

So the addressing scheme is purely a host convention; the SDK only ever builds a URL
string.

### 2.5 The host worker

`StatefulInstanceManager` ([crates/trigger-http/src/stateful.rs](crates/trigger-http/src/stateful.rs))
owns the live instances:

- `workers: RwLock<HashMap<(component_id, instance_id), StatefulWorker>>` — the live set.
- A `StatefulWorker` is just a **handle**: a generation `id`, a bounded
  `mpsc::Sender<HttpTask>` (capacity 64), a shared `last_activity: Instant`, and an
  `in_flight` counter. The actual Wasm `Store` + `Instance` live on a **detached tokio
  task**, reachable only through the channel.
- Caps: `DEFAULT_MAX_INSTANCES = 1024`, `WORKER_CHANNEL_CAPACITY = 64`. Only the idle
  timeout is CLI‑configurable (`--stateful-idle-timeout`, default `300s`).

**Dispatch.** `handle_request` builds the key and takes a fast path under a read lock: it
finds the worker, reserves an `InFlightGuard` (so the idle checker can't evict a worker
about to be used), then `dispatch`es — packing the request, a `oneshot` response channel,
the in‑flight guard, and the **caller's tracing span** into an `HttpTask` and sending it
over the bounded channel (this is also the backpressure point). On a miss, the slow path
runs `ensure_stateful`, evicts the LRU idle worker if at the cap, and `spawn_worker`s a
fresh one, all wrapped in a bounded retry so a worker that self‑evicts between the
liveness check and the send is recreated once.

**Worker lifecycle** (`run_stateful_worker`), in order:

```rust
// 1. prepare the store and scope per-instance factor state to this (component, instance)
let mut builder = trigger_app.prepare(component_id)?;
if let Some(kv) = builder.factor_builder::<KeyValueFactor>() {
    kv.set_instance_id(format!("{component_id}/{instance_id}"));
}
if let Some(sq) = builder.factor_builder::<SqliteFactor>() {
    sq.set_instance_id(format!("{component_id}/{instance_id}"));
}
let mut store = builder.instantiate_store(())?.into_inner();

// 2. instantiate the Wasm component
let instance = pre.instantiate_async(&mut store).await?;

// 3. resolve lifecycle exports by name (interface, then instantiate, then suspend)
let lifecycle_idx   = instance.get_export_index(&mut store, None, LIFECYCLE_EXPORT)?;
let instantiate_idx = instance.get_export_index(&mut store, Some(&lifecycle_idx), "instantiate")?;
let suspend_idx     = instance.get_export_index(&mut store, Some(&lifecycle_idx), "suspend")?;
let instantiate_func = instance.get_typed_func::<(&str,), ()>(&mut store, &instantiate_idx)?;
let suspend_func     = instance.get_typed_func::<(), ()>(&mut store, &suspend_idx)?;

// 4. activate: lifecycle::instantiate(instance_id)
instantiate_func.call_async(&mut store, (instance_id,)).await?;

// 5. load the wasip3 wasi:http/handler Service from the SAME instance
let service = ServiceIndices::new(pre)?.load(&mut store, &instance)?;

// 6. serve requests until the channel closes, then suspend()
let run_result = store.run_concurrent(async |accessor| {
    accessor.spawn(receiver);   // ReceiverTask drains the mpsc channel
    let _ = shutdown_rx.await;  // unblocked when the channel closes
    Ok(())
}).await;
suspend_func.call_async(&mut store, ()).await?;

// 7. flush local-first SQLite (push to remote) before dropping the store
for conn in sqlite_state.connections_to_sync() { let _ = conn.sync().await; }
```

Two subtleties:

- **Why a spawned `ReceiverTask` instead of polling the channel in the main closure.**
  Inside `run_concurrent` the accessor scheduler does not re‑poll the main closure when a
  tokio waker fires while other spawned tasks are in flight, but it *does* schedule spawned
  tasks against each other. Making the receiver a spawned task guarantees new requests are
  picked up even while in‑flight handlers await WASI timers.
- **Per‑request execution.** Each `HttpTask` becomes a spawned `HandleRequestTask` that
  opens an `execute_wasm` tracing span parented to the **caller's** span (carried across
  the channel), converts the inbound p2 hyper body to a p3 request, calls
  `service.wasi_http_handler().call_handle(accessor, request_handle).await`, converts the
  response back, and holds the `InFlightGuard` until the response body has fully streamed
  (`NotifyOnDropBody`) — so eviction can never tear an instance down mid‑stream.

**Idle timeout & eviction.** A 10 s ticker marks a worker idle iff `in_flight == 0` **and**
`last_activity.elapsed() > idle_timeout`, re‑checks under the write lock, and `remove`s it.
Suspension is driven **entirely by dropping the worker handle**: dropping `task_tx` closes
the channel → the `ReceiverTask` loop ends → fires the shutdown oneshot → `run_concurrent`
returns → `suspend()` + SQLite flush run. LRU eviction and self‑eviction‑on‑trap use the
same drop‑to‑close path, and never pick a worker with `in_flight > 0`.

### 2.6 End‑to‑end request flow

```
  client ── HTTP ──▶ router (public, stateless)
                       │  send  https://spin.alt/component/todo/groceries/add?text=Milk
                       ▼
        OutboundHttpInterceptor.intercept           crates/trigger-http/src/outbound_http.rs
          parse_stateful_target ─▶ (todo, groceries, /add?text=Milk)
          rewrite URI ─▶ /add?text=Milk
                       ▼
        HttpServer.handle_stateful_request ─▶ StatefulInstanceManager.handle_request
                       │  find-or-spawn worker keyed (todo, groceries); reserve InFlightGuard
                       ▼
        dispatch: HttpTask ──(bounded mpsc)──▶ worker task
                       ▼
        run_stateful_worker (warm instance of `todo`/`groceries`)
          ReceiverTask.recv ─▶ accessor.spawn(HandleRequestTask)
          service.wasi_http_handler().call_handle(...)   ◀── guest #[http_service] runs
            guest: Todo::get_mut().await ─▶ cached Connection.open("instance-db")
                   INSERT ... ; SELECT ...                ◀── per-instance database
          response ──(oneshot)──▶ dispatch ──▶ interceptor ──▶ router ──▶ client
                       ⋮
        (idle 300s) ─▶ channel closes ─▶ suspend() ─▶ Connection.sync() (push to remote)
```

---

## 3. Per‑instance state isolation

Two instances of the same component must never see each other's mutable state. Spin
isolates state **two different ways** depending on the backend, both keyed off the same
scope string `"{component_id}/{instance_id}"` set by the worker at activation
([stateful.rs](crates/trigger-http/src/stateful.rs):522).

### 3.1 Key‑value: prefix one shared store

For key‑value, isolation is **key‑prefixing inside a single shared store**. When the
worker sets an instance id, `KeyValueFactor`'s builder wraps the store manager in an
`InstanceScopedStoreManager`, which wraps **only** the well‑known `instance-store` label in
an `InstanceScopedStore`:

```rust
// crates/factor-key-value/src/util.rs
async fn get(&self, name: &str) -> Result<Arc<dyn Store>, Error> {
    let store = self.inner.get(name).await?;
    if name == INSTANCE_STORE_LABEL {
        Ok(Arc::new(InstanceScopedStore { inner: store, prefix: format!("{}/", self.instance_id) }))
    } else { Ok(store) }   // any other KV label passes through unwrapped
}
```

`InstanceScopedStore` transparently prepends `"{component}/{instance}/"` to every key on
write and strips it on reads, and filters listing/bulk ops to this instance's prefix. Net
effect: **one physical store, many logical namespaces** partitioned by key prefix.

### 3.2 SQLite: a separate database per instance

For SQLite there is no SQL analogue of key‑prefixing, so isolation is a **wholly separate
database** for the well‑known `instance-db` label. The factor's per‑instance state holds a
`label -> Arc<dyn ConnectionCreator>` map; `set_instance_id` rebinds the `instance-db`
entry to a per‑instance creator:

```rust
// crates/factor-sqlite/src/host.rs
pub fn set_instance_id(&mut self, instance_id: String) {
    if let Some(creator) = self.connection_creators.get(crate::INSTANCE_DB_LABEL) {
        if let Some(scoped) = creator.scoped_to_instance(&instance_id) {
            self.connection_creators.insert(crate::INSTANCE_DB_LABEL.to_owned(), scoped);
        }
    }
}
```

`ConnectionCreator::scoped_to_instance` **defaults to `None`** — most backends can't
provide a separate per‑instance DB, so the call is a silent no‑op and the instance keeps
the shared creator. Backends that *can* (in‑process file, Turso) override it. The contrast
is explicit in the trait doc:

```rust
// crates/factor-sqlite/src/lib.rs
/// Unlike key-value (which isolates instances by key-prefixing one shared
/// store), a per-instance SQLite database is a *separate* database. Backends
/// that can provide one ... override this. The default returns `None`.
fn scoped_to_instance(&self, instance_id: &str) -> Option<Arc<dyn ConnectionCreator>> { None }
```

### 3.3 The `Connection` abstraction and push‑on‑suspend

Backends sit behind two `Arc<dyn ...>` traits in
[crates/factor-sqlite/src/lib.rs](crates/factor-sqlite/src/lib.rs):

- **`ConnectionCreator`** — `create_connection(label) -> Arc<dyn Connection>` plus the
  optional `scoped_to_instance`.
- **`Connection`** — `query` / `query_async` / `execute_batch` / `changes` /
  `last_insert_rowid`, plus two optional hooks: `summary()` (human‑readable description)
  and **`sync()`** (flush locally‑buffered changes to a durable/remote backend; default
  no‑op).

Because a per‑instance database may be **local‑first** (writes buffered locally, pushed to
a remote lazily), the worker must flush before dropping the instance. The host can't
iterate the wasmtime resource table, so `InstanceState` records every opened connection in
an `open_connections: Vec` and exposes `connections_to_sync()` (clones, so the worker can
await outside the instance‑state borrow). **After** `suspend()` returns, the worker calls
`conn.sync().await` on each — flushing even writes made *inside* `suspend()`:

```rust
// crates/trigger-http/src/stateful.rs
let to_sync = store.data_mut().factors_instance_state_mut()
    .get::<SqliteFactor>().map(|s| s.connections_to_sync()).unwrap_or_default();
for conn in to_sync { let _ = conn.sync().await; }   // push to remote
```

---

## 4. The SQLite backend abstraction and dispatch

Which concrete backend a database **label** gets is decided at runtime‑config resolution
time by a `type` string. `RuntimeConfigResolver::get_connection_creator`
([crates/sqlite/src/lib.rs](crates/sqlite/src/lib.rs):91) is the central dispatch:

```rust
match config.type_.as_str() {
    "spin"   => /* InProcConnectionCreator (rusqlite, local file/memory) */,
    "libsql" => /* LazyLibSqlConnection (remote libSQL over HTTP)        */,
    "turso"  => /* TursoConnectionCreator (local-first synced)           */,
    _ => anyhow::bail!("Unknown database kind: {database_kind}"),
}
```

Each arm re‑deserializes the flattened TOML into a backend struct (`InProcDatabase`,
`LibSqlDatabase`, `TursoDatabase`). The factor validates that every label a component
declares in `sqlite_databases` has a configured creator, injects an in‑process `default`
creator if the user didn't define one, and clones the map into each instance. One app can
freely mix backends across labels.

How each backend handles **per‑instance scoping**:

| Backend (`type`) | `scoped_to_instance` | Per‑instance unit |
|---|---|---|
| `spin` (in‑proc rusqlite) | overridden, file‑backed only | a separate **on‑disk file** `<base-without-ext>/<sanitized-instance>.db` |
| `libsql` (remote) | **default `None`** | none — all instances share the one remote DB |
| `turso` (local‑first sync) | overridden | a separate **local file _and_ remote database** per instance |

The in‑process backend's scoping (no networking, just a per‑instance file):

```rust
// crates/sqlite-inproc/src/lib.rs
fn location(&self) -> anyhow::Result<InProcDatabaseLocation> {
    match (&self.path, &self.instance_id) {
        (Some(base), Some(id)) => {
            let dir  = base.with_extension("");                 // strip .db -> dir
            let file = dir.join(format!("{}.db", sanitize_instance(id)));
            InProcDatabaseLocation::from_path(Some(file))
        }
        (Some(base), None) => InProcDatabaseLocation::from_path(Some(base.clone())),
        (None, _)          => InProcDatabaseLocation::from_path(None),  // in-memory: cannot scope
    }
}
```

This is what makes the example app work **locally with no server** (`type = "spin"`): each
instance simply gets its own file under `.spin/`.

---

## 5. Turso: a synced SQLite database per instance

### 5.1 Concept

[Turso](https://github.com/tursodatabase/turso) is the from‑scratch Rust rewrite of
SQLite. Its **sync** feature gives a database **local‑first offline sync**: all reads and
writes hit a local SQLite file; changes are reconciled with a remote ("hosted") database by
explicit `push`/`pull`. The `spin-sqlite-turso` backend uses this to give each stateful
instance its **own** local file kept in sync with its **own** remote database — the SQLite
analogue of the per‑instance key‑value store, but durable beyond the local disk.

Why a database *per instance* requires a multi‑tenant remote: **Turso addresses a database
by its URL host (subdomain), never by a path.** A single‑database server (`tursodb
--sync-server`) therefore cannot route per instance — every instance would share the one
remote. A remote *per* instance needs a multi‑tenant control plane, which today means
**Turso Cloud**. Local files stay per‑instance regardless of backend.

> ⚠️ **Beta.** Turso offline sync currently has **no durability guarantees** and implements
> conflict *detection* only (no resolution). This backend is opt‑in and intended as a
> forward‑looking prototype, not production. The production‑ready alternative today is
> libSQL embedded replicas; the backend is isolated behind the `Connection` trait so the
> engine can be swapped.

### 5.2 The turso crate sync model (what drives the design)

From reading `turso 0.7.0-pre.10` (`~/.cargo/registry/src/*/turso-0.7.0-pre.10/src/sync.rs`):

- **`sync::Builder::new_remote(path).with_remote_url(url).with_auth_token(t).build()`** —
  builds the synced `Database`. With the default `bootstrap_if_empty(true)`, `build()` does
  a **network bootstrap** (downloads the DB); against a brand‑new, not‑yet‑ready remote it
  can even *error*. With **`bootstrap_if_empty(false)` it does ZERO network I/O** — it only
  writes local sync metadata and opens the local file. This is the non‑blocking seam.
- **`Database::connect()`** — local‑only, no network.
- **`push()` / `pull()`** — the **only** network operations, and they are **entirely
  caller‑driven**. There is no implicit background sync; the backend orchestrates it.
- **No API to attach a remote after `build()`** — the remote URL must be known at build
  time. This single constraint shapes the whole connection lifecycle (§5.5).
- The Cloud hostname is deterministic: **`{db}-{org}.{region}.turso.io`** — which is what
  makes hostname *prediction* possible (§5.4).

### 5.3 Provisioning the remote

The backend obtains each instance's remote through a `RemoteProvisioner`
([crates/sqlite-turso/src/lib.rs](crates/sqlite-turso/src/lib.rs)):

```rust
pub struct RemoteTarget { pub url: String, pub token: Option<String>, pub created: bool }

#[async_trait]
pub trait RemoteProvisioner: Send + Sync {
    async fn ensure(&self, db_name: &str) -> anyhow::Result<RemoteTarget>;      // create-or-fetch (blocking)
    async fn predicted(&self, _db_name: &str) -> Option<RemoteTarget> { None }  // zero-create, for the fast path
}
```

The `created` flag is load‑bearing: **`true`** means this call just created the remote (it
is empty, the local replica is the source of truth → *push‑first*, never bootstrap);
**`false`** means it already existed (a cold local replica should *bootstrap/restore* from
it).

Two implementations:

- **`AutoCreateProvisioner`** (`provision = "auto"`) — targets a **single** remote at a
  fixed `base_url` (one `tursodb --sync-server`). `ensure` ignores the db name and returns
  that one url with `created = false`. **Not per‑instance** (single‑db server can't route
  by host); only for single‑instance smoke tests.
- **`TursoPlatformProvisioner`** (`provision = "platform"`) — a database **per instance** on
  Turso Cloud via the Platform API. `ensure`:
  - `POST /v1/organizations/{org}/databases {name, group}` (bearer `api_token`). On success
    it returns `(hostname, created=true)`; on **409/400** the DB already exists, so it
    `GET`s the database and returns `(hostname, created=false)`:

    ```rust
    if status.is_success() { return Ok((parse_hostname(resp).await?, true)); }
    if status.as_u16() == 409 || status.as_u16() == 400 {
        return Ok((self.get_database_hostname(name).await?, false));
    }
    ```
  - reads the **real `Hostname`** from the API response (Cloud hostnames include a region,
    so a static template would be unreliable) and builds the sync URL `libsql://{hostname}`
    (the crate normalizes `libsql://` → `https://`);
  - resolves the sync token: the configured **group `db_token`** (one token authenticates
    every DB in the group — ideal for many per‑instance DBs), else a db‑scoped token minted
    via `POST .../databases/{name}/auth/tokens`;
  - caches the resolved `RemoteTarget` per instance db name.

**Cloud‑safe naming.** `cloud_db_name(prefix, id)` derives a valid Cloud name
`"{prefix}{slug}-{hash}"`: lowercase, every non‑alphanumeric → `-`, runs of `-` collapsed,
length‑bounded to 54, with a short **stable hash** appended. The hash is **FNV‑1a** (not
`DefaultHasher`, which is not stable across versions) computed over the *full* id, so
distinct instances never collide even when the slug is truncated, and an instance always
maps to the same Cloud database:

```rust
fn stable_hash_hex(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() { h ^= b as u64; h = h.wrapping_mul(0x0000_0100_0000_01b3); }
    format!("{h:016x}")
}
```

### 5.4 Hostname prediction (the instant‑first‑write path)

The crate needs the remote URL at `build()` time, and on Turso Cloud the hostname is only
known *after* the create‑POST. Naively, the first write to a brand‑new instance would block
~1.8 s on Cloud database creation. The backend avoids this by **predicting** the hostname
instead of creating the database first:

- The Cloud hostname is `{db}-{org}.{region}.turso.io`, where the region is the **group's
  primary location** — identical for every per‑instance DB (they all live in one group). So
  `region()` does a single, cached `GET …/groups/{group}` → `group.primary`, and
  `predicted_hostname` formats `{cloud_name}-{org}.{region}.turso.io`.
- `predicted()` returns this target **without any create call** (requires a group
  `db_token`, since a db‑scoped token can't be minted before the DB exists). The first
  region lookup touches the network once and is cached; subsequent instances are fully
  local.

This is verified against the assigned hostname after the background create, and the URL
mismatch is logged if Turso ever changes the format.

### 5.5 The connection lifecycle

```
scoped_to_instance("todo/groceries")            ← worker activation (before first request)
   │  create per-instance shared OnceCell  (warm)
   └─ spawn warm-up ───────────────┐  (overlaps Wasm instantiation)
                                    ▼
  build_connection(provisioner, local_path, db_name, sync_interval)
     ├─ FAST path: predicted() = Some
     │     TursoConnection::create(url, bootstrap = false)   ← ZERO network, local-speed write
     │     spawn_remote_setup(...)  ── background ──▶ ensure() (create the Cloud DB)
     │                                                └─ if cold replica of existing DB: restore() (pull)
     └─ SLOW path: predicted() = None
           target = ensure()                       ← blocking create-or-fetch
           bootstrap = local_absent && !target.created
           TursoConnection::create(url, bootstrap)
```

- **`TursoConnectionCreator`** carries `local_dir`, the `provisioner`, a `sync_interval`,
  the `instance_id`, and a `warm: Option<Arc<OnceCell<Arc<TursoConnection>>>>`. When the
  worker calls `scoped_to_instance`, it creates a fresh shared `OnceCell` and — if a tokio
  runtime is present — **spawns an activation‑time warm‑up** that runs `build_connection`
  through the shared cell. This overlaps the one‑time Cloud provisioning with Wasm
  instantiation, so by the time the guest first opens `instance-db`, the connection is ready
  (or the first request simply joins the in‑flight open via the cell). Every connection the
  creator hands out shares that one cell.

- **`LazyTursoConnection`** is the `Connection` the guest gets. Its `inner` is the shared
  `Arc<OnceCell<Arc<TursoConnection>>>`; `get_or_create_connection` does
  `inner.get_or_try_init(build_connection)`, joining the warm‑up. Setup errors are surfaced
  to the guest only as `InvalidConnection`, but the real cause is logged at `error` level so
  failures stay diagnosable. `sync()` (push‑on‑suspend) pushes **only if** the db was
  actually opened.

- **`build_connection`** chooses the fast or slow path. Fast: open local‑first against the
  predicted URL with `bootstrap_if_empty(false)` (zero network → local‑speed first write),
  then `spawn_remote_setup` to create the remote in the background. Slow: `ensure()` first,
  then bootstrap only to restore a cold replica of a pre‑existing DB
  (`bootstrap = local_absent && !created`).

```rust
// crates/sqlite-turso/src/lib.rs  (build_connection, fast path)
if let Some(target) = provisioner.predicted(&db_name).await {
    let conn = Arc::new(TursoConnection::create(
        local_path, target.url.clone(), target.token, sync_interval, /* bootstrap = */ false).await?);
    spawn_remote_setup(provisioner, db_name, target.url, local_absent, conn.clone());
    return Ok(conn);
}
```

- **`spawn_remote_setup`** runs in the background: call `ensure()` to actually create/confirm
  the Cloud DB (so a later `push` has a target); if the predicted URL differs from the real
  one, log an error; and if this was a **cold local replica of a database that already
  existed** (`local_absent && !real.created`), call `conn.restore()` to pull. This is safe
  because a just‑opened replica has no local writes to lose, a brand‑new DB has nothing to
  pull, and **`push()` of an empty local is a no‑op**, so the remote is never clobbered
  before a restore lands.

### 5.6 The synced connection: build, serialization, sync timing

`TursoConnection` holds `db: Arc<turso::sync::Database>`, `conn: turso::Connection`, and a
**`gate: Arc<tokio::sync::Mutex<()>>`**. `create`:

```rust
let mut builder = turso::sync::Builder::new_remote(&local_path.to_string_lossy())
    .with_remote_url(&remote_url)
    .bootstrap_if_empty(bootstrap);
if let Some(token) = &token { builder = builder.with_auth_token(token); }
let db   = Arc::new(builder.build().await?);
let conn = db.connect().await?;
// periodic, push-only background task holding a Weak ref (self-terminates on drop):
if let Some(interval) = sync_interval {
    let db = Arc::downgrade(&db); let gate = gate.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.tick().await;                       // consume the immediate first tick
        loop {
            ticker.tick().await;
            let Some(db) = db.upgrade() else { break };  // connection dropped -> stop, free db
            let _guard = gate.lock().await;
            if let Err(e) = db.push().await { tracing::debug!("Turso background push failed: {e}"); }
        }
    });
}
```

**Concurrency — the gate is mandatory.** A turso connection/database **cannot be used
concurrently** ("concurrent use forbidden"). Without serialization, the background `push`
races foreground queries and can revert in‑flight writes. So **every** `query` /
`execute_batch` / `changes` / `last_insert_rowid` / `push` / `restore` holds the gate.
Streaming `query_async` takes an **owned** guard and moves it into the row‑draining task, so
the gate is held for the whole streaming lifetime, not just the call.

**Sync timing** — three triggers, all explicit:

| Trigger | Action | Why |
|---|---|---|
| **On open (bootstrap)** | `build()` with `bootstrap_if_empty(true)` | only to restore a cold replica of a pre‑existing remote |
| **Periodic** | `push()` every `sync_interval_seconds` | flush local writes; **push‑only** (single writer → nothing to pull, and pulling would revert un‑pushed writes) |
| **On suspend** | `Connection::sync()` → `push()` | durable flush before the instance is dropped (Part 3.3) |

The background task holds only a `Weak` ref, so it stops and frees the database the moment
the connection is dropped (on suspend/eviction).

### 5.7 Durability and restore semantics

- **Brand‑new instance** — fast path opens local‑first (no network), first write is
  local‑speed; the Cloud DB is created in the background; periodic/suspend `push` carries
  the data up. Verified: brand‑new first write ≈ 0.14 s with the region cached (≈ 0.66 s for
  the first instance that warms it), then ≈ 12 ms; vs ≈ 1.8 s before prediction.
- **Re‑activation** (local file present) — local is authoritative; no bootstrap, no
  restore; push carries new writes up.
- **Cold restore** (local wiped, remote has data) — `spawn_remote_setup` detects the DB
  already existed (`created = false`) and triggers a background `restore()` (pull). This is
  now **eventual**: the very first read after a cold wipe may briefly see empty before the
  restore lands, and a write in that narrow window risks a turso conflict (beta).

### 5.8 Configuration

Per‑instance multi‑DB on **Turso Cloud** (`provision = "platform"`):

```toml
[sqlite_database.instance-db]
type = "turso"
provision = "platform"
org = "<your-org>"                 # org slug; region is read from the group
group = "<your-group>"             # all per-instance DBs go here (defines the region)
api_token = "<org/platform token>" # creates databases + mints tokens
db_token  = "<group sync token>"   # syncs every DB in the group (enables the fast path)
sync_interval_seconds = 5
# name_prefix = "spin-"            # optional; namespaces derived Cloud names
```

Single‑DB **local** smoke test (one instance only; the new protocol is served by
`tursodb --sync-server`, **not** the legacy `turso dev`):

```toml
[sqlite_database.instance-db]
type = "turso"
provision = "auto"
url = "http://127.0.0.1:8080"
sync_interval_seconds = 5
```

Local, **no server** (per‑instance files via the in‑process backend):

```toml
[sqlite_database.instance-db]
type = "spin"
path = ".spin/instance-db.db"      # -> .spin/instance-db/<component>_<instance>.db per instance
```

---

## 6. Reference

### Key constants and labels

| Name | Value | Where |
|---|---|---|
| `LIFECYCLE_EXPORT` | `spin:stateful-component/lifecycle@0.1.0` | [stateful.rs](crates/trigger-http/src/stateful.rs):27 |
| `STATEFUL_KEY` | `stateful` (locked‑app metadata) | [locked-app/src/lib.rs](crates/locked-app/src/lib.rs):26 |
| `STATEFUL_DOMAIN` | `spin.alt` | [allowed_hosts.rs](crates/outbound-networking-config/src/allowed_hosts.rs):15 |
| `INSTANCE_STORE_LABEL` | `instance-store` (KV) | [factor-key-value/src/util.rs](crates/factor-key-value/src/util.rs) |
| `INSTANCE_DB_LABEL` | `instance-db` (SQLite) | [factor-sqlite/src/lib.rs](crates/factor-sqlite/src/lib.rs) |
| `DEFAULT_MAX_INSTANCES` | `1024` | [stateful.rs](crates/trigger-http/src/stateful.rs):32 |
| `WORKER_CHANNEL_CAPACITY` | `64` | [stateful.rs](crates/trigger-http/src/stateful.rs):36 |
| `--stateful-idle-timeout` | `300s` default | [trigger-http/src/lib.rs](crates/trigger-http/src/lib.rs):147 |

### File map

| Concern | File |
|---|---|
| Lifecycle WIT | `wit/deps/spin-stateful-component@0.1.0/lifecycle.wit`, `wit/world.wit` |
| Manifest opt‑in | [crates/manifest/src/schema/v2.rs](crates/manifest/src/schema/v2.rs), [crates/loader/src/local.rs](crates/loader/src/local.rs) |
| SDK macros | `spin-rust-sdk/crates/spin-sdk-macro/src/lib.rs` |
| Example app | `spin-rust-sdk/examples/stateful-sqlite/` (router + todo/guestbook/counter) |
| Worker / routing | [crates/trigger-http/src/stateful.rs](crates/trigger-http/src/stateful.rs), [outbound_http.rs](crates/trigger-http/src/outbound_http.rs), [server.rs](crates/trigger-http/src/server.rs), [allowed_hosts.rs](crates/outbound-networking-config/src/allowed_hosts.rs) |
| KV instance scope | [crates/factor-key-value/src/util.rs](crates/factor-key-value/src/util.rs), [lib.rs](crates/factor-key-value/src/lib.rs) |
| SQLite factor / scope | [crates/factor-sqlite/src/lib.rs](crates/factor-sqlite/src/lib.rs), [host.rs](crates/factor-sqlite/src/host.rs) |
| Backend dispatch | [crates/sqlite/src/lib.rs](crates/sqlite/src/lib.rs) |
| In‑process backend | [crates/sqlite-inproc/src/lib.rs](crates/sqlite-inproc/src/lib.rs) |
| libSQL backend | [crates/sqlite-libsql/src/lib.rs](crates/sqlite-libsql/src/lib.rs) |
| Turso backend | [crates/sqlite-turso/src/lib.rs](crates/sqlite-turso/src/lib.rs), [README](crates/sqlite-turso/README.md) |
