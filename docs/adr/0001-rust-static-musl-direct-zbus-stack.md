---
status: accepted
---

# Rust Static `musl` Stack with Direct `zbus` over BlueZ and Offline Main Binary

BPMute v1 is implemented in Rust as a single unified binary (`bpmute`) compiled as truly static `musl` executables (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`) with zero runtime `.so` dependencies, using pure-Rust `zbus` directly for BlueZ instead of `bluer`/`btleplug`, and delegating upgrades to a separate `cargo-dist` `bpmute-update` binary. Keeping the entire D-Bus, Wayland (`wayland-client`/`wayland-protocols`), X11 (`x11rb`), SQLite (`rusqlite` `bundled`), and TUI (`ratatui`/`crossterm`) stack free of C system libraries guarantees cross-distro binary portability across `glibc` versions while keeping the biometric daemon 100% network-free.

## Considered Options

- **Rust with `bluer` or `btleplug` (`bluez-async`):** Provides pre-typed BlueZ bindings, but both pull in `dbus-rs`/`libdbus-1`, forcing either two D-Bus stacks plus dynamic `libdbus-1.so.3`/`glibc` coupling or vendored C `libdbus-src` cross-compilation. BPMute only touches ~6 BlueZ interfaces (`ObjectManager`, `Adapter1`, `Device1`, `GattManager1`/`GattProfile1`, `GattCharacteristic1`, `Battery1`), so a focused internal `zbus::proxy` module is simpler and unifies all five D-Bus surfaces (BlueZ, MPRIS2, `systemd-logind`, Notifications, GNOME `Mutter.IdleMonitor`) on `zbus`.
- **Embedded `axoupdater` library inside `bpmute`:** Avoids shipping a companion `bpmute-update` executable, but compiles an HTTP/TLS stack (`rustls`) into a background daemon that reads local biometric HR data. Using `cargo-dist` `install-updater = true` with a thin `bpmute update` wrapper (which invokes `bpmute-update` and runs `systemctl --user try-restart bpmute.service`) preserves one-command terminal upgrades with zero outbound networking code in `bpmute`.
- **Go or Python:** Go (`godbus` + `bubbletea` + `modernc.org/sqlite`) has strong static-binary ergonomics but lacks a maintained full-featured Linux BlueZ library and pure-Go Wayland `ext-idle-notify-v1` support. Python (`bleak` + `textual`) speeds prototyping but faces PEP 668 install friction (`uv`/`pipx`), higher daemon RSS, and weaker Wayland/X11 idle client libraries.

## Consequences

- BPMute owns and tests a small (~400–600 LOC) BlueZ D-Bus proxy layer over `zbus` rather than delegating to `bluer`.
- Releases target `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` via `cargo-dist` (`curl | sh` and `cargo-binstall`) with no distro-specific packages (`.deb`/`.rpm`/AUR) in v1.
- Users who install via `cargo binstall bpmute` without a `cargo-dist` install receipt receive fallback upgrade instructions from `bpmute update`.
