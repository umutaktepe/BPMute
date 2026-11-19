pub mod clock;
pub mod state;

use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use thiserror::Error;
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::config::{CliOverrides, LoadedConfig};
use crate::ipc::pubsub::EventBroker;
use crate::ipc::server::handle_connection;
use crate::paths::{EnvSource, ResolvedPaths, ensure_private_dir};

pub use clock::{DaemonClock, SystemClock, VirtualClock};
pub use state::DaemonCore;

/// Errors that prevent `bpmute daemon` from starting or binding its socket.
#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("daemon is already running on socket {0}")]
    AlreadyRunning(PathBuf),
    #[error("socket path {0} exists and is not a Unix domain socket")]
    NonSocketConflict(PathBuf),
    #[error("daemon I/O error: {0}")]
    Io(#[from] io::Error),
}

/// Startup parameters for [`DaemonHandle::spawn`].
pub struct DaemonOptions {
    pub paths: ResolvedPaths,
    pub loaded_config: LoadedConfig,
    pub env: Arc<dyn EnvSource>,
    pub cli_overrides: CliOverrides,
    pub clock: Arc<dyn DaemonClock>,
    pub install_signals: bool,
}

/// Running background daemon instance with graceful shutdown and IPC control handles.
pub struct DaemonHandle {
    core: Arc<DaemonCore>,
    shutdown_tx: watch::Sender<bool>,
    task: JoinHandle<()>,
    socket_path: PathBuf,
}

impl DaemonHandle {
    /// Binds the `0600` Unix domain socket, starts the accept loop, and returns a handle.
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

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let broker = Arc::new(EventBroker::default());
        let core = Arc::new(DaemonCore::new(
            opts.paths,
            opts.loaded_config,
            opts.env,
            opts.cli_overrides,
            opts.clock,
            broker,
            shutdown_rx.clone(),
        ));

        let core_clone = Arc::clone(&core);
        let socket_for_task = socket_path.clone();
        let install_signals = opts.install_signals;

        let task = tokio::spawn(async move {
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
            task,
            socket_path,
        })
    }

    /// Returns the Unix domain socket path bound by this daemon.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Publishes an `HrSample` stream event through the drop-oldest broker.
    pub fn publish_hr_sample(&self, sample: Value) {
        self.core.publish_hr_sample(sample);
    }

    /// Waits until the daemon task exits (typically after receiving `SIGINT`/`SIGTERM`).
    pub async fn wait(self) {
        let _ = self.task.await;
    }

    /// Triggers graceful shutdown, drains connection tasks, and cleans up the socket file.
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        let _ = self.task.await;
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
