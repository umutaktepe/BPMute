use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use bpmute::ipc::IpcClient;
use serde_json::json;
use tempfile::TempDir;
use zbus::interface;
use zbus::object_server::SignalContext;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

const HRS_UUID: &str = "0000180d-0000-1000-8000-00805f9b34fb";
const HRM_CHAR_UUID: &str = "00002a37-0000-1000-8000-00805f9b34fb";
const ADAPTER_PATH: &str = "/org/bluez/hci0";
const DEVICE_MAC: &str = "AA:BB:CC:DD:EE:FF";
const DEVICE_PATH: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";
const HRM_CHAR_PATH: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/service0010/char0011";

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn owned<'a>(v: impl Into<Value<'a>>) -> OwnedValue {
    OwnedValue::try_from(v.into()).expect("convert Value into OwnedValue")
}

fn spawn_private_dbus(temp: &TempDir) -> (ChildGuard, String) {
    let bus_sock = temp.path().join("system-bus.sock");
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
    (ChildGuard(child), addr)
}

#[derive(Clone)]
struct MockBluezState {
    connected: Arc<AtomicBool>,
    expose_gatt: Arc<AtomicBool>,
    notify_started: Arc<AtomicBool>,
    trusted: Arc<AtomicBool>,
    app_registered: Arc<AtomicBool>,
}

struct MockObjectManager {
    state: MockBluezState,
}

#[interface(name = "org.freedesktop.DBus.ObjectManager")]
impl MockObjectManager {
    fn get_managed_objects(
        &self,
    ) -> HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>> {
        let mut objects = HashMap::new();

        let mut adapter_ifaces = HashMap::new();
        let mut adapter_props = HashMap::new();
        adapter_props.insert("Powered".to_string(), owned(true));
        adapter_ifaces.insert("org.bluez.Adapter1".to_string(), adapter_props);
        adapter_ifaces.insert("org.bluez.GattManager1".to_string(), HashMap::new());
        objects.insert(
            OwnedObjectPath::try_from(ADAPTER_PATH).unwrap(),
            adapter_ifaces,
        );

        let mut dev_ifaces = HashMap::new();
        let mut dev_props = HashMap::new();
        dev_props.insert("Address".to_string(), owned(DEVICE_MAC));
        dev_props.insert("AddressType".to_string(), owned("random"));
        dev_props.insert("Name".to_string(), owned("HUAWEI WATCH HR-C05"));
        dev_props.insert("Paired".to_string(), owned(false));
        dev_props.insert(
            "Trusted".to_string(),
            owned(self.state.trusted.load(Ordering::SeqCst)),
        );
        dev_props.insert(
            "Connected".to_string(),
            owned(self.state.connected.load(Ordering::SeqCst)),
        );
        dev_props.insert("RSSI".to_string(), owned(-55i16));
        dev_props.insert("UUIDs".to_string(), owned(vec![HRS_UUID]));
        dev_ifaces.insert("org.bluez.Device1".to_string(), dev_props);
        objects.insert(OwnedObjectPath::try_from(DEVICE_PATH).unwrap(), dev_ifaces);

        if self.state.expose_gatt.load(Ordering::SeqCst) {
            let mut char_ifaces = HashMap::new();
            let mut char_props = HashMap::new();
            char_props.insert("UUID".to_string(), owned(HRM_CHAR_UUID));
            char_props.insert(
                "Notifying".to_string(),
                owned(self.state.notify_started.load(Ordering::SeqCst)),
            );
            char_props.insert("Value".to_string(), owned(vec![0x06u8, 68u8]));
            char_ifaces.insert("org.bluez.GattCharacteristic1".to_string(), char_props);
            objects.insert(
                OwnedObjectPath::try_from(HRM_CHAR_PATH).unwrap(),
                char_ifaces,
            );
        }

        objects
    }
}

struct MockAdapter;

#[interface(name = "org.bluez.Adapter1")]
impl MockAdapter {
    fn start_discovery(&self) {}
    fn stop_discovery(&self) {}
    fn set_discovery_filter(&self, _filter: HashMap<String, OwnedValue>) {}
    fn remove_device(&self, _device: ObjectPath<'_>) {}
}

struct MockGattManager {
    state: MockBluezState,
}

#[interface(name = "org.bluez.GattManager1")]
impl MockGattManager {
    async fn register_application(
        &self,
        application: ObjectPath<'_>,
        _options: HashMap<String, OwnedValue>,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        let sender = hdr
            .sender()
            .ok_or_else(|| zbus::fdo::Error::Failed("missing caller bus name".into()))?
            .to_owned();
        let obj_mgr = zbus::fdo::ObjectManagerProxy::builder(conn)
            .destination(sender)
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
            .path(application)
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
            .build()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        let managed = obj_mgr
            .get_managed_objects()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("GetManagedObjects failed: {e}")))?;

        let has_hrs_profile = managed.values().any(|ifaces| {
            ifaces
                .get("org.bluez.GattProfile1")
                .and_then(|props| props.get("UUIDs"))
                .and_then(|v| Vec::<String>::try_from(v.try_clone().ok()?).ok())
                .is_some_and(|uuids| uuids.iter().any(|u| u.eq_ignore_ascii_case(HRS_UUID)))
        });
        if !has_hrs_profile {
            return Err(zbus::fdo::Error::InvalidArgs(
                "missing org.bluez.GattProfile1 for 0x180D".into(),
            ));
        }

        self.state.app_registered.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn unregister_application(&self, _application: ObjectPath<'_>) {
        self.state.app_registered.store(false, Ordering::SeqCst);
    }
}

struct MockDevice {
    state: MockBluezState,
}

#[interface(name = "org.bluez.Device1")]
impl MockDevice {
    fn connect(&self) {
        self.state.connected.store(true, Ordering::SeqCst);
    }
    fn disconnect(&self) {
        self.state.connected.store(false, Ordering::SeqCst);
    }

    #[zbus(property)]
    fn address(&self) -> String {
        DEVICE_MAC.to_string()
    }

    #[zbus(property)]
    fn connected(&self) -> bool {
        self.state.connected.load(Ordering::SeqCst)
    }

    #[zbus(property)]
    fn paired(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn trusted(&self) -> bool {
        self.state.trusted.load(Ordering::SeqCst)
    }

    #[zbus(property)]
    fn set_trusted(&self, value: bool) {
        self.state.trusted.store(value, Ordering::SeqCst);
    }
}

struct MockHrmCharacteristic {
    state: MockBluezState,
}

#[interface(name = "org.bluez.GattCharacteristic1")]
impl MockHrmCharacteristic {
    async fn start_notify(&self, #[zbus(signal_context)] ctxt: SignalContext<'_>) {
        self.state.notify_started.store(true, Ordering::SeqCst);
        let _ = self.value_changed(&ctxt).await;
    }

    fn stop_notify(&self) {
        self.state.notify_started.store(false, Ordering::SeqCst);
    }

    #[zbus(property)]
    fn uuid(&self) -> String {
        HRM_CHAR_UUID.to_string()
    }

    #[zbus(property)]
    fn notifying(&self) -> bool {
        self.state.notify_started.load(Ordering::SeqCst)
    }

    #[zbus(property)]
    fn value(&self) -> Vec<u8> {
        vec![0x06, 68]
    }
}

async fn start_mock_bluez(
    bus_addr: &str,
    initial_connected: bool,
    initial_gatt: bool,
) -> (zbus::Connection, MockBluezState) {
    let state = MockBluezState {
        connected: Arc::new(AtomicBool::new(initial_connected)),
        expose_gatt: Arc::new(AtomicBool::new(initial_gatt)),
        notify_started: Arc::new(AtomicBool::new(false)),
        trusted: Arc::new(AtomicBool::new(false)),
        app_registered: Arc::new(AtomicBool::new(false)),
    };
    let conn = zbus::connection::Builder::address(bus_addr)
        .expect("valid private dbus addr")
        .name("org.bluez")
        .expect("mock org.bluez bus name")
        .serve_at(
            "/",
            MockObjectManager {
                state: state.clone(),
            },
        )
        .unwrap()
        .serve_at(ADAPTER_PATH, MockAdapter)
        .unwrap()
        .serve_at(
            ADAPTER_PATH,
            MockGattManager {
                state: state.clone(),
            },
        )
        .unwrap()
        .serve_at(
            DEVICE_PATH,
            MockDevice {
                state: state.clone(),
            },
        )
        .unwrap()
        .serve_at(
            HRM_CHAR_PATH,
            MockHrmCharacteristic {
                state: state.clone(),
            },
        )
        .unwrap()
        .build()
        .await
        .expect("mock bluez connection");

    (conn, state)
}

async fn spawn_daemon_and_connect(
    temp: &TempDir,
    bus_addr: &str,
) -> (ChildGuard, IpcClient, std::path::PathBuf) {
    let cfg_path = temp.path().join("cfg/bpmute/config.toml");
    let data_dir = temp.path().join("data/bpmute");
    let socket_path = temp.path().join("run/bpmute/daemon.sock");

    let mut daemon_child = Command::new(env!("CARGO_BIN_EXE_bpmute"))
        .args([
            "--config",
            cfg_path.to_str().unwrap(),
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--socket",
            socket_path.to_str().unwrap(),
            "daemon",
        ])
        .env("DBUS_SYSTEM_BUS_ADDRESS", bus_addr)
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/dev/null")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("bpmute daemon should spawn");

    let start_wait = std::time::Instant::now();
    let client = loop {
        if let Some(status) = daemon_child.try_wait().expect("poll daemon child") {
            panic!("bpmute daemon exited early with {status}");
        }
        match IpcClient::connect(&socket_path).await {
            Ok(c) => break c,
            Err(_) if start_wait.elapsed() < Duration::from_secs(3) => {
                tokio::task::yield_now().await;
            }
            Err(err) => {
                let _ = daemon_child.kill();
                let _ = daemon_child.wait();
                panic!("timed out connecting to daemon socket ({err})");
            }
        }
    };

    (ChildGuard(daemon_child), client, socket_path)
}

async fn wait_for_hr_sample(client: &mut IpcClient, expected_bpm: f64) -> bool {
    tokio::time::timeout(Duration::from_secs(3), async {
        while let Ok(ev) = client.next_event().await {
            if ev.event == "HrSample" && ev.data["bpm"].as_f64() == Some(expected_bpm) {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false)
}

async fn wait_for_wearable_status(client: &mut IpcClient, expected_status: &str) -> bool {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(res) = client.request("wearable.list", json!({})).await
                && let Some(arr) = res["wearables"].as_array()
                && arr
                    .iter()
                    .any(|w| w["status"].as_str() == Some(expected_status))
            {
                return true;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or(false)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bluez_daemon_subscribes_and_streams_hr_packets_after_live_enrollment() {
    let temp = TempDir::new().unwrap();
    let (_dbus_guard, bus_addr) = spawn_private_dbus(&temp);
    let (bluez_conn, bluez_state) = start_mock_bluez(&bus_addr, true, true).await;

    let (_daemon_guard, mut client, socket_path) = spawn_daemon_and_connect(&temp, &bus_addr).await;

    client
        .request("daemon.subscribe", json!({}))
        .await
        .expect("subscribe succeeds");

    let enroll_out = tokio::process::Command::new(env!("CARGO_BIN_EXE_bpmute"))
        .args([
            "--socket",
            socket_path.to_str().unwrap(),
            "enroll",
            "--address",
            DEVICE_MAC,
            "--alias",
            "Watch",
        ])
        .env("DBUS_SYSTEM_BUS_ADDRESS", &bus_addr)
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/dev/null")
        .output()
        .await
        .expect("bpmute enroll runs");
    assert!(
        enroll_out.status.success(),
        "enroll failed: {}",
        String::from_utf8_lossy(&enroll_out.stderr)
    );

    assert!(
        wait_for_wearable_status(&mut client, "acquiring").await
            || wait_for_wearable_status(&mut client, "streaming").await,
        "BluezWearableSource must transition enrolled wearable to acquiring/streaming"
    );
    assert!(
        bluez_state.app_registered.load(Ordering::SeqCst),
        "BluezWearableSource must register GattProfile1 via GattManager1.RegisterApplication + ObjectManager"
    );

    let mut changed: HashMap<&str, Value<'_>> = HashMap::new();
    changed.insert("Value", Value::from(vec![0x06u8, 68u8]));
    let invalidated: Vec<&str> = Vec::new();
    bluez_conn
        .emit_signal(
            Option::<zbus::names::BusName>::None,
            HRM_CHAR_PATH,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &("org.bluez.GattCharacteristic1", changed, invalidated),
        )
        .await
        .expect("emit PropertiesChanged");

    assert!(
        wait_for_hr_sample(&mut client, 68.0).await,
        "bpmute daemon must decode BlueZ 0x2A37 notifications into live HrSample IPC events"
    );
    assert!(
        bluez_state.notify_started.load(Ordering::SeqCst),
        "BluezWearableSource must call StartNotify() on 0x2A37 characteristic"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bluez_daemon_handles_connect_services_resolved_disconnect_and_reconnect() {
    let temp = TempDir::new().unwrap();
    let (_dbus_guard, bus_addr) = spawn_private_dbus(&temp);
    // Start disconnected with GATT not yet resolved
    let (bluez_conn, bluez_state) = start_mock_bluez(&bus_addr, false, false).await;

    let (_daemon_guard, mut client, socket_path) = spawn_daemon_and_connect(&temp, &bus_addr).await;

    client
        .request("daemon.subscribe", json!({}))
        .await
        .expect("subscribe succeeds");

    // Enroll while disconnected
    let enroll_out = tokio::process::Command::new(env!("CARGO_BIN_EXE_bpmute"))
        .args([
            "--socket",
            socket_path.to_str().unwrap(),
            "enroll",
            "--address",
            DEVICE_MAC,
            "--alias",
            "Watch",
        ])
        .env("DBUS_SYSTEM_BUS_ADDRESS", &bus_addr)
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/dev/null")
        .output()
        .await
        .expect("bpmute enroll runs");
    assert!(enroll_out.status.success());

    // 1. Device connects (GATT not yet resolved)
    bluez_state.connected.store(true, Ordering::SeqCst);
    let mut dev_changed: HashMap<&str, Value<'_>> = HashMap::new();
    dev_changed.insert("Connected", Value::from(true));
    let empty_inv: Vec<&str> = Vec::new();
    bluez_conn
        .emit_signal(
            Option::<zbus::names::BusName>::None,
            DEVICE_PATH,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &("org.bluez.Device1", dev_changed, &empty_inv),
        )
        .await
        .unwrap();
    assert!(wait_for_wearable_status(&mut client, "connecting").await);

    // 2. BlueZ resolves GATT 0x2A37 characteristic and emits InterfacesAdded
    bluez_state.expose_gatt.store(true, Ordering::SeqCst);
    let mut char_props: HashMap<&str, Value<'_>> = HashMap::new();
    char_props.insert("UUID", Value::from(HRM_CHAR_UUID));
    let mut ifaces: HashMap<&str, HashMap<&str, Value<'_>>> = HashMap::new();
    ifaces.insert("org.bluez.GattCharacteristic1", char_props);
    bluez_conn
        .emit_signal(
            Option::<zbus::names::BusName>::None,
            "/",
            "org.freedesktop.DBus.ObjectManager",
            "InterfacesAdded",
            &(ObjectPath::try_from(HRM_CHAR_PATH).unwrap(), ifaces),
        )
        .await
        .unwrap();
    assert!(wait_for_wearable_status(&mut client, "acquiring").await);
    assert!(bluez_state.notify_started.load(Ordering::SeqCst));

    // 3. Heart rate notification arrives -> Streaming + HrSample
    let mut hr_changed: HashMap<&str, Value<'_>> = HashMap::new();
    hr_changed.insert("Value", Value::from(vec![0x06u8, 72u8]));
    bluez_conn
        .emit_signal(
            Option::<zbus::names::BusName>::None,
            HRM_CHAR_PATH,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &("org.bluez.GattCharacteristic1", hr_changed, &empty_inv),
        )
        .await
        .unwrap();
    assert!(wait_for_hr_sample(&mut client, 72.0).await);

    // 4. Device disconnects -> status returns to disconnected
    bluez_state.connected.store(false, Ordering::SeqCst);
    bluez_state.notify_started.store(false, Ordering::SeqCst);
    let mut disc_changed: HashMap<&str, Value<'_>> = HashMap::new();
    disc_changed.insert("Connected", Value::from(false));
    bluez_conn
        .emit_signal(
            Option::<zbus::names::BusName>::None,
            DEVICE_PATH,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &("org.bluez.Device1", disc_changed, &empty_inv),
        )
        .await
        .unwrap();
    assert!(wait_for_wearable_status(&mut client, "disconnected").await);

    // 5. Device reconnects passively -> StartNotify re-arms -> new HR packet streams
    bluez_state.connected.store(true, Ordering::SeqCst);
    let mut rec_changed: HashMap<&str, Value<'_>> = HashMap::new();
    rec_changed.insert("Connected", Value::from(true));
    bluez_conn
        .emit_signal(
            Option::<zbus::names::BusName>::None,
            DEVICE_PATH,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &("org.bluez.Device1", rec_changed, &empty_inv),
        )
        .await
        .unwrap();
    assert!(wait_for_wearable_status(&mut client, "acquiring").await);

    let mut hr_changed2: HashMap<&str, Value<'_>> = HashMap::new();
    hr_changed2.insert("Value", Value::from(vec![0x06u8, 64u8]));
    bluez_conn
        .emit_signal(
            Option::<zbus::names::BusName>::None,
            HRM_CHAR_PATH,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &("org.bluez.GattCharacteristic1", hr_changed2, &empty_inv),
        )
        .await
        .unwrap();
    assert!(wait_for_hr_sample(&mut client, 64.0).await);
}
