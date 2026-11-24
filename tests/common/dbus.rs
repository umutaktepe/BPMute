use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

use tempfile::TempDir;

/// RAII guard that kills and reaps a spawned child process (such as `dbus-daemon`) on drop.
pub struct PrivateDbusGuard(pub Child);

impl Drop for PrivateDbusGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawns a private `dbus-daemon` listening on a Unix socket inside `temp` and returns its guard
/// and D-Bus address.
#[allow(
    dead_code,
    reason = "shared across integration test targets that use different subsets of helpers"
)]
pub fn spawn_private_dbus_named(temp: &TempDir, sock_name: &str) -> (PrivateDbusGuard, String) {
    let bus_sock = temp.path().join(sock_name);
    let listen_addr = format!("unix:path={}", bus_sock.display());
    let mut child = Command::new("dbus-daemon")
        .args([
            "--session",
            "--nopidfile",
            "--nosyslog",
            "--nofork",
            "--print-address=1",
            "--address",
            &listen_addr,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("dbus-daemon must spawn");

    let stdout = child.stdout.take().expect("dbus-daemon stdout");
    let mut reader = BufReader::new(stdout);
    let mut addr = String::new();
    reader
        .read_line(&mut addr)
        .expect("read dbus-daemon address");
    let addr = addr.trim().to_string();
    assert!(!addr.is_empty(), "dbus-daemon did not print a bus address");
    (PrivateDbusGuard(child), addr)
}

/// Spawns a private session `dbus-daemon` (`session-bus.sock`) inside `temp`.
#[allow(
    dead_code,
    reason = "shared across integration test targets that use different subsets of helpers"
)]
pub fn spawn_private_session_dbus(temp: &TempDir) -> (PrivateDbusGuard, String) {
    spawn_private_dbus_named(temp, "session-bus.sock")
}
