use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use http_body_util::BodyExt;
use spin_factors::RuntimeFactors;
use spin_factors_executor::InstanceState;
use tokio::sync::{mpsc, oneshot, RwLock};
use tracing::Instrument as _;
use wasmtime::component::Accessor;
use wasmtime_wasi_http::{
    p2::body::HyperIncomingBody as Body,
    p3::{
        bindings::{http::types, ServiceIndices},
        WasiHttpCtxView,
    },
};

use crate::TriggerApp;

const LIFECYCLE_EXPORT: &str = "spin:stateful-component/lifecycle@0.1.0";

/// Maximum number of live stateful instances kept in memory at once. When the
/// cap is reached, the least-recently-used idle instance is suspended to make
/// room, bounding memory against a flood of distinct instance IDs.
const DEFAULT_MAX_INSTANCES: usize = 1024;

/// Bounded capacity of each worker's request channel. Sending blocks — applying
/// backpressure to the caller — when an instance falls this far behind.
const WORKER_CHANNEL_CAPACITY: usize = 64;

type StoreData<F> = InstanceState<<F as RuntimeFactors>::InstanceState, ()>;

struct HttpTask {
    request: http::Request<Body>,
    response_tx: oneshot::Sender<Result<http::Response<Body>>>,
    /// Keeps the request counted as in-flight from reservation (under the
    /// workers lock) until the response body has finished streaming, so the
    /// worker is never evicted out from under a still-streaming body.
    in_flight: InFlightGuard,
    /// The caller's tracing span (captured at dispatch, on the caller's task —
    /// typically the outbound `spin_outbound_http.send_request` span). The
    /// worker runs the guest on a different task, so the span is carried across
    /// the channel and used as the parent of the per-request `execute_wasm`
    /// span, keeping the stateful component's work in the caller's trace.
    parent_span: tracing::Span,
}

/// Task for handling a single HTTP request, spawned concurrently within
/// a stateful instance's `run_concurrent` loop.
struct HandleRequestTask<F: RuntimeFactors> {
    service: Arc<wasmtime_wasi_http::p3::bindings::Service>,
    getter: fn(&mut StoreData<F>) -> WasiHttpCtxView<'_>,
    task: HttpTask,
    /// Component id, used to label the `execute_wasm` span so the stateful
    /// component (not the `spin` host) is attributed in traces.
    component_id: Arc<str>,
    /// Instance id, recorded on the span so traces show which live instance
    /// served the request.
    instance_id: Arc<str>,
}

impl<F: RuntimeFactors> wasmtime::component::AccessorTask<StoreData<F>>
    for HandleRequestTask<F>
{
    fn run(
        self,
        accessor: &Accessor<StoreData<F>>,
    ) -> impl std::future::Future<Output = wasmtime::Result<()>> + Send {
        let HandleRequestTask {
            service,
            getter,
            task,
            component_id,
            instance_id,
        } = self;
        let HttpTask {
            request,
            response_tx,
            in_flight,
            parent_span,
        } = task;

        // Mirror the non-stateful executors' span (see `wasip3.rs`): same span
        // name and `otel.name` format, so a stateful request shows up as
        // `execute_wasm_component <component>` and is attributed to the
        // component. Parented to the caller's span so it joins the same trace.
        // The stateful side has no separate per-request server span (the
        // `handle_http_request` span belongs to the router, the `send_request`
        // span to the outbound hop), so record the method and path here too,
        // making this span self-describing — which `(component, instance)`
        // served which operation.
        let span = tracing::info_span!(
            parent: &parent_span,
            "spin_trigger_http.execute_wasm",
            "otel.name" = format!("execute_wasm_component {component_id}"),
            component_id = %component_id,
            instance_id = %instance_id,
            "http.request.method" = %request.method(),
            "url.path" = %request.uri().path(),
        );

        async move {
            match handle_single_request::<F>(accessor, &service, getter, request).await {
                Ok((response, body_rx)) => {
                    if response_tx.send(Ok(response)).is_ok() {
                        // Keep this spawned task — and therefore the store's
                        // event loop — alive until Hyper has finished reading
                        // the response body; otherwise the guest would not get
                        // a chance to finish writing it.
                        let _ = body_rx.await;
                    }
                }
                Err(e) => {
                    let _ = response_tx.send(Err(e));
                }
            }
            // Release the in-flight reservation only now that the body has been
            // fully streamed, so eviction can't tear the instance down
            // mid-stream and truncate the response body.
            drop(in_flight);
            Ok(())
        }
        .instrument(span)
    }
}

/// Spawned task that receives new HTTP requests from the channel and
/// spawns a `HandleRequestTask` for each one.  By living as a spawned
/// task (rather than the main `run_concurrent` closure), its tokio
/// channel waker integrates properly with the accessor's scheduler,
/// so new requests are picked up even while other tasks are in-flight.
struct ReceiverTask<F: RuntimeFactors> {
    task_rx: mpsc::Receiver<HttpTask>,
    service: Arc<wasmtime_wasi_http::p3::bindings::Service>,
    getter: fn(&mut StoreData<F>) -> WasiHttpCtxView<'_>,
    shutdown: oneshot::Sender<()>,
    component_id: Arc<str>,
    instance_id: Arc<str>,
}

impl<F: RuntimeFactors> wasmtime::component::AccessorTask<StoreData<F>>
    for ReceiverTask<F>
{
    fn run(
        self,
        accessor: &Accessor<StoreData<F>>,
    ) -> impl std::future::Future<Output = wasmtime::Result<()>> + Send {
        async move {
            let mut task_rx = self.task_rx;
            while let Some(task) = task_rx.recv().await {
                accessor.spawn(HandleRequestTask::<F> {
                    service: Arc::clone(&self.service),
                    getter: self.getter,
                    task,
                    component_id: Arc::clone(&self.component_id),
                    instance_id: Arc::clone(&self.instance_id),
                });
            }
            let _ = self.shutdown.send(());
            Ok(())
        }
    }
}

/// Handle to a background worker managing a live stateful component instance.
struct StatefulWorker {
    /// Generation id, used to guard self-eviction against a newer worker having
    /// already replaced this one under the same key.
    id: u64,
    task_tx: mpsc::Sender<HttpTask>,
    last_activity: Arc<std::sync::Mutex<Instant>>,
    /// Number of requests currently outstanding (queued or in-flight). A worker
    /// is only evicted while this is zero, so eviction never drops live work.
    in_flight: Arc<AtomicUsize>,
}

impl StatefulWorker {
    /// The bits needed to dispatch a request after the workers map lock has
    /// been released.
    fn handle(&self) -> WorkerHandle {
        WorkerHandle {
            task_tx: self.task_tx.clone(),
            last_activity: Arc::clone(&self.last_activity),
        }
    }
}

/// Cloned worker bits used to dispatch a request without holding the workers
/// map lock across the (possibly blocking) send + response await.
#[derive(Clone)]
struct WorkerHandle {
    task_tx: mpsc::Sender<HttpTask>,
    last_activity: Arc<std::sync::Mutex<Instant>>,
}

/// Increments a worker's in-flight count on creation and decrements on drop,
/// so a request is counted as outstanding for its whole lifetime (queued and
/// processing). Eviction is gated on the count being zero.
struct InFlightGuard(Arc<AtomicUsize>);

impl InFlightGuard {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The outcome of trying to dispatch a request to a worker.
enum DispatchOutcome {
    /// The worker accepted the request; carries its (possibly error) response.
    Handled(Result<http::Response<Body>>),
    /// The worker's channel was closed (it has exited); the request is returned
    /// so the caller can recreate the instance and retry.
    WorkerGone(http::Request<Body>),
}

/// Manages long-lived stateful component instances keyed by (component_id, instance_id).
///
/// On first request for an ID, the manager:
/// 1. Spawns a background worker that creates a Store + Instance
/// 2. Calls the component's `lifecycle::instantiate(id)` export
/// 3. Enters `store.run_concurrent` to handle HTTP requests via a task channel
///
/// On idle timeout, the worker exits `run_concurrent`, calls `lifecycle::suspend()`,
/// and drops the instance.
pub struct StatefulInstanceManager<F: RuntimeFactors> {
    workers: RwLock<HashMap<(String, String), StatefulWorker>>,
    trigger_app: Arc<TriggerApp<F>>,
    idle_timeout: Duration,
    /// Maximum number of live instances before LRU eviction kicks in.
    max_instances: usize,
    /// Monotonic source of worker generation ids.
    next_worker_id: AtomicU64,
}

impl<F: RuntimeFactors> StatefulInstanceManager<F> {
    pub fn new(trigger_app: Arc<TriggerApp<F>>, idle_timeout: Duration) -> Self {
        Self {
            workers: RwLock::new(HashMap::new()),
            trigger_app,
            idle_timeout,
            max_instances: DEFAULT_MAX_INSTANCES,
            next_worker_id: AtomicU64::new(0),
        }
    }

    /// Handle an HTTP request to a stateful component instance.
    pub async fn handle_request(
        self: &Arc<Self>,
        req: http::Request<Body>,
        component_id: &str,
        instance_id: &str,
    ) -> Result<http::Response<Body>> {
        let key = (component_id.to_string(), instance_id.to_string());

        // Fast path: dispatch to an existing worker. We reserve the in-flight
        // slot and clone the handle while holding the read lock, then release
        // the lock before sending — so we never hold the lock across an await,
        // and the idle checker can't evict a worker we're about to use.
        let existing = {
            let guard = self.workers.read().await;
            guard
                .get(&key)
                .map(|w| (w.handle(), InFlightGuard::new(Arc::clone(&w.in_flight))))
        };
        let req = if let Some((handle, in_flight)) = existing {
            match Self::dispatch(handle, in_flight, req).await {
                DispatchOutcome::Handled(resp) => return resp,
                // Worker has exited; fall through to recreate the instance.
                DispatchOutcome::WorkerGone(req) => req,
            }
        } else {
            req
        };

        // Slow path: only `stateful = true` components may be addressed here.
        self.ensure_stateful(component_id)?;

        // Slow path: (re)create the worker under the write lock — reusing a
        // live one if a concurrent request already created it — and dispatch.
        // Retry once if the chosen worker exits between the liveness check and
        // the send (self-eviction or a trap racing the check); bounded so it
        // can never loop.
        let mut req = req;
        for _ in 0..2 {
            let (handle, in_flight) = {
                let mut guard = self.workers.write().await;
                let live = guard
                    .get(&key)
                    .filter(|w| !w.task_tx.is_closed())
                    .map(|w| (w.handle(), Arc::clone(&w.in_flight)));
                let (handle, counter) = match live {
                    Some(hc) => hc,
                    None => {
                        if !guard.contains_key(&key) && guard.len() >= self.max_instances {
                            self.evict_lru_idle(&mut guard);
                        }
                        let worker = self.spawn_worker(&key);
                        let hc = (worker.handle(), Arc::clone(&worker.in_flight));
                        guard.insert(key.clone(), worker);
                        hc
                    }
                };
                // Reserve the in-flight slot under the lock, before dispatching.
                (handle, InFlightGuard::new(counter))
            };

            match Self::dispatch(handle, in_flight, req).await {
                DispatchOutcome::Handled(resp) => return resp,
                DispatchOutcome::WorkerGone(returned) => req = returned,
            }
        }

        Err(anyhow::anyhow!(
            "stateful instance worker exited before handling the request"
        ))
    }

    /// Verify the target component is declared `stateful = true`.
    fn ensure_stateful(&self, component_id: &str) -> Result<()> {
        let component = self
            .trigger_app
            .app()
            .get_component(component_id)
            .with_context(|| format!("no such component {component_id:?}"))?;
        let is_stateful = component
            .get_metadata(spin_locked_app::STATEFUL_KEY)?
            .unwrap_or(false);
        anyhow::ensure!(
            is_stateful,
            "component {component_id:?} is not a stateful component and cannot be addressed via spin.alt"
        );
        Ok(())
    }

    /// Dispatch a request to a worker via its handle, holding the in-flight
    /// reservation for the whole call. Returns [`DispatchOutcome::WorkerGone`]
    /// (with the request) if the worker's channel has closed.
    async fn dispatch(
        handle: WorkerHandle,
        in_flight: InFlightGuard,
        req: http::Request<Body>,
    ) -> DispatchOutcome {
        let (response_tx, response_rx) = oneshot::channel();
        // The in-flight guard travels with the task so the worker holds the
        // reservation until the response body has finished streaming (not just
        // until the response head is delivered here).
        let task = HttpTask {
            request: req,
            response_tx,
            in_flight,
            // Capture the caller's span here, on the caller's task (inside the
            // outbound `send_request` span), so the worker — which runs the
            // guest on a different task — can parent its `execute_wasm` span to
            // it and keep the stateful request in the same trace.
            parent_span: tracing::Span::current(),
        };
        // Bounded send: blocks (backpressure) while the instance is behind, and
        // errors — returning the task — only once the worker has exited.
        match handle.task_tx.send(task).await {
            Ok(()) => {
                *handle.last_activity.lock().unwrap() = Instant::now();
                DispatchOutcome::Handled(response_rx.await.unwrap_or_else(|_| {
                    Err(anyhow::anyhow!("stateful instance worker dropped the request"))
                }))
            }
            Err(mpsc::error::SendError(task)) => DispatchOutcome::WorkerGone(task.request),
        }
    }

    /// Evict the least-recently-used idle worker to stay within the instance
    /// cap. Only idle workers (no outstanding requests) are eligible, so live
    /// work is never dropped.
    fn evict_lru_idle(&self, workers: &mut HashMap<(String, String), StatefulWorker>) {
        let victim = workers
            .iter()
            .filter(|(_, w)| w.in_flight.load(Ordering::SeqCst) == 0)
            .min_by_key(|(_, w)| *w.last_activity.lock().unwrap())
            .map(|(key, _)| key.clone());
        match victim {
            Some(key) => {
                tracing::info!(
                    component_id = key.0,
                    instance_id = key.1,
                    "Evicting LRU stateful instance to stay within the instance cap"
                );
                // Dropping the worker closes its channel, which makes the
                // background worker exit run_concurrent and call suspend.
                workers.remove(&key);
            }
            None => tracing::warn!(
                "stateful instance cap reached but all instances are busy; \
                 temporarily exceeding the cap"
            ),
        }
    }

    /// Spawn a background worker for a new stateful component instance.
    ///
    /// When the worker task exits — from idle suspension, eviction, or a
    /// failure/trap during instantiation or handling — it removes its own entry
    /// from the map (unless a newer worker has already taken its place), so a
    /// subsequent request creates a fresh instance instead of wedging on a dead
    /// one.
    fn spawn_worker(self: &Arc<Self>, key: &(String, String)) -> StatefulWorker {
        let id = self.next_worker_id.fetch_add(1, Ordering::Relaxed);
        let (task_tx, task_rx) = mpsc::channel(WORKER_CHANNEL_CAPACITY);
        let last_activity = Arc::new(std::sync::Mutex::new(Instant::now()));
        let in_flight = Arc::new(AtomicUsize::new(0));

        let manager = Arc::clone(self);
        let key = key.clone();
        tokio::spawn(async move {
            if let Err(e) =
                run_stateful_worker::<F>(manager.trigger_app.clone(), &key.0, &key.1, task_rx).await
            {
                tracing::error!(
                    component_id = key.0,
                    instance_id = key.1,
                    "stateful instance worker failed: {e:?}"
                );
            }
            // Self-evict, unless a newer worker has already replaced us.
            let mut workers = manager.workers.write().await;
            if workers.get(&key).map(|w| w.id) == Some(id) {
                workers.remove(&key);
            }
        });

        StatefulWorker {
            id,
            task_tx,
            last_activity,
            in_flight,
        }
    }

    /// Start the background idle-checker that suspends timed-out instances.
    pub fn start_idle_checker(self: &Arc<Self>) {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            loop {
                interval.tick().await;

                // A worker is idle only when it has no outstanding requests and
                // hasn't been used within the timeout — so long-running requests
                // are never evicted mid-flight.
                let idle = |w: &StatefulWorker| {
                    w.in_flight.load(Ordering::SeqCst) == 0
                        && w.last_activity.lock().unwrap().elapsed() > manager.idle_timeout
                };

                let expired: Vec<(String, String)> = {
                    let guard = manager.workers.read().await;
                    guard
                        .iter()
                        .filter(|(_, w)| idle(w))
                        .map(|(key, _)| key.clone())
                        .collect()
                };

                if expired.is_empty() {
                    continue;
                }

                let mut write_guard = manager.workers.write().await;
                for key in expired {
                    // Re-check under the write lock: a request may have arrived
                    // (bumping activity / in-flight) since the read scan.
                    if write_guard.get(&key).map(|w| idle(w)).unwrap_or(false) {
                        tracing::info!(
                            component_id = key.0,
                            instance_id = key.1,
                            "Suspending idle stateful component instance"
                        );
                        // Dropping the worker closes the task channel, which
                        // causes the background worker to exit run_concurrent
                        // and call lifecycle::suspend before cleaning up.
                        write_guard.remove(&key);
                    }
                }
            }
        });
    }
}

/// Background worker that owns a Wasm instance and processes HTTP requests.
///
/// Lifecycle:
/// 1. Create store + component instance
/// 2. Call `lifecycle::instantiate(id)`
/// 3. Enter `store.run_concurrent` — loop receiving HTTP tasks from the channel
/// 4. When the channel closes (idle timeout), exit and call `lifecycle::suspend()`
async fn run_stateful_worker<F: RuntimeFactors>(
    trigger_app: Arc<TriggerApp<F>>,
    component_id: &str,
    instance_id: &str,
    task_rx: mpsc::Receiver<HttpTask>,
) -> Result<()> {
    tracing::info!(component_id, instance_id, "Starting stateful component instance");

    // 1. Prepare store with all host factors. Scope the key-value
    //    "instance-store" to this (component, instance) pair so each stateful
    //    instance sees isolated data — and two components that happen to use the
    //    same instance id don't collide (a no-op if the app has no key-value
    //    factor).
    let mut builder = trigger_app.prepare(component_id)?;
    if let Some(kv) = builder.factor_builder::<spin_factor_key_value::KeyValueFactor>() {
        kv.set_instance_id(format!("{component_id}/{instance_id}"));
    }
    // Likewise scope the SQLite "instance-db" to this (component, instance): with a
    // Turso-sync backend each instance gets its own local file + remote database.
    if let Some(sq) = builder.factor_builder::<spin_factor_sqlite::SqliteFactor>() {
        sq.set_instance_id(format!("{component_id}/{instance_id}"));
    }
    let mut store: wasmtime::Store<StoreData<F>> = builder.instantiate_store(())?.into_inner();

    // 2. Instantiate the Wasm component
    let pre = trigger_app.get_instance_pre(component_id)?;
    let instance = pre.instantiate_async(&mut store).await?;

    // 3. Look up lifecycle exports
    let lifecycle_idx = instance
        .get_export_index(&mut store, None, LIFECYCLE_EXPORT)
        .with_context(|| format!("component does not export {LIFECYCLE_EXPORT}"))?;
    let instantiate_idx = instance
        .get_export_index(&mut store, Some(&lifecycle_idx), "instantiate")
        .context("missing lifecycle::instantiate export")?;
    let suspend_idx = instance
        .get_export_index(&mut store, Some(&lifecycle_idx), "suspend")
        .context("missing lifecycle::suspend export")?;

    let instantiate_func =
        instance.get_typed_func::<(&str,), ()>(&mut store, &instantiate_idx)?;
    let suspend_func = instance.get_typed_func::<(), ()>(&mut store, &suspend_idx)?;

    // 4. Call lifecycle::instantiate(id)
    instantiate_func
        .call_async(&mut store, (instance_id,))
        .await
        .map_err(|e| anyhow::anyhow!("lifecycle::instantiate failed: {e}"))?;

    tracing::info!(component_id, instance_id, "Stateful instance activated");

    // 5. Prepare the HTTP Service from the same instance
    let service_indices =
        ServiceIndices::new(pre).map_err(|e| anyhow::anyhow!("missing wasi:http/handler: {e}"))?;
    let service = Arc::new(
        service_indices
            .load(&mut store, &instance)
            .map_err(|e| anyhow::anyhow!("failed to load HTTP service: {e}"))?,
    );

    // 6. Enter run_concurrent to handle HTTP requests concurrently.
    //
    //    The channel receiver is wrapped in a spawned `ReceiverTask` rather
    //    than polled directly in the main closure.  Inside run_concurrent the
    //    accessor's scheduler does not re-poll the *main closure* when a tokio
    //    waker fires while other spawned tasks are in-flight — but it does
    //    properly schedule *spawned tasks* against each other.  By making the
    //    receiver a spawned task, new channel messages are picked up promptly
    //    even while HandleRequestTasks are awaiting WASI timers.
    let getter = (|data: &mut StoreData<F>| wasi_http::<F>(data).unwrap())
        as fn(&mut StoreData<F>) -> WasiHttpCtxView<'_>;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let receiver = ReceiverTask::<F> {
        task_rx,
        service: Arc::clone(&service),
        getter,
        shutdown: shutdown_tx,
        component_id: Arc::from(component_id),
        instance_id: Arc::from(instance_id),
    };

    let run_result = store
        .run_concurrent(async |accessor: &Accessor<StoreData<F>>| {
            accessor.spawn(receiver);
            // Keep the main closure alive until the ReceiverTask signals
            // that the channel has closed (idle timeout).
            let _ = shutdown_rx.await;
            anyhow::Ok(())
        })
        .await;

    if let Err(e) = &run_result {
        tracing::error!(component_id, instance_id, "run_concurrent failed: {e:?}");
    }

    // 7. run_concurrent has returned (channel closed) — call lifecycle::suspend
    tracing::info!(component_id, instance_id, "Suspending stateful instance");
    if let Err(e) = suspend_func.call_async(&mut store, ()).await {
        tracing::error!(
            component_id,
            instance_id,
            "lifecycle::suspend failed: {e:?}"
        );
    }

    Ok(())
}

/// Dispatch a single HTTP request to the component's handler within run_concurrent.
///
/// Returns the HTTP response together with a receiver that fires once the
/// response body has been fully read, so the caller can keep the store's event
/// loop alive until the guest has finished streaming the body.
async fn handle_single_request<F: RuntimeFactors>(
    accessor: &Accessor<StoreData<F>>,
    service: &wasmtime_wasi_http::p3::bindings::Service,
    getter: fn(&mut StoreData<F>) -> WasiHttpCtxView<'_>,
    req: http::Request<Body>,
) -> Result<(
    http::Response<Body>,
    futures::channel::oneshot::Receiver<()>,
)> {
    let (parts, body) = req.into_parts();
    let body = body.map_err(spin_factor_outbound_http::p2_to_p3_error_code);
    let request = http::Request::from_parts(parts, body);
    let (request, request_io_result) = types::Request::from_http(request);

    // Push the request resource into the table.
    let request_handle = accessor
        .with(|mut store| anyhow::Ok(wasi_http::<F>(store.data_mut())?.table.push(request)?))?;

    // Call the component's HTTP handler (async WIT export).
    let response = service
        .wasi_http_handler()
        .call_handle(accessor, request_handle)
        .await?;

    // Extract the response resource from the table.
    let response = accessor
        .with(|mut store| anyhow::Ok(wasi_http::<F>(store.get())?.table.delete(response?)?))?;

    // Convert to an http::Response.
    let response = accessor
        .with(|mut store| response.into_http_with_getter(&mut store, request_io_result, getter))?;

    // Wrap the response body so we are notified once Hyper has read it fully.
    let (response_body_tx, response_body_rx) = futures::channel::oneshot::channel();
    let response = response.map(|body| {
        spin_factor_outbound_http::NotifyOnDropBody::new(
            body.map_err(spin_factor_outbound_http::p3_to_p2_error_code),
            response_body_tx,
        )
        .boxed_unsync()
    });

    Ok((response, response_body_rx))
}

fn wasi_http<F: RuntimeFactors>(data: &mut StoreData<F>) -> Result<WasiHttpCtxView<'_>> {
    spin_factor_outbound_http::OutboundHttpFactor::get_wasi_p3_http_impl(
        data.factors_instance_state_mut(),
    )
    .context("missing OutboundHttpFactor")
}
