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

The backend is gated behind the `turso` feature so a normal Spin build does not
pull the (large, beta) Turso dependency:

```bash
cargo build --features turso          # builds `spin` with the Turso backend
```

Then select it per database label in your runtime config:

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

## Not yet done (follow-ups)

- **Push on suspend.** The host worker should drive a final `push()` right after
  `lifecycle::suspend()` for prompt durability on idle. This needs a `sync()` hook
  on the `Connection` trait + a way to enumerate an instance's open connections
  from the worker; periodic sync currently approximates it.
- **Remote auto-provisioning.** The Turso crate does **not** auto-create the
  remote database on first sync. Realizing "one auto-created DB per instance"
  needs either a self-hosted sync server that provisions on demand, or a
  `RemoteProvisioner` hook that calls the Turso Platform API. Today the remote
  must already exist (or be created by the hosted engine).
- **End-to-end test** against a running Turso sync server.

[Turso]: https://github.com/tursodatabase/turso
