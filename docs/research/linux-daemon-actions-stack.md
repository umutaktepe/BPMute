# Research: Linux Daemon Integration, Desktop Actions, and Cross-Distro Stack Comparison

> **Issue:** [umutaktepe/BPMute#4](https://github.com/umutaktepe/BPMute/issues/4)
> **Scope:** Primary-source investigation of Linux desktop/system D-Bus and display-server primitives, `systemd --user` daemon integration and IPC trade-offs, and an objective comparison of Rust, Go, and Python ecosystems for BPMute.
> **Verification Date:** 2026-11-19 (all upstream specifications, XML interfaces, source files, and repository metadata verified directly against primary sources).

---

## Executive Summary

1. **Desktop & System Actions (D-Bus):** All three core actions—media pausing, system suspend/power-off, and interactive notifications—are standardized on freedesktop.org D-Bus interfaces.
   - **MPRIS2 (`org.mpris.MediaPlayer2.Player`)** allows enumerating all active media players on the session bus and calling idempotent `Pause()` on every player with `PlaybackStatus == "Playing"`.
   - **`systemd-logind` (`org.freedesktop.login1.Manager`)** permits unprivileged local desktop users to call `Suspend(false)` and `PowerOff(false)` out of the box (`allow_active=yes` in `org.freedesktop.login1.policy`), while blocking inhibitors require admin authentication (`allow_active=auth_admin_keep`).
   - **`org.freedesktop.Notifications`** supports interactive cancel buttons (`ActionInvoked`) and flicker-free countdown updates (`replaces_id`), but clients must check `GetCapabilities()` for `"actions"` and never rely on notifications as the sole cancellation channel.
2. **Cross-Compositor Idle / User-Activity Detection:** No single Linux API covers all compositors.
   - **KDE KWin, Sway, Hyprland, COSMIC, Labwc, niri, river** implement Wayland **`ext-idle-notify-v1` v2**, whose `get_input_idle_notification` **ignores idle inhibitors** (essential when a user falls asleep during fullscreen video playback).
   - **GNOME Mutter** does **not** implement `ext-idle-notify-v1`; it exposes **`org.gnome.Mutter.IdleMonitor`** over session D-Bus (`AddUserActiveWatch()` fires once on next user input).
   - **KDE Plasma Wayland** explicitly rejects `org.freedesktop.ScreenSaver.GetSessionIdleTime` with `QDBusError::NotSupported`.
   - **X11** sessions support **`MIT-SCREEN-SAVER`** (`ScreenSaverQueryInfo` → `idle` ms) and **`XInput2`** raw events (`XI_RawKeyPress`, `XI_RawButtonPress`, `XI_RawMotion`).
3. **`systemd --user` & IPC:** `graphical-session.target` is a passive user target not activated by every standalone window manager, and `bluetooth.target` in `systemd --user` does not guarantee system `bluetoothd` availability—so BPMute must lazily handle session environment availability and watch `org.bluez` dynamically via D-Bus. A **Unix domain socket in `$XDG_RUNTIME_DIR/bpmute/`** is the cleanest primary IPC channel for high-frequency daemon↔TUI telemetry and control.
4. **Language Stack:**
   - **Rust** offers the strongest overall technical fit for a tiny, zero-dependency static binary across distros: pure-Rust `zbus` + pure-Rust Wayland (`wayland-client`/`wayland-protocols`) + pure-Rust X11 (`x11rb`) + `rusqlite` (`bundled`) + `ratatui` + `cargo-dist`. Note that `bluer` and `btleplug`/`bluez-async` pull in C `libdbus-1` via `dbus-rs` unless `zbus` talks to BlueZ directly or `libdbus-sys/vendored` is enabled.
   - **Go** offers the fastest path to `CGO_ENABLED=0` static binaries (`goreleaser`) and best-in-class TUI ergonomics (`bubbletea`) with pure-Go `godbus/dbus/v5` and `modernc.org/sqlite`, but lacks a deep maintained Linux BlueZ library (`muka/go-bluetooth` is archived; `tinygo-org/bluetooth` omits pairing/bonding and `AcquireNotify`) and has a thinner Wayland client ecosystem.
   - **Python** offers the fastest prototyping with turnkey BlueZ BLE (`bleak` + `dbus-fast`) and `textual`, but has the weakest cross-distro single-binary install experience (PEP 668 requires `uv` or `pipx` isolation), higher persistent daemon RAM usage, and weaker Wayland/X11 idle client bindings.

---

## 1. Linux Desktop & System Primitives

### 1.1 Pausing Active Media Players via MPRIS2

The **MPRIS D-Bus Interface Specification v2.2** defines a standard session-bus interface for discovering and controlling compliant media players ([MPRIS2 Specification](https://specifications.freedesktop.org/mpris/latest/), [Player Interface](https://specifications.freedesktop.org/mpris/latest/Player_Interface.html)).

- **Bus Discovery:** Every MPRIS2 player must claim a unique session bus name prefixed with `org.mpris.MediaPlayer2.` (e.g., `org.mpris.MediaPlayer2.vlc`, or multi-instance names appending a unique suffix such as `org.mpris.MediaPlayer2.vlc.instance7389`, per [MPRIS2 Bus Name Policy](https://specifications.freedesktop.org/mpris/latest/index.html)). Clients enumerate active players by calling `org.freedesktop.DBus.ListNames()` on the session bus ([D-Bus Specification](https://dbus.freedesktop.org/doc/dbus-specification.html)) and filtering for the `org.mpris.MediaPlayer2.` prefix.
- **Object Path & Interface:** Every compliant player exposes `/org/mpris/MediaPlayer2` implementing `org.mpris.MediaPlayer2` and `org.mpris.MediaPlayer2.Player` ([MPRIS2 Entry Point](https://specifications.freedesktop.org/mpris/latest/index.html)).
- **Key Properties & Method (`org.mpris.MediaPlayer2.Player`):**
  - `PlaybackStatus` (`s`, read-only): `"Playing"`, `"Paused"`, or `"Stopped"`. Changes emit `org.freedesktop.DBus.Properties.PropertiesChanged` ([Player Interface](https://specifications.freedesktop.org/mpris/latest/Player_Interface.html)).
  - `CanPause` (`b`, read-only) and `CanControl` (`b`, read-only): Indicate whether pausing and general control are permitted.
  - `Pause() -> nothing`: Pauses playback. Per the specification, *"If playback is already paused, this has no effect"* and *"If `CanPause` is false, attempting to call this method should have no effect"* ([Player Interface](https://specifications.freedesktop.org/mpris/latest/Player_Interface.html)).

**Design implications for BPMute:**
- Always call **`Pause()`**, never `PlayPause()`, to avoid toggling an already-paused player into playback.
- Filter targets by `PlaybackStatus == "Playing"` (and `CanPause == true` when present), record which bus names were paused if BPMute ever reports what it paused, and fan out calls concurrently with a short per-player timeout (e.g., 1–2 seconds) so an unresponsive browser tab or sandboxed player cannot stall the pipeline or delay subsequent actions.

---

### 1.2 System Suspend and Power-Off via `systemd-logind` & Polkit

`systemd-logind` exposes power management on the **system bus** at service `org.freedesktop.login1`, object `/org/freedesktop/login1`, interface `org.freedesktop.login1.Manager` ([`org.freedesktop.login1(5)`](https://www.freedesktop.org/software/systemd/man/latest/org.freedesktop.login1.html)).

#### Core D-Bus Methods & Signals

| Member | Signature | Behavior |
|---|---|---|
| `Suspend` | `(in b interactive)` | Suspends the system to RAM. Enforces polkit and non-privileged inhibition locks ([`org.freedesktop.login1(5)`](https://www.freedesktop.org/software/systemd/man/latest/org.freedesktop.login1.html)). |
| `PowerOff` | `(in b interactive)` | Powers off the system. Enforces polkit and non-privileged inhibition locks ([`org.freedesktop.login1(5)`](https://www.freedesktop.org/software/systemd/man/latest/org.freedesktop.login1.html)). |
| `CanSuspend` / `CanPowerOff` | `(out s result)` | Tests hardware support, polkit authorization, and inhibitors. Returns `"na"`, `"yes"`, `"no"`, `"challenge"`, `"inhibited"`, `"inhibitor-blocked"`, or `"challenge-inhibitor-blocked"` ([`org.freedesktop.login1(5)`](https://www.freedesktop.org/software/systemd/man/latest/org.freedesktop.login1.html)). |
| `Inhibit` | `(in s what, in s who, in s why, in s mode) -> (out h pipe_fd)` | Acquires a `block` or `delay` inhibitor lock for colon-separated targets (`shutdown`, `sleep`, `idle`, `handle-lid-switch`, etc.); lock is released when the returned file descriptor is closed ([`org.freedesktop.login1(5)`](https://www.freedesktop.org/software/systemd/man/latest/org.freedesktop.login1.html)). |
| `PrepareForSleep` (signal) | `(b start)` | Broadcast on the system bus immediately before sleep (`true`) and after resume (`false`) ([`org.freedesktop.login1(5)`](https://www.freedesktop.org/software/systemd/man/latest/org.freedesktop.login1.html)). |

#### Default Polkit Authorization (`org.freedesktop.login1.policy`)

Verified directly from upstream [`systemd/src/login/org.freedesktop.login1.policy`](https://raw.githubusercontent.com/systemd/systemd/main/src/login/org.freedesktop.login1.policy):

| Polkit Action ID | `allow_any` | `allow_inactive` | `allow_active` |
|---|---|---|---|
| `org.freedesktop.login1.suspend` | `auth_admin_keep` | `auth_admin_keep` | **`yes`** |
| `org.freedesktop.login1.suspend-multiple-sessions` | `auth_admin_keep` | `auth_admin_keep` | **`yes`** |
| `org.freedesktop.login1.suspend-ignore-inhibit` | `auth_admin_keep` | `auth_admin_keep` | **`auth_admin_keep`** |
| `org.freedesktop.login1.power-off` | `auth_admin_keep` | `auth_admin_keep` | **`yes`** |
| `org.freedesktop.login1.power-off-multiple-sessions` | `auth_admin_keep` | `auth_admin_keep` | **`yes`** |
| `org.freedesktop.login1.power-off-ignore-inhibit` | `auth_admin_keep` | `auth_admin_keep` | **`auth_admin_keep`** |
| `org.freedesktop.login1.inhibit-delay-sleep` / `inhibit-delay-shutdown` | `yes` | `yes` | **`yes`** |
| `org.freedesktop.login1.inhibit-block-sleep` / `inhibit-block-shutdown` | `auth_admin_keep` | `yes` | **`yes`** |

**Design implications for BPMute:**
- **No root privileges required for local active sessions:** Because `allow_active` defaults to `yes` for `suspend`, `suspend-multiple-sessions`, `power-off`, and `power-off-multiple-sessions`, an unprivileged `systemd --user` daemon running for an active local seat session can suspend or shut down the machine without polkit prompts or sudoers rules.
- **Pass `interactive = false`:** When triggering unattended sleep actions, pass `false` so polkit immediately returns an authorization error rather than popping up an interactive password prompt nobody is awake to dismiss.
- **Pre-flight diagnostics:** Call `CanSuspend()` / `CanPowerOff()` in the TUI settings screen and doctor/status view so users are warned immediately if hardware lacks sleep support (`"na"`), a blocking inhibitor is held (`"inhibitor-blocked"`), or a custom distro/SSH policy returns `"challenge"` / `"no"`.
- **Inhibitors & Suspend/Resume awareness:** Respect active blocking inhibitors (`suspend-ignore-inhibit` requires admin auth by default, which is desirable—BPMute should not kill a running OS upgrade or backup). Subscribe to `PrepareForSleep(b)` to cleanly pause/reset BPMute's HR sleep state machine across system suspend and resume.

---

### 1.3 Desktop Notifications via `org.freedesktop.Notifications`

The **Desktop Notifications Specification** defines session-bus service `org.freedesktop.Notifications` at object path `/org/freedesktop/Notifications` ([D-Bus Protocol](https://specifications.freedesktop.org/notification/latest/protocol.html), [Hints](https://specifications.freedesktop.org/notification/latest/hints.html), [Urgency Levels](https://specifications.freedesktop.org/notification/latest/urgency-levels.html)).

- **Capability Discovery (`GetCapabilities() -> as`):** Check whether `"actions"` is advertised before assuming interactive buttons will render. The spec explicitly notes that servers without `"actions"` may ignore action lists, and clients should not assume `ActionInvoked` will always be emitted ([D-Bus Protocol §9.1.1, §9.2.2](https://specifications.freedesktop.org/notification/latest/protocol.html)).
- **Sending & Updating (`Notify`):**
  ```
  UINT32 Notify(
      STRING app_name,
      UINT32 replaces_id,
      STRING app_icon,
      STRING summary,
      STRING body,
      ARRAY<STRING> actions,
      DICT<STRING, VARIANT> hints,
      INT32 expire_timeout
  )
  ```
  - `actions`: Flat even-length array of `[action_key, localized_label]` pairs (e.g., `["cancel", "Cancel Sleep Action"]`; optionally include `["default", "Cancel"]` so clicking the notification body also cancels on servers supporting default invocation).
  - `replaces_id`: Pass `0` for a new notification, or the previously returned `UINT32` ID to atomically update the grace-period countdown in place without flicker or stacking notifications ([D-Bus Protocol §9.1.2](https://specifications.freedesktop.org/notification/latest/protocol.html)).
  - `hints`:
    - `"urgency"` (`BYTE`): `0` = Low (BLE connect/disconnect info), `1` = Normal, `2` = Critical (grace-period countdown before suspend/power-off) ([Urgency Levels](https://specifications.freedesktop.org/notification/latest/urgency-levels.html)).
    - `"transient"` (`BOOLEAN`): `true` tells persistent notification centers (e.g., GNOME Shell, KDE Plasma) not to keep ephemeral status/countdown notifications in history ([Hints](https://specifications.freedesktop.org/notification/latest/hints.html)).
  - `expire_timeout` (`INT32`): `-1` uses server default, `0` never expires until dismissed or closed via `CloseNotification(id)`, `>0` expires after the given milliseconds.
- **Closing & Interaction Signals:**
  - `CloseNotification(UINT32 id)`: Programmatically removes the grace-period notification when cancelled or when the action fires ([D-Bus Protocol §9.1.3](https://specifications.freedesktop.org/notification/latest/protocol.html)).
  - `ActionInvoked(UINT32 id, STRING action_key)`: Emitted when the user clicks an action button or invokes the notification body ([D-Bus Protocol §9.2.2](https://specifications.freedesktop.org/notification/latest/protocol.html)).
  - `NotificationClosed(UINT32 id, UINT32 reason)`: Reason codes `1` = expired, `2` = dismissed by user, `3` = closed by `CloseNotification`, `4` = undefined ([D-Bus Protocol §9.2.1](https://specifications.freedesktop.org/notification/latest/protocol.html)).

**Design implications for BPMute:**
- Subscribe to `ActionInvoked` on the same persistent session D-Bus connection kept open by the daemon; match both `id` and `action_key` (`"cancel"` / `"default"`).
- Treat notification dismissal (`reason == 2`) conservatively (configurable or distinct from explicit cancel, since accidental swipe/timeout varies by daemon), and always close the countdown notification with `CloseNotification(id)` when the grace period terminates.

---

### 1.4 Cross-Compositor User Activity & Idle Detection

For grace-period cancellation, BPMute needs to know **whether the user physically interacted with keyboard or pointer during the grace window**—even if a fullscreen media player is actively holding an idle inhibitor (`zwp_idle_inhibitor_v1`) to prevent screen blanking!

#### Protocol & API Landscape

1. **Wayland `ext-idle-notify-v1` (Staging Protocol, v1 & v2)** ([Wayland Explorer: `ext-idle-notify-v1`](https://wayland.app/protocols/ext-idle-notify-v1))
   - Event-driven (not query-based): after binding global `ext_idle_notifier_v1` and a `wl_seat`, clients create an `ext_idle_notification_v1` object with a `timeout` in milliseconds (a `0` ms timeout is explicitly valid) and receive `idled` and `resumed` events.
   - **Critical v2 distinction:**
     - `get_idle_notification(id, timeout, seat)` (v1): Respects idle inhibitors. If a video player holds `zwp_idle_inhibitor_v1` on a visible surface, the compositor **must not** send `idled`—which prevents ever receiving a subsequent `resumed` event.
     - `get_input_idle_notification(id, timeout, seat)` (**since v2**): Explicitly **ignores idle inhibitors** and tracks only direct user input (keyboard/mouse). When created at the start of BPMute's grace period with `timeout = 0` (or a small threshold like `1000` ms), the compositor emits `idled` immediately once no input is occurring and emits `resumed` the instant the user moves the mouse or presses a key—even during inhibited fullscreen playback ([`ext-idle-notify-v1` spec](https://wayland.app/protocols/ext-idle-notify-v1)).
   - **Verified Compositor Support Matrix** ([Wayland Explorer](https://wayland.app/protocols/ext-idle-notify-v1), [COSMIC `idle_notify.rs`](https://raw.githubusercontent.com/pop-os/cosmic-comp/master/src/wayland/handlers/idle_notify.rs), [KDE `kidletime` Wayland poller](https://raw.githubusercontent.com/KDE/kidletime/master/src/plugins/wayland/poller.cpp)):

     | Compositor / Desktop | `ext_idle_notifier_v1` Version | Notes |
     |---|---|---|
     | **KDE KWin** (6.7) | **v2** | `KIdleTime` uses `ext-idle-notify-v1` (legacy `org_kde_kwin_idle` deprecated). |
     | **Sway** (1.11) / **wlroots** | **v2** | Native wlroots support. |
     | **Hyprland** (0.52.1) | **v2** | Native support. |
     | **COSMIC** (`cosmic-comp` 1.11.0) | **v2** | Via Smithay `IdleNotifierHandler`. |
     | **Labwc** (0.20.2), **niri** (26.04), **river** (0.5.0) | **v2** | Native support. |
     | **Wayfire** (0.9.0) | **v1** | `get_idle_notification` only (affected by inhibitors). |
     | **GNOME Mutter** (51) | **Not supported** | Confirmed absent in [`mutter/src/meson.build`](https://gitlab.gnome.org/GNOME/mutter/-/raw/main/src/meson.build); uses D-Bus `org.gnome.Mutter.IdleMonitor`. |
     | **Weston** (15.0.1), **Muffin** (6.7.4) | **Not supported** | Require D-Bus or fallback cancellation. |

2. **GNOME Mutter D-Bus (`org.gnome.Mutter.IdleMonitor`)** ([`org.gnome.Mutter.IdleMonitor.xml`](https://gitlab.gnome.org/GNOME/mutter/-/raw/main/data/dbus-interfaces/org.gnome.Mutter.IdleMonitor.xml))
   - Exposed on the session bus at `/org/gnome/Mutter/IdleMonitor/Core` (destination `org.gnome.Mutter.IdleMonitor` / `org.gnome.Shell`).
   - Methods: `GetIdletime() -> t` (`uint64` ms), `AddIdleWatch(in t interval) -> out u id`, **`AddUserActiveWatch() -> out u id`**, `RemoveWatch(in u id)`. Signal: `WatchFired(out u id)`.
   - **`AddUserActiveWatch()`** registers a one-shot watch that emits `WatchFired(id)` the next time the user provides input and automatically removes itself—an exact semantic match for BPMute's grace-period cancellation on GNOME (works on both GNOME Wayland and GNOME X11, and tracks raw input independently of `zwp_idle_inhibitor_v1`).

3. **KDE `KIdleTime` & `org.freedesktop.ScreenSaver`**
   - On KDE Plasma Wayland, `kscreenlocker`'s implementation of `org.freedesktop.ScreenSaver.GetSessionIdleTime()` explicitly returns `QDBusError::NotSupported` ([`KDE/kscreenlocker` `interface.cpp`](https://raw.githubusercontent.com/KDE/kscreenlocker/master/interface.cpp), [`org.freedesktop.ScreenSaver.xml`](https://raw.githubusercontent.com/KDE/kscreenlocker/master/dbus/org.freedesktop.ScreenSaver.xml)).
   - Therefore, BPMute must **not** rely on `org.freedesktop.ScreenSaver.GetSessionIdleTime()` on Wayland; KDE Plasma Wayland is served directly via `ext-idle-notify-v1` v2.

4. **X11 (`MIT-SCREEN-SAVER` & `XInput2`)**
   - **`MIT-SCREEN-SAVER` (`ScreenSaverQueryInfo`)** ([X.Org Screen Saver Extension Protocol](https://www.x.org/releases/X11R7.7/doc/scrnsaverproto/saver.html)): Querying the root window returns `idle: CARD32` (milliseconds since last user input). Polling every 250–500 ms *only while the grace period is active* (cancelling if `idle` resets/drops below elapsed grace duration) is lightweight and avoids keeping a permanent event hook open.
   - **`XInput2` Raw Events** ([X.Org XI2 Protocol](https://www.x.org/releases/X11R7.7/doc/inputproto/XI2proto.txt)): Subscribing to `XI_RawKeyPress`, `XI_RawButtonPress`, and `XI_RawMotion` on the root window (`XIAllMasterDevices`) provides zero-polling, focus-independent activity detection on X11.
   - *XWayland caveat:* Under Wayland, X11 queries only observe input delivered to XWayland surfaces, not native Wayland applications—so X11 must only be used when running a genuine X11 session (`WAYLAND_DISPLAY` unset / `XDG_SESSION_TYPE=x11`).

#### Recommended Runtime Probe & Fallback Chain

1. **Wayland (`WAYLAND_DISPLAY` set):** Bind `ext_idle_notifier_v1`. If version ≥ 2, use `get_input_idle_notification(timeout=0, seat)` and cancel grace period on `resumed`. If version == 1, use `get_idle_notification(timeout=0, seat)` and note in diagnostics that active media idle inhibitors may block detection.
2. **GNOME D-Bus fallback:** If `ext_idle_notifier_v1` is absent, probe `/org/gnome/Mutter/IdleMonitor/Core` (`org.gnome.Mutter.IdleMonitor`) and call `AddUserActiveWatch()`, listening for `WatchFired(id)`.
3. **Native X11 (`DISPLAY` set, no `WAYLAND_DISPLAY`):** Use `MIT-SCREEN-SAVER` (`QueryInfo.idle`) or `XInput2` raw events (`XI_RawKeyPress` / `XI_RawButtonPress` / `XI_RawMotion`).
4. **Degraded fallback (unsupported compositors such as Weston or headless sessions):** Log a clear diagnostic warning, surface the capability status in the TUI, and rely on interactive notification cancel (`ActionInvoked`), TUI/CLI `cancel`, and heart-rate recovery above threshold. Avoid reading `/dev/input/event*` (`evdev`) by default, as it requires root or `input` group membership and breaks unprivileged user-session isolation.

---

## 2. `systemd --user` Daemon Best Practices & Daemon–TUI IPC

### 2.1 `systemd --user` Unit Dependencies & Environment

Verified directly from upstream [`systemd.special(7)`](https://www.freedesktop.org/software/systemd/man/latest/systemd.special.html) and [`systemd.service(5)`](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html):

- **`graphical-session.target`:** Defined in `systemd.special(7)` as a *Special Passive User Unit* active whenever a graphical session is running; services tied to the desktop session should set `PartOf=graphical-session.target` and `After=graphical-session.target` so they stop cleanly on logout.
  - **Cross-distro caveat:** `systemd` does not start `graphical-session.target` automatically—desktop environments (GNOME, KDE Plasma, COSMIC) or session wrappers (`uwsm`) bind it. Users on minimal tiling WMs (Sway/Hyprland/i3 launched from TTY without `uwsm` or manual `systemctl --user start graphical-session.target`) will never activate `graphical-session.target`.
  - **Practical strategy:** Ship a default unit targeting `graphical-session.target` for standard DEs (`WantedBy=graphical-session.target`, `PartOf=graphical-session.target`, `After=graphical-session.target dbus.socket`), **or** target `default.target` with lazy/retrying display-server discovery when a grace period actually starts (since BPMute only needs `WAYLAND_DISPLAY`/`DISPLAY` during grace-period idle checks, not at daemon boot). If using `default.target`, avoid `PartOf=graphical-session.target` so the daemon does not fail to start on non-systemd-session WMs.
- **`dbus.socket`:** On modern systemd distros (`dbus-broker` or `dbus-daemon` user socket activation), the session bus socket lives at `$XDG_RUNTIME_DIR/bus` (`%t/bus`) and `DBUS_SESSION_BUS_ADDRESS` is propagated into the user manager environment. Ordering `After=dbus.socket` ensures the session bus socket exists before startup (`Type=dbus` adds this implicitly per `systemd.service(5)`).
- **`bluetooth.target`:** `systemd.special(7)` confirms `bluetooth.target` exists in the *user* manager as a hardware target similar to the system counterpart. However, `systemd --user` cannot depend on system units (`bluetooth.service`), and Bluetooth adapters or `bluetoothd` can restart at any time. Treat `After=bluetooth.target` purely as an optional hint and always watch `org.bluez` ownership dynamically via D-Bus `NameOwnerChanged`.
- **`WAYLAND_DISPLAY` & `DISPLAY` availability:** Because `systemd --user` runs outside the compositor's process tree, `WAYLAND_DISPLAY`, `DISPLAY`, and `XAUTHORITY` are only present inside user services after the DE/compositor runs `systemctl --user import-environment` or `dbus-update-activation-environment --systemd`. Connecting to Wayland/X11 **lazily at grace-period start** (and inspecting `$XDG_RUNTIME_DIR` / `systemd --user show-environment` if a cached env var is missing) makes the daemon resilient to startup race conditions.
- **Restart Policy (`systemd.service(5)`):**
  - Use `Restart=on-failure` (never `Restart=always`, which fights clean exits), paired with `RestartSec=3s` and exponential backoff (`RestartSteps=4`, `RestartMaxDelaySec=60s` on systemd ≥ 254, per [`systemd.service(5)`](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html)).
  - Handle `SIGTERM` / `SIGINT` cleanly with exit code `0` after closing BLE notifications and IPC sockets.
  - Default slice (`app.slice`) or `Slice=background.slice` (for low-latency-insensitive background monitoring, per [`systemd.special(7)`](https://www.freedesktop.org/software/systemd/man/latest/systemd.special.html)).

Example resilient `systemd --user` unit sketch:
```ini
[Unit]
Description=BPMute heart-rate sleep automation daemon
After=dbus.socket graphical-session.target bluetooth.target
PartOf=graphical-session.target

[Service]
Type=simple
ExecStart=%h/.local/bin/bpmute daemon
Restart=on-failure
RestartSec=3s
NoNewPrivileges=true

[Install]
WantedBy=graphical-session.target
```
*(For minimal WMs that do not activate `graphical-session.target`, an installer/docs option can substitute `WantedBy=default.target` and drop `PartOf=graphical-session.target`.)*

### 2.2 Daemon–TUI IPC: Unix Domain Socket vs. Session D-Bus

| Dimension | Unix Domain Socket (`$XDG_RUNTIME_DIR/bpmute/daemon.sock`) | Session D-Bus (`org.bpmute.Daemon`) |
|---|---|---|
| **Security & Lifecycle** | Per [XDG Base Directory Specification](https://specifications.freedesktop.org/basedir/latest/), `$XDG_RUNTIME_DIR` is owned exclusively by the user with mode `0700`, resides on a local filesystem/tmpfs, and is cleaned on logout. Peer UID can also be verified via `SO_PEERCRED`. | Authenticated per-user via session bus (`$XDG_RUNTIME_DIR/bus`). |
| **Live Telemetry Streaming** | Ideal for 1 Hz HR stream, rolling graph history, state-machine transitions, and grace-period countdowns via newline-delimited JSON (NDJSON) or CBOR over a persistent stream socket. | Awkward for rich nested telemetry snapshots; requires D-Bus variant/struct marshaling and property-change boilerplate. |
| **External Scriptability** | Requires calling `bpmute status --json` / `bpmute cancel` CLI wrapper. | Directly callable from `busctl`, `gdbus`, Waybar/Polybar scripts without invoking the CLI. |
| **Bus Coupling** | Works even if session D-Bus restarts or in minimal SSH/TTY sessions. | Fails if session bus is unavailable. |
| **systemd Integration** | Supports `systemd` socket activation (`bpmute.socket`) if desired. | Supports `Type=dbus` + `BusName=` readiness notification and D-Bus activation. |

**Recommendation:** Use a **Unix domain socket in `$XDG_RUNTIME_DIR/bpmute/daemon.sock`** with newline-delimited JSON (NDJSON) for primary daemon↔TUI/CLI communication. It minimizes serialization friction for live HR graphing and state snapshots, works independently of D-Bus restarts, and keeps the daemon interface clean. If desktop bar integration (Waybar/GNOME extensions) is later desired, expose either `bpmute status --json` or a small read-only D-Bus interface as a secondary layer.

---

## 3. Ecosystem Comparison: Rust vs. Go vs. Python for BPMute

### 3.1 Summary Matrix

| Criterion | **Rust** | **Go** | **Python** |
|---|---|---|---|
| **Linux BlueZ BLE** | **`bluer`** ([`bluez/bluer`](https://github.com/bluez/bluer) v0.17.4, official BlueZ org; deepest BlueZ API, **links C `libdbus-1`**); **`btleplug`** ([`deviceplug/btleplug`](https://github.com/deviceplug/btleplug) v0.13.5 via `bluez-async` v0.9.0; also links `libdbus-1`); or **direct `zbus`** (pure Rust, zero C deps). | **`tinygo-org/bluetooth`** ([v0.17.0](https://github.com/tinygo-org/bluetooth), pure Go via `godbus`, **no Pair/Bond/AcquireNotify**); `muka/go-bluetooth` is **archived** (2024-07). Best Linux depth requires direct **`godbus/dbus/v5`**. | **`bleak`** ([`hbldh/bleak`](https://github.com/hbldh/bleak) v3.1.0, MIT, actively maintained) built on **`dbus-fast`** ([v5.2.0](https://github.com/Bluetooth-Devices/dbus-fast)). Supports `StartNotify` & `AcquireNotify`; pairing/agents require direct `dbus-fast`. |
| **D-Bus Bindings** | **`zbus`** ([`z-galaxy/zbus`](https://github.com/z-galaxy/zbus) v6.3.0, MIT): pure Rust, async, `#[proxy]` macros, zero C deps. | **`godbus/dbus`** ([`godbus/dbus`](https://github.com/godbus/dbus) v5.2.4, BSD-2-Clause): mature pure Go, zero CGo, more manual variant plumbing. | **`dbus-fast`** ([`Bluetooth-Devices/dbus-fast`](https://github.com/Bluetooth-Devices/dbus-fast) v5.2.0, MIT): fast `asyncio` D-Bus (optional Cython speedup, pure-Python fallback). |
| **Wayland & X11 Idle Clients** | **Pure-Rust wire clients for both:** `wayland-client` v0.31.14 + `wayland-protocols` v0.32.13 (`ext-idle-notify-v1`, no `libwayland-client.so`) and `x11rb` v0.14.0 (no `libX11`/`libXss`). | Pure-Go X11 via `jezek/xgb`; Wayland client libraries are fragmented (small custom wire client over Unix socket is feasible but manual). | `pywayland` (CFFI binding to host `libwayland-client`); `python-xlib` unmaintained since 2023-04 (or `ctypes` to `libXss.so.1`). |
| **TUI Framework** | **`ratatui`** ([`ratatui/ratatui`](https://github.com/ratatui/ratatui) v0.30.2, MIT, ~23.4k stars): immediate-mode, crisp charts/sparklines, minimal RAM, more state boilerplate. | **`bubbletea`** ([`charmbracelet/bubbletea`](https://github.com/charmbracelet/bubbletea) v2.1.2, MIT, ~45.8k stars) + Lip Gloss/Bubbles: Elm architecture, very ergonomic interactive TUI. | **`textual`** ([`Textualize/textual`](https://github.com/Textualize/textual) v8.2.8, MIT, ~37.7k stars): CSS-styled reactive widgets, fastest UI iteration, higher RSS & startup time. |
| **Embedded SQLite** | **`rusqlite`** ([v0.43.0](https://github.com/rusqlite/rusqlite), `bundled` statically embeds SQLite amalgamation, musl-safe); **`sqlx`** ([`transact-rs/sqlx`](https://github.com/transact-rs/sqlx), async). | **`mattn/go-sqlite3`** (v1.14.53, requires **CGo**); **`modernc.org/sqlite`** ([pkg.go.dev](https://pkg.go.dev/modernc.org/sqlite), pure Go) & **`ncruces/go-sqlite3`** (v0.37.0, WASM) preserve `CGO_ENABLED=0`. | Stdlib **[`sqlite3`](https://docs.python.org/3/library/sqlite3.html)** (built-in) + **`aiosqlite`** ([`omnilib/aiosqlite`](https://github.com/omnilib/aiosqlite)) for `asyncio`. |
| **Cross-Distro GitHub Install & Update** | Single static musl binary (if avoiding dynamic `libdbus-1`) or old-glibc binary; **`cargo-dist`** (v0.34.0) generates GitHub Releases + `curl \| sh` + `cargo-binstall` + updater. Zero runtime deps. | Single truly static `CGO_ENABLED=0` binary out of the box (with `godbus` + `modernc.org/sqlite`); **`goreleaser`** (v2.20.2) generates GitHub Releases, `.deb`/`.rpm`, `curl \| sh`. Zero runtime deps. | Blocked from bare `pip` by PEP 668 on modern distros; requires **`uv tool install`** ([`astral-sh/uv`](https://docs.astral.sh/uv/guides/tools/) v0.13.9) or **`pipx`** ([`pypa/pipx`](https://github.com/pypa/pipx) v1.18.19). Bootstraps Python + venv. |
| **Daemon Footprint (Typical)** | Lowest (~5–15 MB RSS, instant CLI startup). | Low (~10–25 MB RSS, instant CLI startup). | Highest (~40–80+ MB RSS with `textual`/Python runtime; noticeable CLI startup latency). |

---

### 3.2 Deep-Dive Notes & Verified Gotchas

#### Rust
- **The `libdbus-1` split (verified from upstream `Cargo.toml` files):**
  - Both [`bluez/bluer` `Cargo.toml`](https://raw.githubusercontent.com/bluez/bluer/master/bluer/Cargo.toml) (`v0.17.4`) and [`deviceplug/btleplug` `Cargo.toml`](https://raw.githubusercontent.com/deviceplug/btleplug/master/Cargo.toml) (`v0.13.5`, via [`bluez-rs/bluez-async` `Cargo.toml`](https://raw.githubusercontent.com/bluez-rs/bluez-async/main/bluez-async/Cargo.toml) `v0.9.0`) depend on `dbus` (`0.9`, `dbus-rs`) → [`libdbus-sys`](https://raw.githubusercontent.com/diwic/dbus-rs/master/libdbus-sys/Cargo.toml) (`v0.2.7`), which links C **`libdbus-1`** via `pkg-config` by default (`vendored` feature compiles `libdbus-src` from C source).
  - By contrast, **[`zbus`](https://raw.githubusercontent.com/z-galaxy/zbus/main/zbus/Cargo.toml) (`v6.3.0`)** is **100% pure Rust** with zero C dependencies. Because BPMute's BLE client only needs a small subset of BlueZ D-Bus (`org.freedesktop.DBus.ObjectManager`, `org.bluez.Adapter1`, `org.bluez.Device1`, `org.bluez.GattCharacteristic1` — including both `StartNotify` and `AcquireNotify`), calling BlueZ directly through `zbus` (or vendoring `libdbus-sys` if `bluer` is chosen) unlocks **true static `musl` binaries** (`x86_64-unknown-linux-musl` / `aarch64-unknown-linux-musl`, [Rust Platform Support](https://doc.rust-lang.org/rustc/platform-support.html)) with zero system `.so` dependencies—even for Wayland ([`wayland-client` `Cargo.toml`](https://raw.githubusercontent.com/Smithay/wayland-rs/master/wayland-client/Cargo.toml), pure Rust wire backend when `system` feature is off), X11 ([`x11rb` `Cargo.toml`](https://raw.githubusercontent.com/psychon/x11rb/master/x11rb/Cargo.toml), pure Rust wire backend by default), and SQLite ([`rusqlite` `Cargo.toml`](https://raw.githubusercontent.com/rusqlite/rusqlite/master/Cargo.toml) `bundled`).
- **Distribution:** [`cargo-dist`](https://github.com/axodotdev/cargo-dist) (`v0.34.0`) + [`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall) (`v1.27.0`) automate GitHub Releases, shell installers (`curl -LsSf ... | sh`), SHA-256 checksums, and optional built-in self-updaters (`axoupdater`).

#### Go
- **BlueZ BLE gap (verified from upstream source):**
  - [`muka/go-bluetooth`](https://github.com/muka/go-bluetooth) is **archived** (last push `2024-07-01`).
  - [`tinygo-org/bluetooth`](https://github.com/tinygo-org/bluetooth) (`v0.17.0`) is actively maintained and uses pure-Go `godbus/dbus/v5` on Linux ([`adapter_linux.go`](https://raw.githubusercontent.com/tinygo-org/bluetooth/release/adapter_linux.go)), keeping `CGO_ENABLED=0` intact. However, inspecting [`gap_linux.go`](https://raw.githubusercontent.com/tinygo-org/bluetooth/release/gap_linux.go) and [`gattc_linux.go`](https://raw.githubusercontent.com/tinygo-org/bluetooth/release/gattc_linux.go) confirms it only calls `StartNotify` (no `AcquireNotify`), exposes no BlueZ `Pair`/`Trusted`/`Bonded`/`RemoveDevice` methods, and targets a lowest-common-denominator embedded/desktop API.
  - Consequently, a production Linux-first Go daemon is better served writing a focused ~300–500 LOC BlueZ D-Bus client directly over [`godbus/dbus/v5`](https://github.com/godbus/dbus) (`v5.2.4`), which already handles the other D-Bus interfaces (MPRIS2, logind, Notifications, Mutter IdleMonitor).
- **CGo vs. Static Builds:** Using `mattn/go-sqlite3` forces CGo ([Go `cgo` docs](https://pkg.go.dev/cmd/cgo)), complicating cross-distro glibc compatibility. Pairing `godbus/dbus/v5` with [`modernc.org/sqlite`](https://pkg.go.dev/modernc.org/sqlite) or [`ncruces/go-sqlite3`](https://github.com/ncruces/go-sqlite3) keeps `CGO_ENABLED=0` for effortless single-binary Linux `amd64`/`arm64` releases via [`goreleaser`](https://github.com/goreleaser/goreleaser) (`v2.20.2`).
- **TUI:** [`bubbletea`](https://github.com/charmbracelet/bubbletea) (`v2.1.2`) offers arguably the smoothest developer experience for rich terminal UIs in a compiled binary.

#### Python
- **Turnkey BLE & TUI:** [`bleak`](https://github.com/hbldh/bleak) (`v3.1.0`) is battle-tested across hundreds of consumer BLE wearables on Linux BlueZ via [`dbus-fast`](https://github.com/Bluetooth-Devices/dbus-fast) (`v5.2.0`), with documented support for both `StartNotify` and `AcquireNotify` ([Bleak Linux Backend Docs](https://raw.githubusercontent.com/hbldh/bleak/develop/docs/backends/linux.rst)). [`textual`](https://github.com/Textualize/textual) (`v8.2.8`) provides fast CSS-driven TUI development on the same `asyncio` event loop.
- **Distribution & Desktop-Client Friction:**
  - Modern Debian/Ubuntu/Fedora/Arch enforce PEP 668 (`externally-managed-environment`), so bare `pip install` fails. Frictionless terminal installation from GitHub requires [`uv tool install`](https://docs.astral.sh/uv/guides/tools/) / `uv tool upgrade` or [`pipx`](https://github.com/pypa/pipx) (`v1.18.19`), which means users either pre-install `uv`/`pipx` or run a bootstrap script that installs `uv` and a managed Python runtime.
  - Wayland/X11 idle clients are weaker than in Rust: [`pywayland`](https://github.com/flacjacket/pywayland) requires CFFI + host `libwayland-client` and protocol generation; [`python-xlib`](https://github.com/python-xlib/python-xlib) has seen no commits since 2023-04.
  - Persistent daemon RSS (~40–80+ MB) and CLI startup latency are noticeably higher than Rust or Go.

---

### 3.3 Decision Guidance for BPMute

- **Choose Rust** if **zero-dependency single-binary distribution, lowest background daemon footprint, and first-class native Linux desktop protocol support** (pure-Rust `zbus` + pure-Rust `wayland-client`/`ext-idle-notify-v1` + pure-Rust `x11rb` + bundled SQLite + `ratatui`) are top priorities.
  - *Implementation note:* Evaluate whether BPMute uses **`zbus` directly for BlueZ** (100% pure Rust, effortless musl static binary, unified D-Bus stack across BlueZ/MPRIS2/logind/Notifications/Mutter) vs. **`bluer`** (richer out-of-the-box BlueZ types, accepting either dynamic `libdbus-1.so.3` or `libdbus-sys/vendored`).
- **Choose Go** if **fast development velocity, effortless `CGO_ENABLED=0` cross-distro releases (`goreleaser`), and `bubbletea` TUI ergonomics** outweigh writing a small bespoke BlueZ D-Bus wrapper on `godbus/dbus/v5` and a small Wayland `ext-idle-notify-v1` wire client.
- **Choose Python** if **fastest BLE prototyping (`bleak`) and richest out-of-the-box TUI widgets (`textual`)** outweigh `uv`/`pipx`-based installation, higher resident daemon memory, and CFFI/`ctypes` glue for Wayland/X11 idle detection.

---

## Primary Sources

- MPRIS D-Bus Interface Specification v2.2: https://specifications.freedesktop.org/mpris/latest/ and https://specifications.freedesktop.org/mpris/latest/Player_Interface.html
- D-Bus Specification: https://dbus.freedesktop.org/doc/dbus-specification.html
- Desktop Notifications Specification: https://specifications.freedesktop.org/notification/latest/protocol.html, https://specifications.freedesktop.org/notification/latest/hints.html, https://specifications.freedesktop.org/notification/latest/urgency-levels.html
- `systemd-logind` D-Bus API (`org.freedesktop.login1(5)`): https://www.freedesktop.org/software/systemd/man/latest/org.freedesktop.login1.html
- `systemd-logind` Polkit Policy (`org.freedesktop.login1.policy`): https://raw.githubusercontent.com/systemd/systemd/main/src/login/org.freedesktop.login1.policy
- `systemd.special(7)` & `systemd.service(5)`: https://www.freedesktop.org/software/systemd/man/latest/systemd.special.html and https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html
- XDG Base Directory Specification (`$XDG_RUNTIME_DIR`): https://specifications.freedesktop.org/basedir/latest/
- Wayland `ext-idle-notify-v1` Protocol & Compositor Matrix: https://wayland.app/protocols/ext-idle-notify-v1
- GNOME Mutter `org.gnome.Mutter.IdleMonitor.xml` & `src/meson.build`: https://gitlab.gnome.org/GNOME/mutter/-/raw/main/data/dbus-interfaces/org.gnome.Mutter.IdleMonitor.xml and https://gitlab.gnome.org/GNOME/mutter/-/raw/main/src/meson.build
- KDE `kidletime` Wayland Poller & `kscreenlocker` `org.freedesktop.ScreenSaver`: https://raw.githubusercontent.com/KDE/kidletime/master/src/plugins/wayland/poller.cpp, https://raw.githubusercontent.com/KDE/kscreenlocker/master/dbus/org.freedesktop.ScreenSaver.xml, and https://raw.githubusercontent.com/KDE/kscreenlocker/master/interface.cpp
- COSMIC Compositor `ext-idle-notify-v1` Handler: https://raw.githubusercontent.com/pop-os/cosmic-comp/master/src/wayland/handlers/idle_notify.rs
- X.Org Screen Saver Extension (`MIT-SCREEN-SAVER`) & XInput2 Protocols: https://www.x.org/releases/X11R7.7/doc/scrnsaverproto/saver.html and https://www.x.org/releases/X11R7.7/doc/inputproto/XI2proto.txt
- Rust Ecosystem Primary Sources:
  - `bluer`: https://github.com/bluez/bluer (`bluer/Cargo.toml`)
  - `btleplug` & `bluez-async`: https://github.com/deviceplug/btleplug and https://github.com/bluez-rs/bluez-async
  - `libdbus-sys`: https://github.com/diwic/dbus-rs (`libdbus-sys/Cargo.toml`)
  - `zbus`: https://github.com/z-galaxy/zbus (`zbus/Cargo.toml`)
  - `wayland-rs` & `x11rb`: https://github.com/Smithay/wayland-rs and https://github.com/psychon/x11rb
  - `ratatui`: https://github.com/ratatui/ratatui
  - `rusqlite` & `sqlx`: https://github.com/rusqlite/rusqlite and https://github.com/transact-rs/sqlx
  - `cargo-dist`, `cargo-binstall`, Rust Platform Support: https://github.com/axodotdev/cargo-dist, https://github.com/cargo-bins/cargo-binstall, https://doc.rust-lang.org/rustc/platform-support.html
- Go Ecosystem Primary Sources:
  - `tinygo-org/bluetooth`: https://github.com/tinygo-org/bluetooth (`adapter_linux.go`, `gap_linux.go`, `gattc_linux.go`)
  - `muka/go-bluetooth` (archived): https://github.com/muka/go-bluetooth
  - `godbus/dbus`: https://github.com/godbus/dbus
  - `bubbletea`: https://github.com/charmbracelet/bubbletea
  - `mattn/go-sqlite3`, `modernc.org/sqlite`, `ncruces/go-sqlite3`: https://github.com/mattn/go-sqlite3, https://pkg.go.dev/modernc.org/sqlite, https://github.com/ncruces/go-sqlite3
  - `goreleaser` & `cgo`: https://github.com/goreleaser/goreleaser and https://pkg.go.dev/cmd/cgo
- Python Ecosystem Primary Sources:
  - `bleak` & Linux backend docs: https://github.com/hbldh/bleak (`docs/backends/linux.rst`)
  - `dbus-fast`: https://github.com/Bluetooth-Devices/dbus-fast
  - `textual`: https://github.com/Textualize/textual
  - `sqlite3` & `aiosqlite`: https://docs.python.org/3/library/sqlite3.html and https://github.com/omnilib/aiosqlite
  - `pywayland` & `python-xlib`: https://github.com/flacjacket/pywayland and https://github.com/python-xlib/python-xlib
  - `uv` Tools & `pipx`: https://docs.astral.sh/uv/guides/tools/ and https://github.com/pypa/pipx
