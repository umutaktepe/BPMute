# BLE Heart Rate Profile (`0x180D`) and BlueZ D-Bus Auto-Reconnect on Linux

**Issue:** [umutaktepe/BPMute#3](https://github.com/umutaktepe/BPMute/issues/3)  
**Scope:** Primary-source technical reference for consuming Bluetooth Low Energy (BLE) Heart Rate broadcasts on Linux via BlueZ D-Bus, covering GATT payload formats, RPA/IRK resolution, low-overhead background reconnection, Linux distribution permissions/packaging, and documented smartwatch broadcast quirks.

---

## Executive Summary

1. **GATT Heart Rate Measurement (`0x2A37`) is variable-length, little-endian, and Notify-only.** Parsers must decode the leading `Flags` byte dynamically (8-bit vs. 16-bit BPM, 2-bit sensor contact semantics, optional Energy Expended, and `0..n` trailing `uint16` RR-intervals in `1/1024 s` units). Battery Service (`0x180F`) is claimed by BlueZ's built-in `battery` plugin, so applications should read `org.bluez.Battery1.Percentage` on the device object rather than enabling GATT notifications on `0x2A19`.
2. **Low-overhead background auto-reconnect on Linux does not require active scanning or experimental BlueZ flags.** Registering a stable `org.bluez.GattProfile1` object for `0x180D` via `org.bluez.GattManager1.RegisterApplication()` causes BlueZ to add bonded, non-temporary devices to the Linux kernel's passive auto-connect list (`MGMT_OP_ADD_DEVICE`, `Action = 0x02`). The kernel then uses controller-filtered passive scanning (Accept List + Resolving List with persisted IRKs) with zero userspace polling or D-Bus wakeups until the wearable begins advertising.
3. **Bonding and `Device1.Trusted = true` are both required for reliable RPA-rotating wearables.** BlueZ explicitly refuses to persist or enable kernel auto-connect for unresolved private addresses (`device_address_is_private()`), and calling `Disconnect()` on an untrusted LE device pauses auto-connect until `Connect()` is explicitly invoked again.
4. **Modern mainstream Linux distros require no `bluetooth` group, root privileges, or polkit rules for BlueZ D-Bus.** Since BlueZ 5.51 (commit `3ef0ce95`), upstream `bluetooth.conf` permits all local users (`<policy context="default"><allow send_destination="org.bluez"/>`) to talk to `org.bluez`. However, all mainstream distros keep `Experimental = false` at runtime in `/etc/bluetooth/main.conf`, meaning `org.bluez.AdvertisementMonitorManager1` cannot be relied on out of the box.
5. **Smartwatch HR broadcast modes have major vendor quirks.** Most watches require manual or workout-triggered activation, may drop the phone companion link (single-connection Huawei wearables), omit RR-intervals on optical wrist broadcast (Garmin watches), expose unreliable skin-contact flags on optical sensors (Polar Verity Sense/OH1), or reject CCCD writes with `NotPermitted` without proprietary auth on legacy models (older Amazfit/Mi Band). BPMute must treat RR-intervals and sensor-contact bits as optional and never depend on them for core mute/unmute logic.

---

## 1. Bluetooth SIG GATT Specifications: Heart Rate (`0x180D`) and Battery (`0x180F`)

All standard 16-bit Bluetooth SIG UUIDs expand into the Bluetooth Base UUID `0000XXXX-0000-1000-8000-00805f9b34fb` ([Bluetooth SIG Assigned Numbers — `service_uuids.yaml`](https://bitbucket.org/bluetooth-SIG/public/src/main/assigned_numbers/uuids/service_uuids.yaml)). All multi-byte integer fields in GATT characteristics are **little-endian** unsigned integers.

### 1.1 Heart Rate Service (`0x180D`)

Defined by the [Bluetooth SIG Heart Rate Service (HRS) v1.0](https://www.bluetooth.com/specifications/specs/heart-rate-service-1-0/), service UUID `0000180d-0000-1000-8000-00805f9b34fb` exposes three characteristics:

| Characteristic | 16-bit UUID | Requirement | Properties | Purpose | Primary Spec |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **Heart Rate Measurement** | `0x2A37` | Mandatory | `Notify` (via CCCD `0x2902`) | Streams BPM, sensor contact, optional energy expended, and optional RR-intervals | [GSS `heart_rate_measurement.yaml`](https://bitbucket.org/bluetooth-SIG/public/src/main/gss/org.bluetooth.characteristic.heart_rate_measurement.yaml) |
| **Body Sensor Location** | `0x2A38` | Optional | `Read` | Static `uint8` enum: `0x00` Other, `0x01` Chest, `0x02` Wrist, `0x03` Finger, `0x04` Hand, `0x05` Ear Lobe, `0x06` Foot (`0x07–0xFF` RFU) | [GSS `body_sensor_location.yaml`](https://bitbucket.org/bluetooth-SIG/public/src/main/gss/org.bluetooth.characteristic.body_sensor_location.yaml) |
| **Heart Rate Control Point** | `0x2A39` | Conditional (mandatory if Energy Expended is supported) | `Write` | `uint8` command: `0x01` resets accumulated Energy Expended to 0 (`0x00`, `0x02–0xFF` RFU) | [GSS `heart_rate_control_point.yaml`](https://bitbucket.org/bluetooth-SIG/public/src/main/gss/org.bluetooth.characteristic.heart_rate_control_point.yaml) |

### 1.2 Heart Rate Measurement (`0x2A37`) Bit-Level Payload Layout

Per the official [Bluetooth SIG GATT Specification Supplement (`org.bluetooth.characteristic.heart_rate_measurement.yaml`)](https://bitbucket.org/bluetooth-SIG/public/src/main/gss/org.bluetooth.characteristic.heart_rate_measurement.yaml), each notification payload has a variable length determined strictly by byte 0 (`Flags`):

#### Byte 0: `Flags` (`boolean[8]`, 1 byte, always present)

| Bit(s) | Mask | Field | Semantics |
| :--- | :--- | :--- | :--- |
| Bit `0` | `0x01` | Heart Rate Value Format | `0` = `uint8` (1 byte, 0–255 BPM); `1` = `uint16` little-endian (2 bytes, BPM) |
| Bits `1–2` | `0x06` | Sensor Contact Status | Bit 2 (`0x04`): Sensor Contact Supported; Bit 1 (`0x02`): Sensor Contact Detected. Combined `(flags >> 1) & 0x03`: `0b00` or `0b01` = feature **not supported**; `0b10` = supported, contact **not detected**; `0b11` = supported, contact **detected** |
| Bit `3` | `0x08` | Energy Expended Status | `0` = Energy Expended field absent; `1` = `uint16` Energy Expended field present |
| Bit `4` | `0x10` | RR-Interval Present | `0` = no RR-Interval values present; `1` = one or more `uint16` RR-Interval values follow |
| Bits `5–7` | `0xE0` | Reserved for Future Use (RFU) | Ignore when parsing |

#### Subsequent Payload Fields (Sequential Offsets)

1. **Heart Rate Measurement Value** (mandatory, starting at offset `1`):
   - If `flags & 0x01 == 0`: 1 byte (`uint8`), next offset = `2`.
   - If `flags & 0x01 == 1`: 2 bytes (`uint16` LE), next offset = `3`.
   - Unit: beats per minute (`org.bluetooth.unit.period.beats_per_minute`).
2. **Energy Expended** (optional, present iff `flags & 0x08 != 0`):
   - 2 bytes (`uint16` LE), advances offset by `2`.
   - *Spec unit note:* HRS v1.0 historically specifies **kilojoules (kJ)** (see also [Polar BLE SDK `BleHrClient.kt`](https://github.com/polarofficial/polar-ble-sdk/blob/master/sources/Android/android-communications/library/src/main/java/com/polar/androidcommunications/api/ble/model/gatt/client/BleHrClient.kt)), whereas the machine-readable [GSS YAML](https://bitbucket.org/bluetooth-SIG/public/src/main/gss/org.bluetooth.characteristic.heart_rate_measurement.yaml) lists base unit `org.bluetooth.unit.energy.joule`. Clients not consuming calorie metrics should simply advance the byte offset by 2 whenever bit 3 is set so RR-interval parsing stays aligned.
3. **RR-Interval(s)** (optional, present iff `flags & 0x10 != 0`):
   - `n` contiguous `uint16` LE values (`size = n * 2`) spanning from the current offset to the end of the notification payload (`n = (len - offset) / 2`).
   - **Resolution:** Each value is in units of **`1/1024` second** (`ms = raw_rr * 1000.0 / 1024.0`).
   - **Ordering:** `RR-Interval Value 0` (lowest offset) is the **oldest** interval since the previous transmission; `RR-Interval Value n-1` (highest offset) is the **newest** ([GSS `heart_rate_measurement.yaml`](https://bitbucket.org/bluetooth-SIG/public/src/main/gss/org.bluetooth.characteristic.heart_rate_measurement.yaml)). Because notifications typically fire at ~1 Hz while heart rates exceed 60 BPM, a single packet frequently carries 0, 1, 2, or more RR-intervals.
   - **Defensive bounds checking:** Reject malformed packets shorter than `2` bytes (or `3` if bit 0 is set, `+2` if bit 3 is set), and ignore a trailing odd byte if `len - offset` is not even.

### 1.3 Battery Service (`0x180F`) and BlueZ Claimed-Service Behavior

- **Specification:** [Bluetooth SIG Battery Service (BAS)](https://www.bluetooth.com/specifications/specs/battery-service/) (`0x180F`) contains the mandatory **Battery Level** characteristic (`0x2A19`), a single `uint8` in `0..100` representing percentage charge (`0` = fully discharged, `100` = fully charged; `101–255` reserved) ([GSS `battery_level.yaml`](https://bitbucket.org/bluetooth-SIG/public/src/main/gss/org.bluetooth.characteristic.battery_level.yaml)).
- **Critical BlueZ Integration Caveat:** Upstream BlueZ ships a built-in `battery` plugin ([`profiles/battery/battery.c`](https://github.com/bluez/bluez/blob/master/profiles/battery/battery.c)) that registers a `btd_profile` for `BATTERY_UUID` (`0x180F`) and **claims** the GATT service. By default in [`src/main.conf`](https://github.com/bluez/bluez/blob/master/src/main.conf), `[GATT] ExportClaimedServices = read-only`, which causes BlueZ's GATT client ([`src/gatt-client.c`](https://github.com/bluez/bluez/blob/master/src/gatt-client.c)) to return `org.bluez.Error.NotAuthorized` if external clients attempt to write to descriptors of a claimed service.
- **Recommended access path on Linux:** Read and subscribe to `PropertiesChanged` on BlueZ's [`org.bluez.Battery1`](https://github.com/bluez/bluez/blob/master/doc/org.bluez.Battery.rst) interface (`Percentage` property, `byte`) exported directly on `/org/bluez/hciX/dev_XX_XX_XX_XX_XX_XX`, rather than calling `StartNotify()` on GATT characteristic `0x2A19`.

---

## 2. BlueZ D-Bus Architecture, RPA/IRK Resolution, and Auto-Reconnect

All D-Bus interfaces below are verified directly against upstream BlueZ documentation (`doc/org.bluez.*.rst`) and C implementation (`src/`), plus Linux kernel Bluetooth HCI/MGMT sources (`net/bluetooth/`).

### 2.1 Core D-Bus Interfaces

| Interface | Object Path | Stability | Role in BPMute | Primary Docs |
| :--- | :--- | :--- | :--- | :--- |
| `org.bluez.Adapter1` | `/org/bluez/hciX` | Stable | Initial LE scanning (`SetDiscoveryFilter`, `StartDiscovery`, `StopDiscovery`), `Powered`, `RemoveDevice` | [`doc/org.bluez.Adapter.rst`](https://github.com/bluez/bluez/blob/master/doc/org.bluez.Adapter.rst) |
| `org.bluez.Device1` | `/org/bluez/hciX/dev_AA_BB_CC_DD_EE_FF` | Stable | `Pair()`, `Connect()`, `Disconnect()`, `Trusted`, `Paired`, `Bonded`, `Connected`, `ServicesResolved`, `Disconnected` signal | [`doc/org.bluez.Device.rst`](https://github.com/bluez/bluez/blob/master/doc/org.bluez.Device.rst) |
| `org.bluez.GattManager1` | `/org/bluez/hciX` | Stable | `RegisterApplication()` to register a client `GattProfile1` hierarchy | [`doc/org.bluez.GattManager.rst`](https://github.com/bluez/bluez/blob/master/doc/org.bluez.GattManager.rst) |
| `org.bluez.GattProfile1` | Client-owned path under `ObjectManager` | Stable | Declares `UUIDs = ["0000180d-0000-1000-8000-00805f9b34fb"]` to opt bonded devices into kernel passive auto-connect | [`doc/org.bluez.GattProfile.rst`](https://github.com/bluez/bluez/blob/master/doc/org.bluez.GattProfile.rst) |
| `org.bluez.GattCharacteristic1` | `.../dev_.../service#/char#` | Stable | `StartNotify()`, `StopNotify()`, `ReadValue()`, `Value` (`PropertiesChanged`), `Notifying` | [`doc/org.bluez.GattCharacteristic.rst`](https://github.com/bluez/bluez/blob/master/doc/org.bluez.GattCharacteristic.rst) |
| `org.bluez.AgentManager1` / `Agent1` | `/org/bluez` / client path | Stable | Optional pairing agent (`NoInputNoOutput` or `DisplayYesNo`) when no desktop agent is running | [`doc/org.bluez.AgentManager.rst`](https://github.com/bluez/bluez/blob/master/doc/org.bluez.AgentManager.rst), [`doc/org.bluez.Agent.rst`](https://github.com/bluez/bluez/blob/master/doc/org.bluez.Agent.rst) |
| `org.bluez.AdvertisementMonitorManager1` / `AdvertisementMonitor1` | `/org/bluez/hciX` / client path | **Experimental** (`Experimental = true` required) | Pattern-based passive advertisement monitoring (`or_patterns`) without `StartDiscovery()` | [`doc/org.bluez.AdvertisementMonitorManager.rst`](https://github.com/bluez/bluez/blob/master/doc/org.bluez.AdvertisementMonitorManager.rst), [`doc/org.bluez.AdvertisementMonitor.rst`](https://github.com/bluez/bluez/blob/master/doc/org.bluez.AdvertisementMonitor.rst) |

### 2.2 Scanning, Pairing/Bonding, Trusting, and RPA/IRK Resolution

1. **Filtered Initial Discovery (Onboarding Only):**
   - Call `Adapter1.SetDiscoveryFilter({"Transport": "le", "UUIDs": ["0000180d-0000-1000-8000-00805f9b34fb"]})` before `Adapter1.StartDiscovery()` ([`doc/org.bluez.Adapter.rst`](https://github.com/bluez/bluez/blob/master/doc/org.bluez.Adapter.rst)).
   - Note that BlueZ merges discovery filters across concurrent D-Bus clients with logical OR, so BPMute must still verify that discovered `Device1.UUIDs` actually contain `0x180D`. Always call `StopDiscovery()` promptly once onboarding finishes.

2. **Pairing vs. Bonding and RPA / IRK Resolution:**
   - `Device1.Paired` indicates encrypted pairing completed; `Device1.Bonded` indicates keys (LTK, IRK, etc.) were persisted to `/var/lib/bluetooth/<adapter>/<device>/info` ([`doc/org.bluez.Device.rst`](https://github.com/bluez/bluez/blob/master/doc/org.bluez.Device.rst)).
   - Modern smartwatches advertise using **Resolvable Private Addresses (RPAs)** that rotate periodically (typically every ~15 minutes). Before pairing, `Device1.Address` reflects the transient RPA (`AddressType = "random"`).
   - During LE bonding, the kernel receives the peer's 16-byte **Identity Resolving Key (IRK)** and **Identity Address** (public or static random) and emits a **New Identity Resolving Key** event (`0x0018`) to `bluetoothd` ([`doc/mgmt-protocol.rst`](https://github.com/bluez/bluez/blob/master/doc/mgmt-protocol.rst)). BlueZ updates `Device1.Address` and `Device1.AddressType` to the permanent Identity Address, clears the `temporary` flag, and persists the IRK ([`src/device.c`](https://github.com/bluez/bluez/blob/master/src/device.c)).
   - On every adapter startup/boot, BlueZ feeds all persisted IRKs into the kernel via **`MGMT_OP_LOAD_IRKS` (`0x0030`)** ([`src/adapter.c`](https://github.com/bluez/bluez/blob/master/src/adapter.c), [`doc/mgmt-protocol.rst`](https://github.com/bluez/bluez/blob/master/doc/mgmt-protocol.rst)). When the controller supports Link Layer Privacy (`ll_privacy_capable(hdev)`), the kernel programs both the controller **Accept List** and **Resolving List** during passive scanning (`hci_update_passive_scan_sync` in [`net/bluetooth/hci_sync.c`](https://github.com/torvalds/linux/blob/master/net/bluetooth/hci_sync.c)); otherwise the kernel resolves incoming RPAs in software against the loaded IRKs.
   - **Hard constraint in BlueZ:** `device_address_is_private()` ([`src/device.c`](https://github.com/bluez/bluez/blob/master/src/device.c)) checks whether `bdaddr_type == BDADDR_LE_RANDOM` and `(bdaddr.b[5] >> 6)` is `0x00` (non-resolvable private) or `0x01` (resolvable private). If true (i.e., an unbonded device using an RPA without an exchanged IRK), **both `store_device_info()` and `device_set_auto_connect()` return immediately and refuse to persist or auto-connect the device.**
   - **Agent handling for headless/TUI setups:** `Device1.Pair()` fails if neither an application agent nor a desktop default agent is registered ([`doc/org.bluez.Device.rst`](https://github.com/bluez/bluez/blob/master/doc/org.bluez.Device.rst)). If a desktop agent (GNOME/KDE/blueman) is already registered, calling `Pair()` uses it automatically; for headless or TUI-only environments, BPMute can register a scoped `org.bluez.Agent1` (capability `DisplayYesNo` for Numeric Comparison on smartwatches, or `NoInputNoOutput` for Just Works chest straps) via `AgentManager1.RegisterAgent()` during pairing without usurping the global default agent.

3. **Why `Device1.Trusted = true` Matters:**
   - In [`src/device.c`](https://github.com/bluez/bluez/blob/master/src/device.c) (`device_pause_auto_connect`, called from `dev_disconnect`), BlueZ explicitly executes:
     ```c
     if (!device->auto_connect || device->trusted)
         return;
     device->disable_auto_connect = TRUE;
     device_set_auto_connect(device, FALSE);
     ```
   - Thus, for an untrusted LE device, any explicit `Disconnect()` call disables kernel passive-scan auto-connect until `Connect()` is called again (also documented in [`doc/org.bluez.Device.rst`](https://github.com/bluez/bluez/blob/master/doc/org.bluez.Device.rst)). Setting `Device1.Trusted = true` ensures kernel auto-reconnect remains active across disconnects.

### 2.3 Comparing Background Reconnection Mechanisms (CPU / Radio Overhead)

| Mechanism | BlueZ Stability | Linux Kernel Scan Behavior & Duty Cycle | Works Across RPA Rotation? | Overhead & Caveats |
| :--- | :--- | :--- | :--- | :--- |
| **`org.bluez.GattProfile1` (`GattManager1.RegisterApplication`)** — **Recommended** | **Stable** (since BlueZ 5.40–5.42; no `Experimental` flag needed) | Sends `MGMT_OP_ADD_DEVICE` (`0x0033`, `Action = 0x02`) → `hci_update_passive_scan_sync()` programs controller Accept List + Resolving List and runs **passive** scan at `DISCOV_LE_SCAN_INT_FAST = 60 ms` / `DISCOV_LE_SCAN_WIN_FAST = 30 ms` (50% passive RX, no `SCAN_REQ` TX; tunable via `ScanIntervalAutoConnect` / `ScanWindowAutoConnect`) | **Yes** (once bonded & IRK loaded; works in controller hardware if LL Privacy supported) | **Zero userspace CPU/D-Bus traffic while idle.** Requires device to be non-temporary, have `0x180D` in known UUIDs, and not have `PreferredBearer == "bredr"`. |
| **`org.bluez.AdvertisementMonitor1` (`or_patterns`)** | **Experimental** (`GDBUS_EXPERIMENTAL_ASYNC_METHOD` in [`src/adv_monitor.c`](https://github.com/bluez/bluez/blob/master/src/adv_monitor.c); requires `Experimental = true` in `main.conf`) | Passive scan (`DISCOV_LE_SCAN_INT_FAST = 60 ms` / `WIN = 30 ms`), optionally offloaded to controller (`controller-patterns`), interleaved with allowlist/no-filter windows (`300 ms` / `500 ms` defaults) | Matches AD payloads (e.g., 16-bit UUID `0x180D` little-endian `0D 18` in AD type `0x02`/`0x03`) even **without** bonding | Unavailable on default Ubuntu/Debian/Fedora/Arch/openSUSE installs unless user enables `Experimental = true` and restarts `bluetoothd`. |
| **Continuous `Adapter1.StartDiscovery()`** | Stable | **Active** scan (`SCAN_REQ`/`SCAN_RSP` TX on every advertisement) at `DISCOV_LE_SCAN_INT = 11.25 ms` / `DISCOV_LE_SCAN_WIN = 11.25 ms` (**100% duty cycle**) | Sees any advertising device | **High battery/radio/CPU overhead:** continuous D-Bus `InterfacesAdded`/`PropertiesChanged` churn for all nearby BLE beacons; known to degrade existing BLE connections and Wi-Fi coexistence (e.g., [Bleak #1290](https://github.com/hbldh/bleak/issues/1290)). Avoid as a standing loop. |

*Kernel scan constant sources:* [`include/net/bluetooth/hci_core.h`](https://github.com/torvalds/linux/blob/master/include/net/bluetooth/hci_core.h) (`DISCOV_LE_SCAN_INT = 0x0012` [11.25 ms], `DISCOV_LE_SCAN_WIN = 0x0012` [11.25 ms], `DISCOV_LE_SCAN_INT_FAST = 0x0060` [60 ms], `DISCOV_LE_SCAN_WIN_FAST = 0x0030` [30 ms]), initialized in [`net/bluetooth/hci_core.c`](https://github.com/torvalds/linux/blob/master/net/bluetooth/hci_core.c), and configurable in [`src/main.conf`](https://github.com/bluez/bluez/blob/master/src/main.conf) `[LE]`.

### 2.4 End-to-End `GattProfile1` Auto-Reconnect Lifecycle and Edge Cases

1. **Daemon startup:** BPMute exports a D-Bus `org.freedesktop.DBus.ObjectManager` root (e.g., `/org/bpmute/gatt`) with a child object implementing `org.bluez.GattProfile1` whose `UUIDs` property is `["0000180d-0000-1000-8000-00805f9b34fb"]`, and calls `GattManager1.RegisterApplication("/org/bpmute/gatt", {})` on the active adapter.
2. **Kernel auto-connect activation:** In [`src/gatt-database.c`](https://github.com/bluez/bluez/blob/master/src/gatt-database.c) (`profile_add`), BlueZ registers an external `btd_profile` with `auto_connect = true` and `accept = profile_device_accept`. For every non-temporary known device whose UUIDs include `0x180D`, `probe_service()` in [`src/device.c`](https://github.com/bluez/bluez/blob/master/src/device.c) calls `device_set_auto_connect(device, TRUE)` → `adapter_auto_connect_add()` in [`src/adapter.c`](https://github.com/bluez/bluez/blob/master/src/adapter.c) → `MGMT_OP_ADD_DEVICE` (`Action = 0x02`).
3. **Wearable starts HR Broadcast:** The kernel's passive scan sees the advertisement (resolving any rotated RPA via IRK), establishes the LE ACL connection automatically, and BlueZ emits `PropertiesChanged` on `Device1` (`Connected = true`, followed by `ServicesResolved = true`).
4. **Subscribing to `0x2A37` (`StartNotify` vs. `AcquireNotify`):**
   - Prefer **`GattCharacteristic1.StartNotify()`** (not `AcquireNotify()`). Per [`doc/org.bluez.GattCharacteristic.rst`](https://github.com/bluez/bluez/blob/master/doc/org.bluez.GattCharacteristic.rst) and [Bleak Linux backend docs](https://github.com/hbldh/bleak/blob/develop/docs/backends/linux.rst), `StartNotify()` has broader device compatibility (Bleak reverted an `AcquireNotify` default back to `StartNotify` in v3.0.2 due to device incompatibilities).
   - **Reconnect notification behavior:** In [`src/gatt-client.c`](https://github.com/bluez/bluez/blob/master/src/gatt-client.c), `btd_gatt_client_disconnected()` closes all `AcquireNotify` socket FDs (`HUP`), whereas for `StartNotify()` clients it clears `notify_id = 0` and attempts automatic re-registration inside `btd_gatt_client_ready()` on reconnect (when `[GATT] Cache = always` is active and handles are unchanged). If re-registration fails or characteristics were rebuilt, BlueZ drops the client and `Notifying` becomes `false`. Therefore, BPMute should watch `Device1.ServicesResolved == true`, inspect `GattCharacteristic1.Notifying`, and call `StartNotify()` whenever `Notifying` is `false`.
5. **Dual-mode smartwatch pitfall:** If a smartwatch is also paired over BR/EDR (e.g., for calls/audio) and `PreferredBearer` is set to `"bredr"` (or BR/EDR was last preferred), `device_set_auto_connect()` in [`src/device.c`](https://github.com/bluez/bluez/blob/master/src/device.c) explicitly skips adding the LE device to the kernel auto-connect list. Users should pair the watch strictly as an LE HR peripheral when dedicated to BPMute.
6. **Authentication-failure backoff:** On `Reason.Authentication` disconnects, BlueZ disables auto-connect and retries with exponential backoff (1 s, 2 s, 4 s) before stopping ([`src/device.c`](https://github.com/bluez/bluez/blob/master/src/device.c)); BPMute should listen for the `Device1.Disconnected(reason, message)` signal and surface a clear "re-pair device" diagnostic if `org.bluez.Reason.Authentication` occurs.
7. **Fallback for unbonded/non-IRK peripherals:** Some simple chest straps (e.g., Polar H10 in default unbonded mode) use a **static** public or random address without requiring bonding—for those, `device_address_is_private()` is `false`, so once connected once (marking `temporary = false` after GATT discovery), `GattProfile1` auto-connect still works even without bonding! Only devices that rotate RPAs *and* refuse bonding/IRK exchange require an optional fallback (either `AdvertisementMonitor1` when `Experimental = true`, or a user-triggered/low-frequency bounded scan).

---

## 3. Linux Permissions, Packaging Prerequisites, and BlueZ Version Differences

### 3.1 D-Bus Policy, `bluetooth` Group, and Polkit Across Mainstream Distros

Historically (BlueZ ≤ 5.50), `bluetooth.conf` denied `context="default"` and only allowed `at_console="true"` and `group="lp"` (with Debian patching in `group="bluetooth"`). In **November 2018** (upstream commit [`3ef0ce95`](https://github.com/bluez/bluez/commit/3ef0ce95), shipped from **BlueZ 5.51** onward), BlueZ removed the deprecated `at_console` rule and changed the default system bus policy in [`src/bluetooth.conf`](https://github.com/bluez/bluez/blob/master/src/bluetooth.conf) to:

```xml
<policy context="default">
  <allow send_destination="org.bluez"/>
</policy>
```

Verified directly against current upstream packaging repositories:

| Distribution | Typical BlueZ Version | `bluetooth.conf` Path & Default Access Policy | `bluetooth` Group Needed? | Runtime `Experimental` Default | Primary Packaging Source |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **Ubuntu 22.04 / 24.04 LTS** | `5.64` (22.04) / `5.72` (24.04) | `/usr/share/dbus-1/system.d/bluetooth.conf` — inherits Debian policy (`context="default"` allows `org.bluez`; `bluetooth` group exists via `sysusers`) | **No** (unless sysadmin hardens `context="default"` to `deny`) | `false` (compiled with `--enable-experimental`, off in `main.conf`) | [Debian `bluez` Salsa `debian/rules`](https://salsa.debian.org/bluetooth-team/bluez/-/blob/debian/sid/debian/rules), [`debian/bluez.sysusers`](https://salsa.debian.org/bluetooth-team/bluez/-/blob/debian/sid/debian/bluez.sysusers) |
| **Debian 12 / 13 / Sid** | `5.66` (12) / `5.82+` (13) / `5.88` (Sid) | `/usr/share/dbus-1/system.d/bluetooth.conf` — `context="default"` allows `org.bluez` (legacy `bluetooth.conf.patch` added `group="bluetooth"` above `context="default" allow`, dropped in Sid) | **No** (same hardening caveat) | `false` (`--enable-experimental` at build, off in `main.conf`) | [Debian `bluez` Salsa `debian/patches/series`](https://salsa.debian.org/bluetooth-team/bluez/-/blob/debian/sid/debian/patches/series) |
| **Fedora** | `5.88` (Rawhide) / `5.77+` (recent releases) | `/usr/share/dbus-1/system.d/bluetooth.conf` — unpatched upstream `src/bluetooth.conf` (`context="default"` allow) | **No** | `false` (`--enable-experimental` at build, off in `main.conf`) | [Fedora `bluez.spec`](https://src.fedoraproject.org/rpms/bluez/blob/rawhide/f/bluez.spec) |
| **Arch Linux** | `5.88` (rolling) | `/usr/share/dbus-1/system.d/bluetooth.conf` — unpatched upstream `src/bluetooth.conf` (`context="default"` allow) | **No** | `false` (`--enable-experimental` at build, off in `main.conf`) | [Arch Linux `bluez` `PKGBUILD`](https://gitlab.archlinux.org/archlinux/packaging/packages/bluez/-/blob/main/PKGBUILD) |
| **openSUSE** | `5.88` (Tumbleweed/Factory) | `/usr/share/dbus-1/system.d/bluetooth.conf` — unpatched upstream `src/bluetooth.conf` (`context="default"` allow) | **No** | `false` (`--enable-experimental` at build, off in `main.conf`) | [openSUSE `Base:System/bluez` `bluez.spec`](https://build.opensuse.org/package/show/Base:System/bluez) |

### 3.2 Key Permission, Sandbox, and BlueZ Configuration Takeaways

- **Polkit is NOT used by `bluetoothd`:** BlueZ's daemon does not perform polkit authorization checks for `Adapter1`, `Device1`, `GattManager1`, or `GattCharacteristic1`. Polkit rules (e.g., `/etc/polkit-1/rules.d/51-blueman.rules`) only affect GUI wrappers like `blueman` (for `org.blueman.rfkill.setstate` or `org.blueman.network.setup`) or NetworkManager—not a direct D-Bus client ([Arch Wiki Bluetooth](https://wiki.archlinux.org/title/Bluetooth)).
- **Do NOT use raw HCI sockets:** Opening `AF_BLUETOOTH`/`SOCK_RAW` directly bypasses `bluetoothd` and requires `root` or `CAP_NET_RAW`/`CAP_NET_ADMIN` capabilities. Using BlueZ D-Bus works completely unprivileged on all four distros.
- **Hardened systems & sandboxes:**
  - If a user gets `org.freedesktop.DBus.Error.AccessDenied` on a custom-hardened Debian/Ubuntu host, instruct them to join the `bluetooth` group (`sudo usermod -aG bluetooth "$USER"`).
  - **Flatpak** packages require `--system-talk-name=org.bluez`; **Snap** packages require the `bluez` plug; containerized/user-namespace environments must ensure the D-Bus `EXTERNAL` auth UID matches the host UID (cf. [Bleak Linux backend docs](https://github.com/hbldh/bleak/blob/develop/docs/backends/linux.rst)).
- **BlueZ daemon readiness:** `bluetooth.service` ([`src/bluetooth.service.in`](https://github.com/bluez/bluez/blob/master/src/bluetooth.service.in)) is not always enabled by default on minimal Arch/server installs (`systemctl enable --now bluetooth.service`). In addition, adapters must be powered (`Adapter1.Powered = true`; upstream BlueZ `[Policy] AutoEnable=true` is now the default in [`src/main.conf`](https://github.com/bluez/bluez/blob/master/src/main.conf)).
- **BlueZ version & `Experimental` caveats:**
  - All supported mainstream releases ship BlueZ ≥ `5.64` (Ubuntu 22.04 `5.64`, Debian 12 `5.66`, Ubuntu 24.04 `5.72`, Fedora/Arch/openSUSE `5.77–5.88`).
  - `org.bluez.GattProfile1` (auto-connect), `org.bluez.Battery1`, `Device1.ServicesResolved`, and `[GATT] Cache = always` are **stable across all of them**.
  - `org.bluez.AdvertisementMonitorManager1` (introduced in BlueZ `5.55`, D-Bus policy added in commit [`9c8538c1`](https://github.com/bluez/bluez/commit/9c8538c1)), `Adapter1.ConnectDevice()`, and `Device1.PreferredBearer` remain marked **experimental** in BlueZ master (`src/adv_monitor.c`, `doc/org.bluez.Device.rst`) and are hidden unless `/etc/bluetooth/main.conf` sets `Experimental = true` (or `bluetoothd -E`).
  - **Kernel LL Privacy:** On older kernels where controller-side RPA resolution was experimental, BlueZ exposes `KernelExperimental = 15c0a148-c273-11ea-b3de-0242ac130004` (`rpa_resolution_uuid` in [`src/adapter.c`](https://github.com/bluez/bluez/blob/master/src/adapter.c) and [`src/main.conf`](https://github.com/bluez/bluez/blob/master/src/main.conf)); modern mainline Linux kernels expose `MGMT_SETTING_LL_PRIVACY` automatically when hardware supports it ([`net/bluetooth/mgmt.c`](https://github.com/torvalds/linux/blob/master/net/bluetooth/mgmt.c)).

---

## 4. Documented Smartwatch HR Broadcast Mode Quirks

Standard BLE Heart Rate chest straps advertise `0x180D` whenever worn, whereas general-purpose smartwatches do **not** advertise `0x180D` continuously (to preserve battery and phone connectivity). Each vendor implements HR Broadcast differently:

### 4.1 Huawei Watch GT Series (GT 3 Pro, GT 4, GT 5, Watch Fit / Band)

- **Activation & UI flow:** Enabled on the watch under **Settings → HR Data Broadcasts** (or *Heart rate broadcasts*), optionally transitioning directly into a workout ([Huawei Official Support — Heart rate broadcasts on your HUAWEI watch/band](https://consumer.huawei.com/en/support/content/en-us15827504/)). Not all Watch GT models or regional firmwares expose this menu item; Huawei also explicitly notes that **HUAWEI WATCH 5 / WATCH Ultimate 2** support HR Data Broadcast only in **Battery saver mode** ([Huawei Support `en-us15827504`](https://consumer.huawei.com/en/support/content/en-us15827504/)).
- **Single-connection phone drop:** Huawei documents that wearables supporting only a single BLE connection **disconnect from the Huawei Health phone app** while HR broadcast is active ([Huawei Support `en-us15827504`](https://consumer.huawei.com/en/support/content/en-us15827504/)).
- **GATT / Linux pairing quirks:**
  - When HR broadcast is active, supported models expose standard GATT `0x180D` / `0x2A37`.
  - Attempting to connect/subscribe from Linux without completing bonding/pairing on Huawei wearables has been reported to fail with `ATT error: 0x0e (Unlikely Error)` during authentication/pairing mismatches (e.g., Huawei Band 7 on BlueZ 5.66, [Bleak #1241](https://github.com/hbldh/bleak/issues/1241)). Pairing first via BlueZ resolves key-exchange failures.
- **Disconnect behavior:** Broadcast stays active until the user manually exits the HR Data Broadcast screen (or ends the workout and confirms exit via the side/Up button), at which point the watch terminates the BLE link (`org.bluez.Reason.Remote`).

### 4.2 Garmin (Forerunner, Fenix, Epix, Instinct, Venu)

- **Activation & dual protocol:** Modern Garmin watches expose **Broadcast Heart Rate** manually (via Wrist Heart Rate settings or Controls menu shortcut) or automatically via per-activity **Broadcast During Activity** settings ([Garmin Instinct Crossover Manual](https://www8.garmin.com/manuals/webhelp/GUID-64328278-25D1-45C2-BF05-6EC0CFD000D0/EN-US/GUID-D8D363C2-0690-48D4-95E2-A3557E7D53C2.html), [Garmin Edge/Watch Broadcast Manual](https://www8.garmin.com/manuals/webhelp/GUID-29C8FC85-6D84-4BD8-8695-745B20B1ED2D/EN-US/GUID-CC9D127C-E658-4856-93B2-678D82AE5EE3.html)). Modern watches broadcast over both ANT+ and BLE `0x180D` (older Garmin models broadcast ANT+ only).
- **RR-Interval omission on optical wrist broadcast:** Garmin's wrist optical HR broadcast streams **BPM only** via `0x2A37` (bit 4 unset—no raw RR-intervals/HRV). Raw RR-intervals over BLE `0x2A37` are provided by Garmin's dedicated ECG chest straps (e.g., HRM-Dual, HRM-Pro), not wrist optical broadcast.
- **Advertising & disconnect behavior:** Leaving the standalone Broadcast Heart Rate screen or finishing a broadcast-enabled activity immediately stops BLE advertising and drops the GATT link. Garmin manuals explicitly warn that continuous HR broadcasting significantly reduces watch battery life.

### 4.3 Polar (H10/H9 Chest Straps, Verity Sense/OH1 Optical Armbands, Vantage/Ignite/Pacer Watches)

- **Reference-grade `0x180D` / `0x2A37` implementation:** Polar's official [`polar-ble-sdk`](https://github.com/polarofficial/polar-ble-sdk/blob/master/sources/Android/android-communications/library/src/main/java/com/polar/androidcommunications/api/ble/model/gatt/client/BleHrClient.kt) implements HRS v1.0 at 1 Hz notifications, little-endian `uint8`/`uint16` BPM, sensor contact bits, and `1/1024 s` RR-intervals.
- **ECG straps (H10/H9) vs. optical sensors (Verity Sense/OH1/Watches):**
  - **Polar H10:** Provides true ECG BPM and accurate `1/1024 s` RR-intervals in `0x2A37` at 1 Hz ([`PolarH10.md`](https://github.com/polarofficial/polar-ble-sdk/blob/master/documentation/products/PolarH10.md)) and supports **two simultaneous BLE connections** when enabled in Polar Beat/Flow ([Polar Support — Pair H10 with several devices](https://support.polar.com/en/support/can_i_pair_h10_with_several_devices)). Does not require bonding for standard `0x180D` access and uses a stable device address.
  - **Polar Verity Sense / OH1 (optical PPG):** Polar explicitly warns in [`PolarVeritySense.md`](https://github.com/polarofficial/polar-ble-sdk/blob/master/documentation/products/PolarVeritySense.md) and [`PPIData.md`](https://github.com/polarofficial/polar-ble-sdk/blob/master/documentation/PPIData.md) that **skin contact detection is unreliable on older optical sensors**—they may report `Sensor Contact Supported = 1` yet continue emitting non-zero BPM values even when not worn, and optical Pulse-to-Pulse Intervals (PPI) are highly motion-sensitive (accurate only at rest).
  - **Polar Watches (Vantage V2/V3, Ignite, Pacer, Grit X):** Broadcast standard `0x180D` only when put into **HR sensor mode** (*Pre-training → Quick Menu → Share HR with other devices*) ([Polar Support — HR sensor mode](https://support.polar.com/en/hr-sensor-mode); note that Polar's proprietary PMD SDK streaming is separate and restricted to the pre-training wait screen, per [`UsingSDKWithWatches.md`](https://github.com/polarofficial/polar-ble-sdk/blob/master/documentation/UsingSDKWithWatches.md)).

### 4.4 Amazfit / Xiaomi (Zepp OS Smartwatches, Helio Strap, Mi Band / Smart Band)

- **Modern Zepp OS devices ("Heart Rate Push" / "Activity Heart Rate Sharing"):** Modern Zepp OS devices (e.g., Amazfit Balance, T-Rex 3, Active, Helio Strap) support standard BLE `0x180D` HR broadcasting once **"Heart Rate Push"** (or *Activity Heart Rate Sharing*) is enabled in the Zepp app (`Zepp → Device → Health Monitoring`) ([Amazfit Official Support — Heart Rate Push](https://support.amazfit.com/us/amazfit_helio_strap/docs/GyUIdtHLvoMqNUxOkuNcQB4hn4d)).
- **Legacy Amazfit / Mi Band proprietary auth trap:** On older Huami/Amazfit watches and Mi Band models (even when `Discoverable` is toggled on and GATT service `0x180D` + characteristic `0x2A37` + CCCD `0x2902` appear in service discovery), attempting to call `StartNotify()` via BlueZ fails with **`org.bluez.Error.NotPermitted`** ([Bleak #558](https://github.com/hbldh/bleak/issues/558)). Those legacy devices enforce a proprietary application-level AES auth-key handshake (documented by Gadgetbridge's Huami Server Pairing protocol) before unlocking standard GATT characteristics, rather than standard BLE SMP bonding. BPMute should explicitly detect `org.bluez.Error.NotPermitted` on `0x2A37` `StartNotify()` and inform the user that the device requires Zepp "Heart Rate Push" mode or is an unsupported proprietary-auth model.

### 4.5 Cross-Vendor Summary Matrix

| Vendor / Family | Standard `0x180D` Broadcast Mode | RR-Intervals in `0x2A37` | Bonding / Auth Quirks | Operational / Disconnect Quirks |
| :--- | :--- | :--- | :--- | :--- |
| **Huawei Watch GT (3 Pro / 4 / 5)** | `Settings → HR Data Broadcasts` (model/firmware-dependent; Watch 5 requires Battery Saver mode) | Typically BPM-only on optical wrist broadcast | Pair/bond first on BlueZ to avoid `ATT 0x0e` errors ([Bleak #1241](https://github.com/hbldh/bleak/issues/1241)) | Single-connection models drop Huawei Health phone link; manual exit ends link |
| **Garmin Watches** | `Broadcast Heart Rate` / `Broadcast During Activity` (ANT+ & BLE on modern models) | **No** on optical wrist broadcast (BPM-only); **Yes** on ECG chest straps (HRM-Dual/Pro) | Open BLE HR pairing; RPA rotation handled via BlueZ bonding | Stopping broadcast screen or activity ends link; high watch battery draw |
| **Polar H10 / Verity Sense / Watches** | Always-on when worn (H10); Sensor Mode (Verity Sense); `Share HR` pre-training mode (watches) | **Yes** on H10/H9 (`1/1024 s`, ECG); optical models use PPG/PPI (motion-sensitive) | Standard unbonded or bonded BLE; H10 supports 2 concurrent BLE links | Verity Sense/OH1 skin-contact bit is unreliable and may emit non-zero BPM off-wrist |
| **Amazfit / Xiaomi** | Zepp `Heart Rate Push` / `Activity Heart Rate Sharing` (Zepp OS 3+ / Helio Strap) | Typically BPM-only | Legacy models return `org.bluez.Error.NotPermitted` on `0x2A37` without proprietary Huami auth ([Bleak #558](https://github.com/hbldh/bleak/issues/558)) | Often restricted to active workout or explicit Zepp app toggle |

---

## 5. Architectural Recommendations for BPMute

1. **Use `org.bluez.GattProfile1` (`0000180d-0000-1000-8000-00805f9b34fb`) as the primary auto-reconnect engine.** It is stable across every mainstream Linux distro (BlueZ ≥ 5.64), needs zero root/group/polkit/experimental setup, handles RPA rotation seamlessly in the kernel/controller once bonded, and consumes near-zero CPU while waiting for a watch to start broadcasting.
2. **Enforce Bond + Trust during onboarding:** During initial setup, run bounded `StartDiscovery` filtered to `Transport = "le"` and `UUIDs = ["0000180d-..."]`, stop discovery immediately after selection, invoke `Pair()` (with a fallback scoped `Agent1` if no desktop agent exists) when the device uses an RPA (`AddressType == "random"` and private address bits), set `Device1.Trusted = true`, and verify `ServicesResolved` + `StartNotify()` on `0x2A37`.
3. **Resilient notification handling:** Use `StartNotify()` (not `AcquireNotify()`). On every `Device1` `ServicesResolved = true` transition, check `GattCharacteristic1.Notifying` and call `StartNotify()` if `false`. Treat `BPM == 0` or `Sensor Contact == (supported && !detected)` as "no valid reading" (with a staleness timeout when notifications pause), but **never require** `Sensor Contact Supported` or `RR-Interval` bits to be present.
4. **Battery display via `org.bluez.Battery1`:** Read optional wearable battery percentage from `org.bluez.Battery1.Percentage` instead of touching GATT `0x2A19` directly.
5. **Actionable error diagnostics:** Surface explicit user hints for the three most common Linux/wearable failure modes:
   - `org.bluez.Error.NotPermitted` on `0x2A37` → legacy Amazfit/Mi Band proprietary auth or Heart Rate Push not enabled.
   - `org.bluez.Reason.Authentication` disconnect → stale bond keys (prompt user to remove device and re-pair).
   - `org.freedesktop.DBus.Error.AccessDenied` → custom-hardened D-Bus policy or Flatpak/Snap sandbox missing `org.bluez` access.
