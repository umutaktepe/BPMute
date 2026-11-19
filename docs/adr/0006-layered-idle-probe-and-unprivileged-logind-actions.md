---
status: accepted
---

# Layered Cross-Compositor Idle Probe, Unprivileged `logind` Actions, and Asymmetric Grace-Period Safety

BPMute v1 detects desktop user activity for both `2 min` pre-trigger gating and Grace Period cancellation via a pure-Rust, unprivileged runtime probe chain—Wayland `ext-idle-notify-v1` v2 (`get_input_idle_notification`), GNOME `org.gnome.Mutter.IdleMonitor`, Wayland `ext-idle-notify-v1` v1, and native X11 (`MIT-SCREEN-SAVER` + `XInput2`)—never reading `/dev/input/event*` (`evdev`) or calling `org.freedesktop.ScreenSaver.GetSessionIdleTime`. Power actions execute strictly via unprivileged `systemd-logind` (`Suspend(false)` / `PowerOff(false)` with `interactive = false`), respect active blocking inhibitors, and pair a `delay` sleep inhibitor for atomic SQLite WAL flushes with a distinct `Snooze` vs. `Cancel` calibration split and asymmetric `Signal Lost` handling during the Grace Period.

## Considered Options

- **Reading `/dev/input/event*` (`evdev`) for universal compositor-agnostic input detection:** Works on every Wayland compositor and TTY, but requires root privileges or `input` group membership (exposing every keystroke system-wide to a background daemon) and breaks zero-privilege cross-distro installation.
- **`org.freedesktop.ScreenSaver.GetSessionIdleTime` as a unified D-Bus idle API:** Appears desktop-agnostic, but KDE Plasma Wayland (`kscreenlocker`) explicitly rejects it with `QDBusError::NotSupported`, and v1 inhibitor-aware idle APIs stay suppressed during fullscreen media playback—the exact scenario in which users fall asleep.
- **Treating `NotificationClosed(reason = 2)` as cancellation or unifying `Snooze` into `Cancel`:** Swiping/focus-dismissing notifications varies across Linux notification daemons and risks silent missed sleep triggers, while lacking a zero-penalty `Snooze` forces sleepy users who want to finish a video to either poison their calibration with a false-positive penalty or disarm the daemon manually.
- **Uniformly aborting (or uniformly executing) all actions on `Signal Lost` during the Grace Period:** Allowing `suspend`/`poweroff` to finish after `Signal Lost` risks shutting down the host when a user removes their Wearable, whereas aborting non-destructive `pause_media` causes frequent missed pauses when a sleeping user rolls onto their wrist and momentarily breaks optical PPG lock.

## Consequences

- On Wayland compositors supporting `ext-idle-notify-v1` v2 (KDE KWin, Sway, Hyprland, COSMIC, Labwc, niri, river) and on GNOME (`Mutter.IdleMonitor`), input detection remains fully functional even while fullscreen video players hold `zwp_idle_inhibitor_v1`.
- Unsupported compositors or Wayland v1 sessions degrade gracefully to notification buttons, TUI/CLI `cancel`, and physiological HR arousal aborts while enforcing a `≥ 90 s` Grace Period safety floor and visible diagnostic warnings for `suspend`/`poweroff`.
- Holding an unprivileged `logind` `delay` sleep inhibitor (`≤ 2 s` flush budget) guarantees that `Monitoring Session` state and `sleep_triggered_at` reach SQLite before suspend freeze, enabling reliable `< 5 min` rapid-resume false-positive detection versus `w = 0.35` uncancelled-suspend calibration credit on `PrepareForSleep(false)`.
- `pause_media` never auto-resumes playback on cancellation or wake, and blocking `logind` inhibitors (such as OS upgrades or backups) are never bypassed or retried in a loop.
