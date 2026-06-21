# spin-sqlite-turso

A Spin SQLite backend that gives a database **local-first sync** using the new
[Turso] engine (the from-scratch Rust rewrite of SQLite). All reads and writes hit
a local SQLite file; the Turso engine syncs that file to a remote ("hosted")
database in the background.

Its main purpose is to give **stateful components** a per-instance database: the
SQLite analog of the per-instance key-value "instance-store". Each
`(component, instance)` gets its **own** local file *and* its **own** remote
database.

> ⚠️ **Beta.** Turso offline sync currently has **no durability guarantees** and
> conflict *resolution* is not yet implemented (only detection). This backend is
> **opt-in** (off by default) and not suitable for production yet. The
> production-ready alternative today is `libsql` embedded replicas
> (`crates/sqlite-libsql`); this crate is intentionally isolated behind the
> `Connection` trait so the engine can be swapped.

## Enabling it

On this branch the Turso backend is **built into `spin`** — a normal `cargo build`
includes it (it pulls the large, beta Turso dependency). Select it per database
label in your runtime config:

```toml
# A normal (shared) Turso-synced database:
[sqlite_database.notes]
type = "turso"
url = "http://127.0.0.1:8080"     # the hosted Turso engine to sync with
token = "<auth-token>"
local_dir = "turso-dbs"            # optional; under the .spin state dir
sync_interval_seconds = 5          # optional; 0 = sync only on open

# The per-instance database for stateful components (well-known label):
[sqlite_database.instance-db]
type = "turso"
url = "http://127.0.0.1:8080"
token = "<auth-token>"
sync_interval_seconds = 5
```

A component opts in like any other SQLite database:

```toml
[component.my-stateful-component]
stateful = true
sqlite_databases = ["instance-db"]
```

In the guest it is just normal SQLite — `connection::open("instance-db")` and run
SQL. The component never knows it is syncing.

## How per-instance scoping works

This reuses the stateful per-instance pattern already used by key-value. When a
stateful worker activates instance `inst` of component `comp`, it calls
`SqliteFactor`'s `set_instance_id("comp/inst")` (see
`crates/trigger-http/src/stateful.rs`). For the well-known `instance-db` label,
that replaces the connection creator with a per-instance one
(`ConnectionCreator::scoped_to_instance`) which derives:

- a local file: `<state-dir>/<local_dir>/comp_inst.db`
- a remote database: `<url>/comp_inst`

so each instance is fully isolated, locally and remotely. (Unlike key-value,
which key-prefixes one shared store, a per-instance SQLite database is a separate
database — which the hosted engine is expected to create on first sync.)

## Sync timing

- **Pull on open** — the local replica is warmed from the remote when the
  database is first opened.
- **Periodic** — a background task pushes+pulls every `sync_interval_seconds`
  while the connection is alive (it stops, via a `Weak` ref, when the instance is
  suspended/evicted).
- **Push on suspend** — when a stateful instance is suspended/evicted, the host
  worker flushes each of the instance's open connections to the remote (via the
  `Connection::sync()` hook → Turso `push()`), so writes are durable before the
  instance is dropped.

## Provisioning the remote database (the `provision` config field)

Turso addresses a database by its URL **host** (the database name is the
subdomain), never by a path. So a server that hosts only one database can't be
shared per-instance; per-instance remote databases require a multi-tenant server
(Turso Cloud). The backend obtains each instance's remote via a `RemoteProvisioner`
selected by `provision`:

- **`provision = "auto"`** — a **single** remote database at the configured `url`,
  for one local `tursodb --sync-server` (or any single-DB server). This is *not*
  per-instance — every instance shares the one remote — so use it only for a single
  instance / smoke tests. (Local files stay per-instance regardless.)
- **`provision = "platform"`** — the per-instance multi-DB path, on **Turso Cloud**.
  Each instance gets its own Cloud database: `ensure` creates it via the Platform
  API (`POST /v1/organizations/{org}/databases`, idempotent), reads the database's
  real `Hostname` from the response, and syncs to `libsql://{hostname}`. Requires
  `org`, `group`, `api_token` (org token, to create); `db_token` (a group token to
  sync — else a db-scoped token is minted per database); optional `name_prefix`
  (default `spin-`). Cloud database names are derived safely from the instance id
  (lowercase, length-bounded, with a stable hash so distinct instances never
  collide).

## Local testing

The new Turso sync protocol is served by **`tursodb --sync-server`** — NOT by
`turso dev` (which is the legacy `sqld`/Hrana server and is **not** compatible
with this crate's `push`/`pull`). Install the `tursodb` binary at the *same commit*
as the `turso` crate this backend depends on (so the server and client speak the
same protocol — see the crate's `.cargo_vcs_info.json` for the sha):

```bash
cargo install --git https://github.com/tursodatabase/turso \
  --rev <sha-of-the-turso-crate-version> --bin tursodb turso_cli

tursodb ./turso-server.db --sync-server 0.0.0.0:8080   # http://127.0.0.1:8080, no auth
```

Point the backend at it (single-DB, no token):

```toml
[sqlite_database.instance-db]
type = "turso"
provision = "auto"
url = "http://127.0.0.1:8080"
sync_interval_seconds = 5
```

One `tursodb --sync-server` process hosts a **single** database, so this only
exercises **one** instance. There is no open-source local multi-tenant server, so
per-instance remote databases are tested on **Turso Cloud**.

### Per-instance multi-DB on Turso Cloud

```bash
turso auth login
turso auth api-tokens mint spin          # -> api_token (creates databases)
turso group tokens create default        # -> db_token  (syncs all DBs in the group)
```

```toml
[sqlite_database.instance-db]
type = "turso"
provision = "platform"
org = "<your-org>"
group = "default"
api_token = "<api_token>"
db_token = "<group-token>"
sync_interval_seconds = 5
# name_prefix = "spin-"   # optional
```

Each instance then becomes its own Cloud database; `turso db list` shows one per
`(component, instance)`.

## Still beta / to verify

- Live end-to-end isolation across many instances + the exact Turso Cloud URL/token
  shape for `provision = "platform"` (built from a template today).
- Conflict resolution (Turso detects but does not yet resolve) and durability — do
  not use in production yet.

[Turso]: https://github.com/tursodatabase/turso
