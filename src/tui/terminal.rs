use std::io::{self, IsTerminal, stdout};
use std::panic;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as CrosstermEvent, EventStream,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::{mpsc, watch};

use super::app::TuiApp;
use super::client::TuiIpcBridge;
use super::event::{TUI_TICK_MS, TuiCommand, TuiEvent};
use super::theme::Theme;
use crate::exit_code::ExitCode;
use crate::paths::{EnvSource, ResolvedPaths};

/// Detects whether a user `bpmute.service` systemd unit is installed in standard unit directories.
pub fn detect_systemd_service_installed(env: &dyn EnvSource) -> bool {
    let mut candidates = Vec::new();
    if let Some(xdg) = env.get("XDG_CONFIG_HOME")
        && !xdg.trim().is_empty()
    {
        candidates.push(PathBuf::from(xdg).join("systemd/user/bpmute.service"));
    } else if let Some(home) = env.get("HOME")
        && !home.trim().is_empty()
    {
        candidates.push(PathBuf::from(home).join(".config/systemd/user/bpmute.service"));
    }
    candidates.push(PathBuf::from("/etc/systemd/user/bpmute.service"));
    candidates.push(PathBuf::from("/usr/lib/systemd/user/bpmute.service"));

    candidates
        .iter()
        .any(|path| path.try_exists().unwrap_or(false))
}

pub async fn run_tui<WOut: io::Write, WErr: io::Write>(
    paths: ResolvedPaths,
    env: &dyn EnvSource,
    no_mouse: bool,
    ascii: bool,
    _stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
    // Verify terminal interactive environment
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        let _ = writeln!(
            stderr,
            "error: interactive terminal required for TUI (try 'bpmute status' or 'bpmute --help')"
        );
        return ExitCode::UsageOrConfigError;
    }

    let theme = Theme::from_env(env, ascii);
    let systemd_installed = detect_systemd_service_installed(env);
    let mut app = TuiApp::new_with_theme(no_mouse, theme, systemd_installed);

    // Terminal setup
    if let Err(err) = enable_raw_mode() {
        let _ = writeln!(stderr, "error: failed to enable terminal raw mode: {err}");
        return ExitCode::RuntimeError;
    }

    let mut out = stdout();
    if let Err(err) = execute!(out, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        let _ = writeln!(stderr, "error: failed to enter alternate screen: {err}");
        return ExitCode::RuntimeError;
    }

    if !no_mouse {
        let _ = execute!(out, EnableMouseCapture);
    }

    // Install panic hook to cleanly restore terminal
    let orig_hook = panic::take_hook();
    let panic_no_mouse = no_mouse;
    panic::set_hook(Box::new(move |panic_info| {
        let mut out = stdout();
        if !panic_no_mouse {
            let _ = execute!(out, DisableMouseCapture);
        }
        let _ = execute!(out, LeaveAlternateScreen);
        let _ = disable_raw_mode();
        orig_hook(panic_info);
    }));

    let backend = CrosstermBackend::new(out);
    let mut terminal = match Terminal::new(backend) {
        Ok(t) => t,
        Err(err) => {
            restore_terminal(no_mouse);
            let _ = writeln!(
                stderr,
                "error: failed to initialize ratatui terminal: {err}"
            );
            return ExitCode::RuntimeError;
        }
    };

    if let Ok(size) = terminal.size() {
        app.last_viewport_width = size.width;
    }

    // IPC client setup
    let (event_tx, mut event_rx) = mpsc::channel::<TuiEvent>(256);
    let (cmd_tx, cmd_rx) = mpsc::channel::<TuiCommand>(64);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let bridge = TuiIpcBridge::new(
        paths.socket_path.clone(),
        event_tx.clone(),
        cmd_rx,
        shutdown_rx,
    );
    let bridge_handle = tokio::spawn(bridge.run());

    // Event streams
    let mut reader = EventStream::new();
    let mut tick_interval = tokio::time::interval(Duration::from_millis(u64::from(TUI_TICK_MS)));

    // Initial render
    let _ = terminal.draw(|f| {
        app.render(f.area(), f.buffer_mut());
    });

    while !app.should_quit {
        tokio::select! {
            _ = tick_interval.tick() => {
                let cmd = app.handle_event(TuiEvent::Tick);
                if let Some(c) = cmd {
                    let _ = cmd_tx.send(c).await;
                }
            }
            crossterm_event = reader.next() => {
                match crossterm_event {
                    Some(Ok(CrosstermEvent::Key(key))) => {
                        let cmd = app.handle_event(TuiEvent::Key(key));
                        if let Some(c) = cmd {
                            let _ = cmd_tx.send(c).await;
                        }
                    }
                    Some(Ok(CrosstermEvent::Mouse(mouse))) => {
                        let cmd = app.handle_event(TuiEvent::Mouse(mouse));
                        if let Some(c) = cmd {
                            let _ = cmd_tx.send(c).await;
                        }
                    }
                    Some(Ok(CrosstermEvent::Resize(w, h))) => {
                        let cmd = app.handle_event(TuiEvent::Resize(w, h));
                        if let Some(c) = cmd {
                            let _ = cmd_tx.send(c).await;
                        }
                    }
                    _ => {}
                }
            }
            Some(tui_event) = event_rx.recv() => {
                let cmd = app.handle_event(tui_event);
                if let Some(c) = cmd {
                    let _ = cmd_tx.send(c).await;
                }
            }
        }

        let _ = terminal.draw(|f| {
            app.render(f.area(), f.buffer_mut());
        });
    }

    // Teardown
    let _ = shutdown_tx.send(true);
    let _ = bridge_handle.await;

    restore_terminal(no_mouse);
    ExitCode::Success
}

fn restore_terminal(no_mouse: bool) {
    let mut out = stdout();
    if !no_mouse {
        let _ = execute!(out, DisableMouseCapture);
    }
    let _ = execute!(out, LeaveAlternateScreen);
    let _ = disable_raw_mode();
}
