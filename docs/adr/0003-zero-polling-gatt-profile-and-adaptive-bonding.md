---
status: accepted
---

# Zero-Polling BlueZ `GattProfile1` Auto-Reconnect and Adaptive Bonding

BPMute v1 delegates background Wearable reconnection entirely to the Linux kernel's passive Accept List / Resolving List via a stable `org.bluez.GattProfile1` (`0000180d-0000-1000-8000-00805f9b34fb`) registered on all powered Bluetooth adapters, paired with an **adaptive bonding** enrollment flow (`Device1.Trusted = true` always; SMP `Pair()` only when the Wearable uses a Resolvable Private Address, is already paired, or rejects unbonded GATT access). This delivers near-zero idle CPU and radio overhead across every mainstream distro without requiring `Experimental = true`, `root`, or unnecessary pairing prompts on Random Static wearables.

## Considered Options

- **BlueZ `org.bluez.AdvertisementMonitorManager1`:** Allows pattern-matching advertisements without bonding, including unbondable RPA devices, but remains behind `Experimental = true` in `/etc/bluetooth/main.conf` across Ubuntu, Debian, Fedora, Arch, and openSUSE by default.
- **Periodic / continuous `Adapter1.StartDiscovery()`:** Works without bonding or `GattProfile1`, but runs a 100% duty-cycle active scan (`11.25 ms / 11.25 ms`), causing continuous D-Bus churn, battery drain, and Wi-Fi/BLE coexistence degradation.
- **Always call `Device1.Pair()` during enrollment:** Simplifies branching, but triggers avoidable OS/watch pairing dialogs and compatibility edge cases on static-address Wearables (such as the Huawei Watch GT5 and Polar H10) that connect, subscribe, and passively auto-reconnect cleanly with `Trusted = true` alone.

## Consequences

- BPMute exports a D-Bus `ObjectManager` root containing `org.bluez.GattProfile1` (`0x180D`) on every powered adapter and must enforce its own enrolled-Wearable allowlist before subscribing, because BlueZ's `GattProfile1` auto-connects any non-temporary `0x180D` device known to the adapter.
- Wearables that both rotate RPAs and refuse BLE SMP bonding cannot auto-reconnect passively in v1; BPMute warns during enrollment and requires manual reconnect via TUI/CLI instead of running background active scans.
- Because SMP `Pair()` is still required for RPA-rotating Wearables, enrollment registers a temporary caller-scoped `org.bluez.Agent1` (never `RequestDefaultAgent()`) so pairing succeeds on both minimal Wayland/TTY environments and full desktop environments without hijacking unrelated Bluetooth pairings.
