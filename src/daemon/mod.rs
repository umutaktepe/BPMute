pub mod clock;
mod monitoring;
pub mod state;
pub mod wearable;

use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use thiserror::Error;
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::actions::traits::{ActionExecutor, Notifier};
use crate::config::{CliOverrides, LoadedConfig};
use crate::idle::IdleProbe;
use crate::ipc::protocol::{ErrorPayload, SessionEndResponse};
use crate::ipc::pubsub::EventBroker;
use crate::ipc::server::handle_connection;
use crate::paths::{EnvSource, ResolvedPaths, ensure_private_dir};
use crate::sleep_engine::GraceOutcome;
use crate::storage::{PersistenceWorker, StorageError, db_path_in_data_dir, lock_path_in_data_dir};

pub use clock::{DaemonClock, SystemClock, VirtualClock};
pub use state::{
    BluezEnrollmentDriver, DaemonCore, DaemonCoreInit, EnrollmentPairingHook, NoopEnrollmentDriver,
};
pub use wearable::{NullWearableSource, WearableDescriptor, WearableEvent, WearableSource};

/// Errors that prevent `bpmute daemon` from starting or binding its socket.
#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("daemon is already running on socket {0}")]
    AlreadyRunning(PathBuf),
    #[error("socket path {0} exists and is not a Unix domain socket")]
    NonSocketConflict(PathBuf),
    #[error("daemon I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("ledger storage error: {0}")]
    Storage(#[from] StorageError),
}

pub enum EngineControlEvent {
    GraceOutcome(GraceOutcome),
    HardWipe {
        wipe_all: bool,
        reply: oneshot::Sender<()>,
    },
    ManualCloseSession {
        reply: oneshot::Sender<Result<SessionEndResponse, ErrorPayload>>,
    },
    ActiveHoursEntered {
        ts_ms: i64,
    },
    ActiveHoursExpired {
        ts_ms: i64,
    },
}

/// Startup parameters for [`DaemonHandle::spawn`].
pub struct DaemonOptions {
    pub paths: ResolvedPaths,
    pub loaded_config: LoadedConfig,
    pub env: Arc<dyn EnvSource>,
    pub cli_overrides: CliOverrides,
    pub clock: Arc<dyn DaemonClock>,
    pub wearable_source: Option<Arc<dyn WearableSource>>,
    pub install_signals: bool,
    pub action_executor: Option<Arc<dyn ActionExecutor>>,
    pub notifier: Option<Arc<dyn Notifier>>,
    pub idle_probe: Option<Arc<dyn IdleProbe>>,
    pub enrollment_driver: Option<Arc<dyn EnrollmentPairingHook>>,
}

impl DaemonOptions {
    /// Constructs [`DaemonOptions`] for hermetic testing with in-memory defaults
    /// (`NullWearableSource`, `FakeActionExecutor`, `FakeNotifier`, `FakeIdleProbe`, `NoopEnrollmentDriver`, `install_signals = false`).
    pub fn for_testing(
        paths: ResolvedPaths,
        loaded_config: LoadedConfig,
        env: Arc<dyn EnvSource>,
        clock: Arc<dyn DaemonClock>,
    ) -> Self {
        Self {
            paths,
            loaded_config,
            env,
            cli_overrides: CliOverrides::default(),
            clock,
            wearable_source: None,
            install_signals: false,
            action_executor: None,
            notifier: None,
            idle_probe: None,
            enrollment_driver: None,
        }
    }

    /// Builder helper to attach a custom [`WearableSource`].
    pub fn with_wearable_source(mut self, source: Arc<dyn WearableSource>) -> Self {
        self.wearable_source = Some(source);
        self
    }

    /// Builder helper to attach a custom [`ActionExecutor`].
    pub fn with_action_executor(mut self, executor: Arc<dyn ActionExecutor>) -> Self {
        self.action_executor = Some(executor);
        self
    }

    /// Builder helper to attach a custom [`Notifier`].
    pub fn with_notifier(mut self, notifier: Arc<dyn Notifier>) -> Self {
        self.notifier = Some(notifier);
        self
    }

    /// Builder helper to attach a custom [`IdleProbe`].
    pub fn with_idle_probe(mut self, idle_probe: Arc<dyn IdleProbe>) -> Self {
        self.idle_probe = Some(idle_probe);
        self
    }

    /// Builder helper to attach a custom [`EnrollmentPairingHook`].
    pub fn with_enrollment_driver(mut self, driver: Arc<dyn EnrollmentPairingHook>) -> Self {
        self.enrollment_driver = Some(driver);
        self
    }

    /// Builder helper to set CLI overrides.
    pub fn with_cli_overrides(mut self, cli_overrides: CliOverrides) -> Self {
        self.cli_overrides = cli_overrides;
        self
    }
}

/// Running background daemon instance with graceful shutdown and IPC control handles.
pub struct DaemonHandle {
    core: Arc<DaemonCore>,
    shutdown_tx: watch::Sender<bool>,
    ctrl_tx: mpsc::Sender<EngineControlEvent>,
    monitor_task: JoinHandle<()>,
    accept_task: JoinHandle<()>,
    persistence_worker: PersistenceWorker,
    socket_path: PathBuf,
}

impl DaemonHandle {
    /// Binds the `0600` Unix domain socket, starts the persistence worker and monitoring loop,
    /// and returns a handle.
    pub async fn spawn(opts: DaemonOptions) -> Result<Self, DaemonError> {
        let socket_path = opts.paths.socket_path.clone();
        if let Some(parent) = socket_path.parent()
            && !parent.as_os_str().is_empty()
        {
            ensure_private_dir(parent)?;
        }

        prepare_socket_path(&socket_path).await?;
        let listener = UnixListener::bind(&socket_path)?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;

        let db_path = db_path_in_data_dir(&opts.paths.data_dir);
        let lock_path = lock_path_in_data_dir(&opts.paths.data_dir);
        let startup_ms = opts.clock.now_utc().timestamp_millis();
        let (persistence_worker, persistence_handle, initial_calibration) =
            PersistenceWorker::spawn(&db_path, &lock_path, startup_ms)?;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let broker = Arc::new(EventBroker::default());
        let initial_detection = opts.loaded_config.config.detection.clone();
        let initial_retention = opts.loaded_config.config.retention.raw_samples_days;
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<EngineControlEvent>(32);

        let action_executor = opts
            .action_executor
            .unwrap_or_else(|| Arc::new(crate::actions::FakeActionExecutor::new()));
        let notifier = opts
            .notifier
            .unwrap_or_else(|| Arc::new(crate::actions::FakeNotifier::new()));
        let idle_probe = opts
            .idle_probe
            .unwrap_or_else(|| Arc::new(crate::idle::FakeIdleProbe::default()));
        idle_probe.resolve_backend().await;
        let enrollment_driver = opts
            .enrollment_driver
            .unwrap_or_else(|| Arc::new(NoopEnrollmentDriver));
        let wearable_source = opts
            .wearable_source
            .unwrap_or_else(|| Arc::new(NullWearableSource));

        let core = Arc::new(DaemonCore::new(DaemonCoreInit {
            paths: opts.paths,
            loaded_config: opts.loaded_config,
            env: opts.env,
            cli_overrides: opts.cli_overrides,
            clock: opts.clock,
            broker,
            shutdown_rx: shutdown_rx.clone(),
            persistence: persistence_handle,
            initial_calibration,
            action_executor,
            notifier,
            idle_probe,
            enrollment_driver,
            wearable_source: Arc::clone(&wearable_source),
            engine_ctrl_tx: ctrl_tx.clone(),
        }));

        let wearable_rx = wearable_source.subscribe(shutdown_rx.clone());

        let monitor_task = tokio::spawn(monitoring::run_monitoring_loop(
            Arc::clone(&core),
            wearable_rx,
            ctrl_rx,
            shutdown_rx.clone(),
            initial_detection,
            initial_retention,
        ));

        let core_clone = Arc::clone(&core);
        let socket_for_task = socket_path.clone();
        let install_signals = opts.install_signals;

        if install_signals {
            let core_sleep = Arc::clone(&core);
            let mut shutdown_sleep = shutdown_rx.clone();
            tokio::spawn(async move {
                run_logind_sleep_listener(core_sleep, &mut shutdown_sleep).await;
            });
        }

        let accept_task = tokio::spawn(async move {
            run_accept_loop(
                listener,
                core_clone,
                shutdown_rx,
                &socket_for_task,
                install_signals,
            )
            .await;
        });

        tracing::info!(socket = %socket_path.display(), "bpmute daemon listening");

        Ok(Self {
            core,
            shutdown_tx,
            ctrl_tx,
            monitor_task,
            accept_task,
            persistence_worker,
            socket_path,
        })
    }

    /// Returns the Unix domain socket path bound by this daemon.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Returns a reference to the shared [`DaemonCore`].
    pub fn core(&self) -> &Arc<DaemonCore> {
        &self.core
    }

    /// Publishes an `HrSample` stream event through the drop-oldest broker.
    pub fn publish_hr_sample(&self, sample: Value) {
        self.core.publish_hr_sample(sample);
    }

    /// Injects a timestamped [`GraceOutcome`] event into the live sleep-onset engine.
    pub async fn deliver_grace_outcome(&self, outcome: GraceOutcome) {
        let _ = self
            .ctrl_tx
            .send(EngineControlEvent::GraceOutcome(outcome))
            .await;
    }

    /// Waits until the daemon exits (typically after receiving `SIGINT`/`SIGTERM`),
    /// then flushes the monitoring loop and shuts down the persistence worker cleanly.
    pub async fn wait(self) {
        let _ = self.accept_task.await;
        let _ = self.shutdown_tx.send(true);
        let _ = self.monitor_task.await;
        self.persistence_worker.shutdown_async().await;
    }

    /// Triggers graceful shutdown, drains monitoring and IPC connection tasks,
    /// shuts down the persistence worker, and cleans up the socket file.
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        let _ = self.monitor_task.await;
        let _ = self.accept_task.await;
        self.persistence_worker.shutdown_async().await;
    }
}

async fn prepare_socket_path(socket_path: &Path) -> Result<(), DaemonError> {
    match fs::symlink_metadata(socket_path) {
        Ok(meta) => {
            if !meta.file_type().is_socket() {
                return Err(DaemonError::NonSocketConflict(socket_path.to_path_buf()));
            }
            if UnixStream::connect(socket_path).await.is_ok() {
                return Err(DaemonError::AlreadyRunning(socket_path.to_path_buf()));
            }
            fs::remove_file(socket_path)?;
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(DaemonError::Io(err)),
    }
}

async fn run_accept_loop(
    listener: UnixListener,
    core: Arc<DaemonCore>,
    mut shutdown_rx: watch::Receiver<bool>,
    socket_path: &Path,
    install_signals: bool,
) {
    let mut sighup = if install_signals {
        signal(SignalKind::hangup()).ok()
    } else {
        None
    };
    let mut sigterm = if install_signals {
        signal(SignalKind::terminate()).ok()
    } else {
        None
    };
    let mut sigint = if install_signals {
        signal(SignalKind::interrupt()).ok()
    } else {
        None
    };

    let (conn_shutdown_tx, _) = watch::channel(false);
    let mut conn_tasks: Vec<JoinHandle<()>> = Vec::new();

    loop {
        tokio::select! {
            accept_res = listener.accept() => {
                match accept_res {
                    Ok((stream, _addr)) => {
                        conn_tasks.retain(|h| !h.is_finished());
                        let c = Arc::clone(&core);
                        let rx = conn_shutdown_tx.subscribe();
                        conn_tasks.push(tokio::spawn(async move {
                            handle_connection(stream, c, rx).await;
                        }));
                    }
                    Err(err) => {
                        tracing::error!(error = %err, "unix socket accept error");
                        break;
                    }
                }
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            _ = async {
                match sighup {
                    Some(ref mut sig) => { let _ = sig.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {
                tracing::info!("received SIGHUP; reloading configuration");
                let _ = core.reload_from_disk();
            }
            _ = async {
                match sigterm {
                    Some(ref mut sig) => { let _ = sig.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {
                tracing::info!("received SIGTERM; shutting down daemon");
                break;
            }
            _ = async {
                match sigint {
                    Some(ref mut sig) => { let _ = sig.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {
                tracing::info!("received SIGINT; shutting down daemon");
                break;
            }
        }
    }

    let _ = conn_shutdown_tx.send(true);
    for handle in conn_tasks {
        let _ = handle.await;
    }
    let _ = fs::remove_file(socket_path);
}

async fn run_logind_sleep_listener(core: Arc<DaemonCore>, shutdown: &mut watch::Receiver<bool>) {
    let conn = match zbus::Connection::system().await {
        Ok(c) => c,
        Err(err) => {
            tracing::debug!("system bus unavailable for logind sleep listener: {err}");
            return;
        }
    };

    let manager = match crate::actions::system::LogindManagerProxy::new(&conn).await {
        Ok(m) => m,
        Err(err) => {
            tracing::debug!("LogindManagerProxy unavailable: {err}");
            return;
        }
    };

    let mut sleep_stream = match manager.receive_prepare_for_sleep().await {
        Ok(s) => s,
        Err(err) => {
            tracing::debug!("failed to subscribe to PrepareForSleep signal: {err}");
            return;
        }
    };

    // Acquire unprivileged systemd-logind delay sleep inhibitor
    let mut inhibitor_guard = manager
        .inhibit("sleep", "BPMute", "atomic SQLite WAL flush", "delay")
        .await
        .ok();

    use zbus::export::ordered_stream::OrderedStreamExt;
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            Some(signal) = sleep_stream.next() => {
                if let Ok(args) = signal.args() {
                    let start = args.start;
                    if start {
                        tracing::info!("systemd-logind PrepareForSleep(true) received; executing <= 2s atomic SQLite WAL flush");
                        core.signal_prepare_for_sleep(true).await;
                        let _ = tokio::time::timeout(
                            Duration::from_secs(2),
                            core.persistence.flush_wal(),
                        )
                        .await;
                        // Release delay inhibitor FD so logind can proceed to suspend
                        drop(inhibitor_guard.take());
                    } else {
                        tracing::info!("systemd-logind PrepareForSleep(false) received; system resumed");
                        core.signal_prepare_for_sleep(false).await;
                        // Re-acquire delay sleep inhibitor for future sleep cycles
                        inhibitor_guard = manager
                            .inhibit("sleep", "BPMute", "atomic SQLite WAL flush", "delay")
                            .await
                            .ok();
                    }
                }
            }
        }
    }
    drop(inhibitor_guard);
}
