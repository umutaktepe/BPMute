use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU8, Ordering},
};
use std::time::Duration;

use bpmute::ipc::IpcClient;
use chrono::Utc;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::mpsc;
use zbus::interface;
use zbus::object_server::SignalContext;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

const HRS_UUID: &str = "0000180d-0000-1000-8000-00805f9b34fb";
const HRM_CHAR_UUID: &str = "00002a37-0000-1000-8000-00805f9b34fb";
const ADAPTER_PATH: &str = "/org/bluez/hci0";
const DEVICE_MAC: &str = "AA:BB:CC:DD:EE:FF";
const DEVICE_PATH: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";
const HRM_CHAR_PATH: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/service0010/char0011";
const DEVICE2_MAC: &str = "11:22:33:44:55:66";
const DEVICE2_PATH: &str = "/org/bluez/hci0/dev_11_22_33_44_55_66";
const HRM_CHAR2_PATH: &str = "/org/bluez/hci0/dev_11_22_33_44_55_66/service0010/char0011";
const BATTERY_LEVEL_CHAR_UUID: &str = "00002a19-0000-1000-8000-00805f9b34fb";
const BATTERY_CHAR_PATH: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/service0012/char0013";

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum MockBluezError {
    #[zbus(error)]
    ZBus(zbus::Error),
    AlreadyConnected(String),
    Failed(String),
}

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

fn spawn_private_dbus_named(temp: &TempDir, sock_name: &str) -> (ChildGuard, String) {
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
    (ChildGuard(child), addr)
}

fn spawn_private_dbus(temp: &TempDir) -> (ChildGuard, String) {
    spawn_private_dbus_named(temp, "system-bus.sock")
}

#[derive(Clone)]
struct MockBluezState {
    adapter_path: String,
    device_path: String,
    hrm_char_path: String,
    adapter_powered: Arc<AtomicBool>,
    expose_device: Arc<AtomicBool>,
    expose_hrs_uuid: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    expose_gatt: Arc<AtomicBool>,
    notify_started: Arc<AtomicBool>,
    trusted: Arc<AtomicBool>,
    app_registered: Arc<AtomicBool>,
    discovering: Arc<AtomicBool>,
    fail_start_discovery: Arc<AtomicBool>,
    fail_connect: Arc<AtomicBool>,
    expose_second_wearable: Arc<AtomicBool>,
    second_connected: Arc<AtomicBool>,
    second_notify_started: Arc<AtomicBool>,
    battery_char_path: String,
    expose_battery_char: Arc<AtomicBool>,
    battery_level: Arc<AtomicU8>,
    battery_notify_started: Arc<AtomicBool>,
}

struct MockBluezOptions<'a> {
    adapter_path: &'a str,
    adapter_powered: bool,
    expose_wearable: bool,
    expose_hrs_uuid: bool,
    initial_connected: bool,
    initial_gatt: bool,
    fail_start_discovery: bool,
    fail_connect: bool,
    expose_second_wearable: bool,
    expose_battery_char: bool,
    battery_level: u8,
}

impl Default for MockBluezOptions<'_> {
    fn default() -> Self {
        Self {
            adapter_path: ADAPTER_PATH,
            adapter_powered: true,
            expose_wearable: false,
            expose_hrs_uuid: true,
            initial_connected: false,
            initial_gatt: false,
            fail_start_discovery: false,
            fail_connect: false,
            expose_second_wearable: false,
            expose_battery_char: false,
            battery_level: 85,
        }
    }
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
        adapter_props.insert(
            "Powered".to_string(),
            owned(self.state.adapter_powered.load(Ordering::SeqCst)),
        );
        adapter_ifaces.insert("org.bluez.Adapter1".to_string(), adapter_props);
        adapter_ifaces.insert("org.bluez.GattManager1".to_string(), HashMap::new());
        objects.insert(
            OwnedObjectPath::try_from(self.state.adapter_path.as_str()).unwrap(),
            adapter_ifaces,
        );

        if self.state.expose_device.load(Ordering::SeqCst) {
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
            let uuids = if self.state.expose_hrs_uuid.load(Ordering::SeqCst) {
                vec![HRS_UUID]
            } else {
                Vec::new()
            };
            dev_props.insert("UUIDs".to_string(), owned(uuids));
            dev_ifaces.insert("org.bluez.Device1".to_string(), dev_props);
            objects.insert(
                OwnedObjectPath::try_from(self.state.device_path.as_str()).unwrap(),
                dev_ifaces,
            );
        }

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
                OwnedObjectPath::try_from(self.state.hrm_char_path.as_str()).unwrap(),
                char_ifaces,
            );
        }

        if self.state.expose_battery_char.load(Ordering::SeqCst) {
            let mut bat_ifaces = HashMap::new();
            let mut bat_props = HashMap::new();
            bat_props.insert("UUID".to_string(), owned(BATTERY_LEVEL_CHAR_UUID));
            bat_props.insert(
                "Notifying".to_string(),
                owned(self.state.battery_notify_started.load(Ordering::SeqCst)),
            );
            bat_props.insert(
                "Value".to_string(),
                owned(vec![self.state.battery_level.load(Ordering::SeqCst)]),
            );
            bat_ifaces.insert("org.bluez.GattCharacteristic1".to_string(), bat_props);
            objects.insert(
                OwnedObjectPath::try_from(self.state.battery_char_path.as_str()).unwrap(),
                bat_ifaces,
            );
        }

        if self.state.expose_second_wearable.load(Ordering::SeqCst) {
            let mut dev2_ifaces = HashMap::new();
            let mut dev2_props = HashMap::new();
            dev2_props.insert("Address".to_string(), owned(DEVICE2_MAC));
            dev2_props.insert("AddressType".to_string(), owned("public"));
            dev2_props.insert("Name".to_string(), owned("Polar H10 Secondary"));
            dev2_props.insert("Paired".to_string(), owned(false));
            dev2_props.insert("Trusted".to_string(), owned(false));
            dev2_props.insert(
                "Connected".to_string(),
                owned(self.state.second_connected.load(Ordering::SeqCst)),
            );
            dev2_props.insert("RSSI".to_string(), owned(-60i16));
            dev2_props.insert("UUIDs".to_string(), owned(vec![HRS_UUID]));
            dev2_ifaces.insert("org.bluez.Device1".to_string(), dev2_props);
            objects.insert(
                OwnedObjectPath::try_from(DEVICE2_PATH).unwrap(),
                dev2_ifaces,
            );

            let mut char2_ifaces = HashMap::new();
            let mut char2_props = HashMap::new();
            char2_props.insert("UUID".to_string(), owned(HRM_CHAR_UUID));
            char2_props.insert(
                "Notifying".to_string(),
                owned(self.state.second_notify_started.load(Ordering::SeqCst)),
            );
            char2_props.insert("Value".to_string(), owned(vec![0x06u8, 65u8]));
            char2_ifaces.insert("org.bluez.GattCharacteristic1".to_string(), char2_props);
            objects.insert(
                OwnedObjectPath::try_from(HRM_CHAR2_PATH).unwrap(),
                char2_ifaces,
            );
        }

        objects
    }
}

struct MockAdapter {
    state: MockBluezState,
}

#[interface(name = "org.bluez.Adapter1")]
impl MockAdapter {
    fn start_discovery(&self) -> zbus::fdo::Result<()> {
        if self.state.fail_start_discovery.load(Ordering::SeqCst) {
            return Err(zbus::fdo::Error::Failed("Resource Not Ready".into()));
        }
        self.state.discovering.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn stop_discovery(&self) {
        self.state.discovering.store(false, Ordering::SeqCst);
    }
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
    async fn connect(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> Result<(), MockBluezError> {
        if self.state.fail_connect.load(Ordering::SeqCst) {
            return Err(MockBluezError::Failed("Host is down".into()));
        }
        if self.state.connected.load(Ordering::SeqCst) {
            return Err(MockBluezError::AlreadyConnected("Already Connected".into()));
        }
        self.state.connected.store(true, Ordering::SeqCst);
        let mut changed: HashMap<&str, Value<'_>> = HashMap::new();
        changed.insert("Connected", Value::from(true));
        let empty_inv: Vec<&str> = Vec::new();
        let _ = conn
            .emit_signal(
                Option::<zbus::names::BusName>::None,
                self.state.device_path.as_str(),
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                &("org.bluez.Device1", changed, &empty_inv),
            )
            .await;
        Ok(())
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

struct MockBatteryCharacteristic {
    state: MockBluezState,
}

#[interface(name = "org.bluez.GattCharacteristic1")]
impl MockBatteryCharacteristic {
    async fn read_value(
        &self,
        _options: HashMap<String, Value<'_>>,
    ) -> Result<Vec<u8>, MockBluezError> {
        Ok(vec![self.state.battery_level.load(Ordering::SeqCst)])
    }

    async fn start_notify(&self, #[zbus(signal_context)] ctxt: SignalContext<'_>) {
        self.state
            .battery_notify_started
            .store(true, Ordering::SeqCst);
        let _ = self.value_changed(&ctxt).await;
    }

    fn stop_notify(&self) {
        self.state
            .battery_notify_started
            .store(false, Ordering::SeqCst);
    }

    #[zbus(property)]
    fn uuid(&self) -> String {
        BATTERY_LEVEL_CHAR_UUID.to_string()
    }

    #[zbus(property)]
    fn notifying(&self) -> bool {
        self.state.battery_notify_started.load(Ordering::SeqCst)
    }

    #[zbus(property)]
    fn value(&self) -> Vec<u8> {
        vec![self.state.battery_level.load(Ordering::SeqCst)]
    }
}

struct MockDevice2 {
    state: MockBluezState,
}

#[interface(name = "org.bluez.Device1")]
impl MockDevice2 {
    async fn connect(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> Result<(), MockBluezError> {
        self.state.second_connected.store(true, Ordering::SeqCst);
        let mut changed: HashMap<&str, Value<'_>> = HashMap::new();
        changed.insert("Connected", Value::from(true));
        let empty_inv: Vec<&str> = Vec::new();
        let _ = conn
            .emit_signal(
                Option::<zbus::names::BusName>::None,
                DEVICE2_PATH,
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                &("org.bluez.Device1", changed, &empty_inv),
            )
            .await;
        Ok(())
    }
    fn disconnect(&self) {
        self.state.second_connected.store(false, Ordering::SeqCst);
    }

    #[zbus(property)]
    fn address(&self) -> String {
        DEVICE2_MAC.to_string()
    }

    #[zbus(property)]
    fn connected(&self) -> bool {
        self.state.second_connected.load(Ordering::SeqCst)
    }

    #[zbus(property)]
    fn paired(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn trusted(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn set_trusted(&self, _value: bool) {}
}

struct MockHrmCharacteristic2 {
    state: MockBluezState,
}

#[interface(name = "org.bluez.GattCharacteristic1")]
impl MockHrmCharacteristic2 {
    async fn start_notify(&self) {
        self.state
            .second_notify_started
            .store(true, Ordering::SeqCst);
    }

    fn stop_notify(&self) {
        self.state
            .second_notify_started
            .store(false, Ordering::SeqCst);
    }

    #[zbus(property)]
    fn uuid(&self) -> String {
        HRM_CHAR_UUID.to_string()
    }

    #[zbus(property)]
    fn notifying(&self) -> bool {
        self.state.second_notify_started.load(Ordering::SeqCst)
    }

    #[zbus(property)]
    fn value(&self) -> Vec<u8> {
        vec![0x06, 65]
    }
}

async fn start_mock_bluez(
    bus_addr: &str,
    expose_wearable: bool,
    initial_connected: bool,
    initial_gatt: bool,
) -> (zbus::Connection, MockBluezState) {
    start_mock_bluez_with_options(
        bus_addr,
        MockBluezOptions {
            expose_wearable,
            initial_connected,
            initial_gatt,
            ..Default::default()
        },
    )
    .await
}

async fn start_mock_bluez_with_options(
    bus_addr: &str,
    opts: MockBluezOptions<'_>,
) -> (zbus::Connection, MockBluezState) {
    let device_path = format!("{}/dev_AA_BB_CC_DD_EE_FF", opts.adapter_path);
    let hrm_char_path = format!("{device_path}/service0010/char0011");
    let battery_char_path = format!("{device_path}/service0012/char0013");
    let state = MockBluezState {
        adapter_path: opts.adapter_path.to_string(),
        device_path: device_path.clone(),
        hrm_char_path: hrm_char_path.clone(),
        adapter_powered: Arc::new(AtomicBool::new(opts.adapter_powered)),
        expose_device: Arc::new(AtomicBool::new(opts.expose_wearable)),
        expose_hrs_uuid: Arc::new(AtomicBool::new(opts.expose_hrs_uuid)),
        connected: Arc::new(AtomicBool::new(opts.initial_connected)),
        expose_gatt: Arc::new(AtomicBool::new(opts.initial_gatt)),
        notify_started: Arc::new(AtomicBool::new(false)),
        trusted: Arc::new(AtomicBool::new(false)),
        app_registered: Arc::new(AtomicBool::new(false)),
        discovering: Arc::new(AtomicBool::new(false)),
        fail_start_discovery: Arc::new(AtomicBool::new(opts.fail_start_discovery)),
        fail_connect: Arc::new(AtomicBool::new(opts.fail_connect)),
        expose_second_wearable: Arc::new(AtomicBool::new(opts.expose_second_wearable)),
        second_connected: Arc::new(AtomicBool::new(false)),
        second_notify_started: Arc::new(AtomicBool::new(false)),
        battery_char_path: battery_char_path.clone(),
        expose_battery_char: Arc::new(AtomicBool::new(opts.expose_battery_char)),
        battery_level: Arc::new(AtomicU8::new(opts.battery_level)),
        battery_notify_started: Arc::new(AtomicBool::new(false)),
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
        .serve_at(
            opts.adapter_path,
            MockAdapter {
                state: state.clone(),
            },
        )
        .unwrap()
        .serve_at(
            opts.adapter_path,
            MockGattManager {
                state: state.clone(),
            },
        )
        .unwrap()
        .serve_at(
            device_path.as_str(),
            MockDevice {
                state: state.clone(),
            },
        )
        .unwrap()
        .serve_at(
            hrm_char_path.as_str(),
            MockHrmCharacteristic {
                state: state.clone(),
            },
        )
        .unwrap()
        .serve_at(
            battery_char_path.as_str(),
            MockBatteryCharacteristic {
                state: state.clone(),
            },
        )
        .unwrap()
        .serve_at(
            DEVICE2_PATH,
            MockDevice2 {
                state: state.clone(),
            },
        )
        .unwrap()
        .serve_at(
            HRM_CHAR2_PATH,
            MockHrmCharacteristic2 {
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
    spawn_daemon_with_session_bus(temp, bus_addr, "unix:path=/dev/null").await
}

async fn spawn_daemon_with_session_bus(
    temp: &TempDir,
    bus_addr: &str,
    session_bus_addr: &str,
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
        .env("DBUS_SESSION_BUS_ADDRESS", session_bus_addr)
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

async fn poll_ipc_until<T>(
    client: &mut IpcClient,
    op: &str,
    extract: impl Fn(&serde_json::Value) -> Option<T>,
) -> Option<T> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(res) = client.request(op, json!({})).await
                && let Some(val) = extract(&res)
            {
                return Some(val);
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .ok()
    .flatten()
}

async fn wait_for_wearable_status(client: &mut IpcClient, expected_status: &str) -> bool {
    poll_ipc_until(client, "wearable.list", |res| {
        res["wearables"]
            .as_array()?
            .iter()
            .any(|w| w["status"].as_str() == Some(expected_status))
            .then_some(())
    })
    .await
    .is_some()
}

async fn wait_for_wearable_battery(client: &mut IpcClient, expected_pct: u8) -> bool {
    poll_ipc_until(client, "wearable.list", |res| {
        res["wearables"]
            .as_array()?
            .iter()
            .any(|w| w["battery"].as_u64() == Some(u64::from(expected_pct)))
            .then_some(())
    })
    .await
    .is_some()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bluez_daemon_subscribes_and_streams_hr_packets_after_live_enrollment() {
    let temp = TempDir::new().unwrap();
    let (_dbus_guard, bus_addr) = spawn_private_dbus(&temp);
    let (bluez_conn, bluez_state) = start_mock_bluez(&bus_addr, true, true, true).await;

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
    let (bluez_conn, bluez_state) = start_mock_bluez(&bus_addr, true, false, false).await;

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

async fn connect_tui_app_to_daemon(
    socket_path: std::path::PathBuf,
) -> (
    bpmute::tui::TuiApp,
    tokio::sync::mpsc::Receiver<bpmute::tui::TuiEvent>,
    tokio::sync::mpsc::Sender<bpmute::tui::TuiCommand>,
    tokio::sync::watch::Sender<bool>,
    tokio::task::JoinHandle<()>,
) {
    use bpmute::tui::{TuiApp, TuiIpcBridge};
    use tokio::sync::{mpsc, watch};

    let (event_tx, mut event_rx) = mpsc::channel(256);
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let bridge = TuiIpcBridge::new(socket_path, event_tx, cmd_rx, shutdown_rx);
    let bridge_task = tokio::spawn(bridge.run());

    let mut app = TuiApp::new(false, false, false);
    tokio::time::timeout(Duration::from_secs(3), async {
        while !app.daemon_connected {
            if let Some(ev) = event_rx.recv().await {
                app.handle_event(ev);
            }
        }
    })
    .await
    .expect("TUI connects to daemon");

    (app, event_rx, cmd_tx, shutdown_tx, bridge_task)
}

async fn wait_for_discovering_flag(
    bluez_state: &MockBluezState,
    expected: bool,
    app: &mut bpmute::tui::TuiApp,
    event_rx: &mut tokio::sync::mpsc::Receiver<bpmute::tui::TuiEvent>,
) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while bluez_state.discovering.load(Ordering::SeqCst) != expected {
            while let Ok(ev) = event_rx.try_recv() {
                app.handle_event(ev);
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for discovering={expected}"));
}

async fn open_wearables_and_start_scan(
    app: &mut bpmute::tui::TuiApp,
    cmd_tx: &tokio::sync::mpsc::Sender<bpmute::tui::TuiCommand>,
) {
    use bpmute::tui::TuiEvent;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    app.handle_event(TuiEvent::Key(KeyEvent::new(
        KeyCode::Char('3'),
        KeyModifiers::NONE,
    )));
    let start_cmd = app
        .handle_event(TuiEvent::Key(KeyEvent::new(
            KeyCode::Char('e'),
            KeyModifiers::NONE,
        )))
        .expect("pressing 'e' emits EnrollStart");
    cmd_tx.send(start_cmd).await.unwrap();
}

async fn emit_mock_wearable_interfaces_added(
    bluez_conn: &zbus::Connection,
    bluez_state: &MockBluezState,
    wearable_path: &str,
    include_hrs_uuid: bool,
) {
    bluez_state.expose_device.store(true, Ordering::SeqCst);
    bluez_state
        .expose_hrs_uuid
        .store(include_hrs_uuid, Ordering::SeqCst);
    let mut props: HashMap<&str, Value<'_>> = HashMap::new();
    props.insert("Address", Value::from(DEVICE_MAC));
    props.insert("AddressType", Value::from("random"));
    props.insert("Name", Value::from("HUAWEI WATCH HR-C05"));
    props.insert("Paired", Value::from(false));
    props.insert("Trusted", Value::from(false));
    props.insert("Connected", Value::from(false));
    props.insert("RSSI", Value::from(-55i16));
    let uuids = if include_hrs_uuid {
        vec![HRS_UUID]
    } else {
        Vec::new()
    };
    props.insert("UUIDs", Value::from(uuids));
    let mut ifaces: HashMap<&str, HashMap<&str, Value<'_>>> = HashMap::new();
    ifaces.insert("org.bluez.Device1", props);
    bluez_conn
        .emit_signal(
            Option::<zbus::names::BusName>::None,
            "/",
            "org.freedesktop.DBus.ObjectManager",
            "InterfacesAdded",
            &(ObjectPath::try_from(wearable_path).unwrap(), ifaces),
        )
        .await
        .unwrap();
}

async fn wait_for_tui_app<F>(
    app: &mut bpmute::tui::TuiApp,
    event_rx: &mut tokio::sync::mpsc::Receiver<bpmute::tui::TuiEvent>,
    cmd_tx: &tokio::sync::mpsc::Sender<bpmute::tui::TuiCommand>,
    pred: F,
) -> bool
where
    F: Fn(&bpmute::tui::TuiApp) -> bool,
{
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if pred(app) {
                return true;
            }
            match event_rx.recv().await {
                Some(ev) => {
                    if let Some(cmd) = app.handle_event(ev) {
                        let _ = cmd_tx.send(cmd).await;
                    }
                }
                None => return false,
            }
        }
    })
    .await
    .unwrap_or(false)
}

async fn wait_for_tui_discovered_wearable(
    app: &mut bpmute::tui::TuiApp,
    event_rx: &mut tokio::sync::mpsc::Receiver<bpmute::tui::TuiEvent>,
    cmd_tx: &tokio::sync::mpsc::Sender<bpmute::tui::TuiCommand>,
    mac: &str,
) -> bool {
    wait_for_tui_app(app, event_rx, cmd_tx, |a| {
        a.discovered_wearables
            .iter()
            .any(|d| d.address.eq_ignore_ascii_case(mac))
    })
    .await
}

async fn wait_for_tui_enrollment_error(
    app: &mut bpmute::tui::TuiApp,
    event_rx: &mut tokio::sync::mpsc::Receiver<bpmute::tui::TuiEvent>,
) -> String {
    use bpmute::tui::ModalState;

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let ModalState::EnrollmentModal {
                scanning_remaining_ms: 0,
                error_message: Some(ref msg),
                ..
            } = app.modal
            {
                return msg.clone();
            }
            if let Some(ev) = event_rx.recv().await {
                app.handle_event(ev);
            }
        }
    })
    .await
    .expect("TUI must surface BlueZ StartDiscovery failure inside EnrollmentModal")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tui_wearable_scan_discovers_wearable_appearing_after_start_discovery() {
    let temp = TempDir::new().unwrap();
    let (_dbus_guard, bus_addr) = spawn_private_dbus(&temp);
    // Start with NO cached wearables in BlueZ; wearable only advertises after StartDiscovery()
    let (bluez_conn, bluez_state) = start_mock_bluez(&bus_addr, false, false, false).await;

    let (_daemon_guard, _client, socket_path) = spawn_daemon_and_connect(&temp, &bus_addr).await;
    let (mut app, mut event_rx, cmd_tx, shutdown_tx, bridge_task) =
        connect_tui_app_to_daemon(socket_path).await;

    open_wearables_and_start_scan(&mut app, &cmd_tx).await;
    wait_for_discovering_flag(&bluez_state, true, &mut app, &mut event_rx).await;

    emit_mock_wearable_interfaces_added(&bluez_conn, &bluez_state, DEVICE_PATH, true).await;

    let found =
        wait_for_tui_discovered_wearable(&mut app, &mut event_rx, &cmd_tx, DEVICE_MAC).await;

    let _ = shutdown_tx.send(true);
    let _ = bridge_task.await;

    assert!(
        found,
        "TUI wearable scan must discover wearable appearing after StartDiscovery; discovered={:?}",
        app.discovered_wearables
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tui_wearable_scan_handles_non_hci0_adapter_late_uuids_rescan_and_cancel() {
    use bpmute::tui::{ModalState, TuiEvent};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let temp = TempDir::new().unwrap();
    let (_dbus_guard, bus_addr) = spawn_private_dbus(&temp);
    let hci1_path = "/org/bluez/hci1";
    let hci1_wearable_path = "/org/bluez/hci1/dev_AA_BB_CC_DD_EE_FF";
    let (bluez_conn, bluez_state) = start_mock_bluez_with_options(
        &bus_addr,
        MockBluezOptions {
            adapter_path: hci1_path,
            expose_hrs_uuid: false,
            ..Default::default()
        },
    )
    .await;

    let (_daemon_guard, _client, socket_path) = spawn_daemon_and_connect(&temp, &bus_addr).await;
    let (mut app, mut event_rx, cmd_tx, shutdown_tx, bridge_task) =
        connect_tui_app_to_daemon(socket_path).await;

    open_wearables_and_start_scan(&mut app, &cmd_tx).await;
    wait_for_discovering_flag(&bluez_state, true, &mut app, &mut event_rx).await;

    // 1. InterfacesAdded arrives initially WITHOUT 0x180D UUID (scan response pending)
    emit_mock_wearable_interfaces_added(&bluez_conn, &bluez_state, hci1_wearable_path, false).await;

    // 2. Scan response arrives later as PropertiesChanged adding HRS_UUID
    tokio::task::yield_now().await;
    bluez_state.expose_hrs_uuid.store(true, Ordering::SeqCst);
    let mut changed: HashMap<&str, Value<'_>> = HashMap::new();
    changed.insert("UUIDs", Value::from(vec![HRS_UUID]));
    let empty_inv: Vec<&str> = Vec::new();
    bluez_conn
        .emit_signal(
            Option::<zbus::names::BusName>::None,
            hci1_wearable_path,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &("org.bluez.Device1", changed, &empty_inv),
        )
        .await
        .unwrap();

    let found =
        wait_for_tui_discovered_wearable(&mut app, &mut event_rx, &cmd_tx, DEVICE_MAC).await;
    assert!(
        found,
        "TUI scan must discover wearable when 0x180D UUID arrives via PropertiesChanged"
    );

    // 3. Pressing 'r' Rescan restarts discovery cleanly without InProgress failure
    let rescan_cmd = app
        .handle_event(TuiEvent::Key(KeyEvent::new(
            KeyCode::Char('r'),
            KeyModifiers::NONE,
        )))
        .unwrap();
    cmd_tx.send(rescan_cmd).await.unwrap();
    wait_for_discovering_flag(&bluez_state, true, &mut app, &mut event_rx).await;
    assert!(matches!(
        app.modal,
        ModalState::EnrollmentModal {
            error_message: None,
            ..
        }
    ));

    // 4. Pressing Esc cancels enrollment and stops BlueZ discovery on /org/bluez/hci1
    let cancel_cmd = app
        .handle_event(TuiEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )))
        .unwrap();
    cmd_tx.send(cancel_cmd).await.unwrap();
    wait_for_discovering_flag(&bluez_state, false, &mut app, &mut event_rx).await;

    let _ = shutdown_tx.send(true);
    let _ = bridge_task.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tui_wearable_scan_surfaces_unpowered_or_refused_adapter_errors() {
    let temp = TempDir::new().unwrap();
    let (_dbus_guard, bus_addr) = spawn_private_dbus(&temp);
    let (_bluez_conn, bluez_state) = start_mock_bluez_with_options(
        &bus_addr,
        MockBluezOptions {
            adapter_powered: false,
            fail_start_discovery: true,
            ..Default::default()
        },
    )
    .await;

    let (_daemon_guard, _client, socket_path) = spawn_daemon_and_connect(&temp, &bus_addr).await;
    let (mut app, mut event_rx, cmd_tx, shutdown_tx, bridge_task) =
        connect_tui_app_to_daemon(socket_path).await;

    // 1. Adapter Powered=false -> explicit unpowered adapter error
    open_wearables_and_start_scan(&mut app, &cmd_tx).await;
    let unpowered_error = wait_for_tui_enrollment_error(&mut app, &mut event_rx).await;
    assert!(
        unpowered_error.contains("no powered Bluetooth adapter"),
        "unexpected error message: {unpowered_error}"
    );

    // 2. Adapter Powered=true but StartDiscovery fails -> Rescan ('r') surfaces D-Bus error
    bluez_state.adapter_powered.store(true, Ordering::SeqCst);
    let rescan_cmd = app
        .handle_event(bpmute::tui::TuiEvent::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('r'),
            crossterm::event::KeyModifiers::NONE,
        )))
        .expect("pressing 'r' inside EnrollmentModal emits EnrollStart rescan");
    cmd_tx.send(rescan_cmd).await.unwrap();
    let surfaced_error = wait_for_tui_enrollment_error(&mut app, &mut event_rx).await;
    assert!(
        surfaced_error.contains("Resource Not Ready"),
        "unexpected error message: {surfaced_error}"
    );

    let _ = shutdown_tx.send(true);
    let _ = bridge_task.await;
}

async fn wait_for_atomic_bool(flag: &AtomicBool, expected: bool, label: &str) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while flag.load(Ordering::SeqCst) != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {label}={expected}"));
}

async fn await_cli_output(
    task: tokio::task::JoinHandle<std::process::Output>,
) -> std::process::Output {
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .expect("CLI enroll completes after wearable discovery")
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_enroll_scan_discovers_wearable_appearing_after_start_discovery() {
    let temp = TempDir::new().unwrap();
    let (_dbus_guard, bus_addr) = spawn_private_dbus(&temp);
    let (bluez_conn, bluez_state) = start_mock_bluez(&bus_addr, false, false, false).await;

    let (_daemon_guard, _client, socket_path) = spawn_daemon_and_connect(&temp, &bus_addr).await;

    let bus_addr_cli = bus_addr.clone();
    let socket_cli = socket_path.clone();
    let cli_task = tokio::spawn(async move {
        tokio::process::Command::new(env!("CARGO_BIN_EXE_bpmute"))
            .args(["--socket", socket_cli.to_str().unwrap(), "enroll"])
            .env("DBUS_SYSTEM_BUS_ADDRESS", &bus_addr_cli)
            .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/dev/null")
            .output()
            .await
            .expect("bpmute enroll runs")
    });

    wait_for_atomic_bool(&bluez_state.discovering, true, "discovering").await;
    emit_mock_wearable_interfaces_added(&bluez_conn, &bluez_state, DEVICE_PATH, true).await;

    let out = await_cli_output(cli_task).await;
    assert!(
        out.status.success(),
        "CLI enroll failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(DEVICE_MAC),
        "CLI enroll stdout should list discovered wearable; stdout={stdout}"
    );
}

async fn wait_for_tui_enrolled_status(
    app: &mut bpmute::tui::TuiApp,
    event_rx: &mut tokio::sync::mpsc::Receiver<bpmute::tui::TuiEvent>,
    cmd_tx: &tokio::sync::mpsc::Sender<bpmute::tui::TuiCommand>,
    mac: &str,
    expected_status: bpmute::sleep_engine::StreamStatus,
) -> bool {
    use bpmute::tui::ModalState;

    wait_for_tui_app(app, event_rx, cmd_tx, |a| {
        a.modal == ModalState::None
            && a.enrolled_wearables
                .iter()
                .any(|w| w.wearable.mac == mac && w.status == expected_status)
    })
    .await
}

async fn scan_and_select_wearable_via_enter(
    app: &mut bpmute::tui::TuiApp,
    event_rx: &mut tokio::sync::mpsc::Receiver<bpmute::tui::TuiEvent>,
    cmd_tx: &tokio::sync::mpsc::Sender<bpmute::tui::TuiCommand>,
    bluez_conn: &zbus::Connection,
    bluez_state: &MockBluezState,
) {
    use bpmute::tui::TuiEvent;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    open_wearables_and_start_scan(app, cmd_tx).await;
    wait_for_discovering_flag(bluez_state, true, app, event_rx).await;

    emit_mock_wearable_interfaces_added(bluez_conn, bluez_state, DEVICE_PATH, true).await;
    assert!(
        wait_for_tui_discovered_wearable(app, event_rx, cmd_tx, DEVICE_MAC).await,
        "wearable must be discovered before selection"
    );

    let select_cmd = app
        .handle_event(TuiEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("pressing Enter in EnrollmentModal emits EnrollSelect");
    cmd_tx.send(select_cmd).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tui_enroll_select_auto_connects_reachable_wearable_to_acquiring() {
    use bpmute::sleep_engine::StreamStatus;

    let temp = TempDir::new().unwrap();
    let (_dbus_guard, bus_addr) = spawn_private_dbus(&temp);
    // Reachable wearable with 0x2A37 GATT ready once connected, initially disconnected
    let (bluez_conn, bluez_state) = start_mock_bluez(&bus_addr, false, false, true).await;

    let (_daemon_guard, _client, socket_path) = spawn_daemon_and_connect(&temp, &bus_addr).await;
    let (mut app, mut event_rx, cmd_tx, shutdown_tx, bridge_task) =
        connect_tui_app_to_daemon(socket_path).await;

    scan_and_select_wearable_via_enter(&mut app, &mut event_rx, &cmd_tx, &bluez_conn, &bluez_state)
        .await;

    let reached_acquiring = wait_for_tui_enrolled_status(
        &mut app,
        &mut event_rx,
        &cmd_tx,
        DEVICE_MAC,
        StreamStatus::Acquiring,
    )
    .await;

    let _ = shutdown_tx.send(true);
    let _ = bridge_task.await;

    assert!(
        reached_acquiring,
        "newly enrolled reachable wearable must auto-connect to Acquiring without manual reconnect; enrolled={:?}",
        app.enrolled_wearables
    );
    assert!(bluez_state.connected.load(Ordering::SeqCst));
    assert!(bluez_state.notify_started.load(Ordering::SeqCst));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tui_enroll_select_succeeds_as_disconnected_and_surfaces_warning_when_connect_fails() {
    use bpmute::sleep_engine::StreamStatus;

    let temp = TempDir::new().unwrap();
    let (_dbus_guard, bus_addr) = spawn_private_dbus(&temp);
    let (bluez_conn, bluez_state) = start_mock_bluez_with_options(
        &bus_addr,
        MockBluezOptions {
            fail_connect: true,
            ..Default::default()
        },
    )
    .await;

    let (_daemon_guard, _client, socket_path) = spawn_daemon_and_connect(&temp, &bus_addr).await;
    let (mut app, mut event_rx, cmd_tx, shutdown_tx, bridge_task) =
        connect_tui_app_to_daemon(socket_path).await;

    scan_and_select_wearable_via_enter(&mut app, &mut event_rx, &cmd_tx, &bluez_conn, &bluez_state)
        .await;

    let enrolled_with_warning = wait_for_tui_app(&mut app, &mut event_rx, &cmd_tx, |a| {
        a.enrolled_wearables
            .iter()
            .any(|w| w.wearable.mac == DEVICE_MAC && w.status == StreamStatus::Disconnected)
            && a.op_feedback
                .as_ref()
                .is_some_and(|fb| !fb.is_error && fb.message.contains("initial connection failed"))
    })
    .await;

    let _ = shutdown_tx.send(true);
    let _ = bridge_task.await;

    assert!(
        enrolled_with_warning,
        "enrollment must succeed as Disconnected and surface connection warning in TUI; enrolled={:?}, feedback={:?}",
        app.enrolled_wearables, app.op_feedback
    );
    assert!(!bluez_state.connected.load(Ordering::SeqCst));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enroll_select_on_already_connected_wearable_produces_no_connect_warning() {
    let temp = TempDir::new().unwrap();
    let (_dbus_guard, bus_addr) = spawn_private_dbus(&temp);
    // Wearable is already Connected=true before enroll_select runs
    let (bluez_conn, _bluez_state) = start_mock_bluez(&bus_addr, true, true, true).await;

    // Directly verify connect_device treats org.bluez.Error.AlreadyConnected as Ok(())
    let mut mock_err = bpmute::bluetooth::operations::connect_device(&bluez_conn, DEVICE_MAC).await;
    assert!(
        mock_err.is_ok(),
        "connect_device must treat Connected=true as Ok: {mock_err:?}"
    );
    let device_proxy = bpmute::bluetooth::dbus::Device1Proxy::builder(&bluez_conn)
        .path(DEVICE_PATH)
        .unwrap()
        .build()
        .await
        .unwrap();
    mock_err = device_proxy.connect().await;
    assert!(
        matches!(
            mock_err,
            Err(zbus::Error::MethodError(ref name, _, _))
                if name.as_str() == "org.bluez.Error.AlreadyConnected"
        ),
        "MockDevice::connect must emit org.bluez.Error.AlreadyConnected, got: {mock_err:?}"
    );

    let (_daemon_guard, mut client, _socket_path) =
        spawn_daemon_and_connect(&temp, &bus_addr).await;

    let start_res = client
        .request("wearable.enroll_start", json!({}))
        .await
        .unwrap();
    assert_eq!(start_res["wearables"].as_array().unwrap().len(), 1);

    let select_res = client
        .request("wearable.enroll_select", json!({ "address": DEVICE_MAC }))
        .await
        .unwrap();
    assert!(
        select_res["warnings"].as_array().unwrap().is_empty(),
        "already-connected wearable must not trigger a connect warning; warnings={:?}",
        select_res["warnings"]
    );
    assert!(wait_for_wearable_status(&mut client, "acquiring").await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tui_enroll_select_second_reachable_wearable_enters_passive_standby() {
    use bpmute::sleep_engine::StreamStatus;

    let temp = TempDir::new().unwrap();
    let (_dbus_guard, bus_addr) = spawn_private_dbus(&temp);
    let (_bluez_conn, bluez_state) = start_mock_bluez_with_options(
        &bus_addr,
        MockBluezOptions {
            expose_wearable: true,
            initial_connected: true,
            initial_gatt: true,
            expose_second_wearable: true,
            ..Default::default()
        },
    )
    .await;

    let (_daemon_guard, mut client, socket_path) = spawn_daemon_and_connect(&temp, &bus_addr).await;

    // 1. Enroll first wearable so it becomes the active Acquiring stream
    client
        .request("wearable.enroll_start", json!({}))
        .await
        .unwrap();
    client
        .request("wearable.enroll_select", json!({ "address": DEVICE_MAC }))
        .await
        .unwrap();
    assert!(wait_for_wearable_status(&mut client, "acquiring").await);

    // 2. Enroll second reachable wearable via TUI -> connects and enters PassiveStandby
    let (mut app, mut event_rx, cmd_tx, shutdown_tx, bridge_task) =
        connect_tui_app_to_daemon(socket_path).await;
    open_wearables_and_start_scan(&mut app, &cmd_tx).await;
    assert!(
        wait_for_tui_discovered_wearable(&mut app, &mut event_rx, &cmd_tx, DEVICE2_MAC).await,
        "second wearable must be discovered"
    );

    // Select DEVICE2_MAC (rows are sorted by MAC: 11:22... is index 0)
    let select_cmd = app
        .handle_event(bpmute::tui::TuiEvent::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        )))
        .unwrap();
    cmd_tx.send(select_cmd).await.unwrap();

    let entered_standby = wait_for_tui_enrolled_status(
        &mut app,
        &mut event_rx,
        &cmd_tx,
        DEVICE2_MAC,
        StreamStatus::PassiveStandby,
    )
    .await;

    let _ = shutdown_tx.send(true);
    let _ = bridge_task.await;

    assert!(
        entered_standby,
        "second reachable wearable enrolled while first is active must enter PassiveStandby; enrolled={:?}",
        app.enrolled_wearables
    );
    assert!(bluez_state.second_connected.load(Ordering::SeqCst));
    assert!(!bluez_state.second_notify_started.load(Ordering::SeqCst));
}

#[tokio::test]
async fn read_battery_percentage_reads_gatt_2a19_when_battery1_missing() {
    let temp = TempDir::new().unwrap();
    let (_dbus_guard, bus_addr) = spawn_private_dbus(&temp);
    let (conn, _state) = start_mock_bluez_with_options(
        &bus_addr,
        MockBluezOptions {
            expose_wearable: true,
            expose_hrs_uuid: true,
            initial_connected: true,
            initial_gatt: true,
            expose_battery_char: true,
            battery_level: 85,
            ..Default::default()
        },
    )
    .await;

    let res = bpmute::bluetooth::operations::read_battery_percentage(&conn, DEVICE_MAC).await;
    assert_eq!(res.unwrap(), Some(85));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bluez_daemon_reads_and_updates_battery_from_gatt_2a19() {
    let temp = TempDir::new().unwrap();
    let (_dbus_guard, bus_addr) = spawn_private_dbus(&temp);
    let (bluez_conn, _bluez_state) = start_mock_bluez_with_options(
        &bus_addr,
        MockBluezOptions {
            expose_wearable: true,
            expose_hrs_uuid: true,
            initial_connected: true,
            initial_gatt: true,
            expose_battery_char: true,
            battery_level: 85,
            ..Default::default()
        },
    )
    .await;

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
    assert!(enroll_out.status.success());

    assert!(
        wait_for_wearable_status(&mut client, "acquiring").await
            || wait_for_wearable_status(&mut client, "streaming").await
    );

    // Initial battery percentage from GATT 0x2A19 must be read into wearable status
    assert!(
        wait_for_wearable_battery(&mut client, 85).await,
        "battery percentage 85% must be reflected in wearable.list"
    );

    // When GATT 0x2A19 emits PropertiesChanged with new battery level 79%
    let mut changed: HashMap<&str, Value<'_>> = HashMap::new();
    changed.insert("Value", Value::from(vec![79u8]));
    let invalidated: Vec<&str> = Vec::new();
    bluez_conn
        .emit_signal(
            Option::<zbus::names::BusName>::None,
            BATTERY_CHAR_PATH,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &("org.bluez.GattCharacteristic1", changed, invalidated),
        )
        .await
        .unwrap();

    assert!(
        wait_for_wearable_battery(&mut client, 79).await,
        "battery percentage 79% must be reflected in wearable.list after PropertiesChanged"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bluez_daemon_clears_battery_on_disconnect_and_ignores_disconnected_gatt_cache() {
    let temp = TempDir::new().unwrap();
    let (_dbus_guard, bus_addr) = spawn_private_dbus(&temp);
    let (bluez_conn, bluez_state) = start_mock_bluez_with_options(
        &bus_addr,
        MockBluezOptions {
            expose_wearable: true,
            expose_hrs_uuid: true,
            initial_connected: false,
            initial_gatt: true,
            fail_connect: true,
            expose_battery_char: true,
            battery_level: 85,
            ..Default::default()
        },
    )
    .await;

    let (_daemon_guard, mut client, socket_path) = spawn_daemon_and_connect(&temp, &bus_addr).await;

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

    // While disconnected, cached BlueZ 0x2A19 value must not populate battery
    let initial = client.request("wearable.list", json!({})).await.unwrap();
    assert_eq!(initial["wearables"][0]["status"], "disconnected");
    assert!(
        initial["wearables"][0]["battery"].is_null(),
        "disconnected wearable must not show cached GATT battery: {initial}"
    );

    // Connect -> battery 85% appears
    bluez_state.fail_connect.store(false, Ordering::SeqCst);
    bluez_state.connected.store(true, Ordering::SeqCst);
    let empty_inv: Vec<&str> = Vec::new();
    let mut dev_changed: HashMap<&str, Value<'_>> = HashMap::new();
    dev_changed.insert("Connected", Value::from(true));
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

    assert!(
        wait_for_wearable_status(&mut client, "acquiring").await
            || wait_for_wearable_status(&mut client, "streaming").await
    );
    assert!(wait_for_wearable_battery(&mut client, 85).await);

    // Disconnect -> battery clears back to null
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
    let after_dc = client.request("wearable.list", json!({})).await.unwrap();
    assert!(
        after_dc["wearables"][0]["battery"].is_null(),
        "battery must be cleared after BlueZ disconnect: {after_dc}"
    );
}

struct MockNotifications {
    tx: mpsc::UnboundedSender<(String, String)>,
    next_id: std::sync::atomic::AtomicU32,
}

#[interface(name = "org.freedesktop.Notifications")]
impl MockNotifications {
    #[allow(
        clippy::too_many_arguments,
        reason = "org.freedesktop.Notifications.Notify D-Bus method signature requires 8 parameters"
    )]
    fn notify(
        &self,
        _app_name: &str,
        _replaces_id: u32,
        _app_icon: &str,
        summary: &str,
        body: &str,
        _actions: Vec<String>,
        _hints: HashMap<String, zbus::zvariant::OwnedValue>,
        _expire_timeout: i32,
    ) -> u32 {
        let _ = self.tx.send((summary.to_string(), body.to_string()));
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }
}

async fn start_mock_notifications(
    temp: &TempDir,
) -> (
    ChildGuard,
    String,
    zbus::Connection,
    mpsc::UnboundedReceiver<(String, String)>,
) {
    let (guard, session_addr) = spawn_private_dbus_named(temp, "session-bus.sock");
    let (tx, rx) = mpsc::unbounded_channel();
    let conn = zbus::connection::Builder::address(session_addr.as_str())
        .expect("valid session dbus addr")
        .name("org.freedesktop.Notifications")
        .expect("mock Notifications bus name")
        .serve_at(
            "/org/freedesktop/Notifications",
            MockNotifications {
                tx,
                next_id: std::sync::atomic::AtomicU32::new(1),
            },
        )
        .unwrap()
        .build()
        .await
        .expect("mock notifications connection");
    (guard, session_addr, conn, rx)
}

async fn wait_for_notification(
    rx: &mut mpsc::UnboundedReceiver<(String, String)>,
    expected_summary: &str,
) -> Option<(String, String)> {
    tokio::time::timeout(Duration::from_secs(3), async {
        while let Some((summary, body)) = rx.recv().await {
            if summary == expected_summary {
                return Some((summary, body));
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

async fn wait_for_daemon_status<T>(
    client: &mut IpcClient,
    extract: impl Fn(&serde_json::Value) -> Option<T>,
) -> Option<T> {
    poll_ipc_until(client, "daemon.status", extract).await
}

struct StreamingDaemonFixture {
    _temp: TempDir,
    _dbus_guard: ChildGuard,
    _sess_guard: ChildGuard,
    _notif_conn: zbus::Connection,
    _daemon_guard: ChildGuard,
    bluez_conn: zbus::Connection,
    bluez_state: MockBluezState,
    client: IpcClient,
    notif_rx: mpsc::UnboundedReceiver<(String, String)>,
}

async fn setup_streaming_daemon_fixture() -> StreamingDaemonFixture {
    let temp = TempDir::new().unwrap();
    let (_dbus_guard, bus_addr) = spawn_private_dbus(&temp);
    let (_sess_guard, session_addr, _notif_conn, mut notif_rx) =
        start_mock_notifications(&temp).await;
    let (bluez_conn, bluez_state) = start_mock_bluez(&bus_addr, true, true, true).await;
    let (_daemon_guard, mut client, socket_path) =
        spawn_daemon_with_session_bus(&temp, &bus_addr, &session_addr).await;

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

    assert!(
        wait_for_wearable_status(&mut client, "acquiring").await
            || wait_for_wearable_status(&mut client, "streaming").await
    );
    assert!(
        wait_for_notification(&mut notif_rx, "BPMute Wearable Connected")
            .await
            .is_some()
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
        .unwrap();

    assert!(
        wait_for_daemon_status(&mut client, |res| {
            (res["monitoring_state"].as_str() == Some("Warmup")).then_some(())
        })
        .await
        .is_some()
    );

    StreamingDaemonFixture {
        _temp: temp,
        _dbus_guard,
        _sess_guard,
        _notif_conn,
        _daemon_guard,
        bluez_conn,
        bluez_state,
        client,
        notif_rx,
    }
}

async fn emit_device_connected_prop(
    bluez_conn: &zbus::Connection,
    bluez_state: &MockBluezState,
    connected: bool,
) {
    bluez_state.connected.store(connected, Ordering::SeqCst);
    if !connected {
        bluez_state.notify_started.store(false, Ordering::SeqCst);
    }
    let mut changed: HashMap<&str, Value<'_>> = HashMap::new();
    changed.insert("Connected", Value::from(connected));
    let empty_inv: Vec<&str> = Vec::new();
    bluez_conn
        .emit_signal(
            Option::<zbus::names::BusName>::None,
            DEVICE_PATH,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &("org.bluez.Device1", changed, &empty_inv),
        )
        .await
        .unwrap();
}

async fn emit_device_disconnected_signal(
    bluez_conn: &zbus::Connection,
    reason: &str,
    message: &str,
) {
    bluez_conn
        .emit_signal(
            Option::<zbus::names::BusName>::None,
            DEVICE_PATH,
            "org.bluez.Device1",
            "Disconnected",
            &(reason, message),
        )
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bluez_device_disconnected_remote_before_connected_false_tightens_deadline_and_notifies_immediately()
 {
    let mut fx = setup_streaming_daemon_fixture().await;

    let t_before = Utc::now().timestamp_millis();
    fx.bluez_state.connected.store(false, Ordering::SeqCst);
    fx.bluez_state.notify_started.store(false, Ordering::SeqCst);
    emit_device_disconnected_signal(
        &fx.bluez_conn,
        "org.bluez.Reason.Remote",
        "Connection Terminated by Remote User",
    )
    .await;

    let notif = wait_for_notification(&mut fx.notif_rx, "BPMute Wearable Disconnected").await;
    assert!(
        notif.is_some(),
        "Remote Device1.Disconnected must notify immediately without 15s debounce"
    );

    let deadline_2m = wait_for_daemon_status(&mut fx.client, |res| {
        res["session_close_deadline_ms"]
            .as_i64()
            .filter(|&d| d > t_before)
    })
    .await
    .expect("session_close_deadline_ms must be set");
    assert!(
        deadline_2m - t_before <= 125_000,
        "Remote disconnect must set 2-minute deadline, got delta {} ms",
        deadline_2m - t_before
    );

    // Trailing PropertiesChanged(Connected = false) must be deduplicated
    emit_device_connected_prop(&fx.bluez_conn, &fx.bluez_state, false).await;
    for _ in 0..5 {
        tokio::task::yield_now().await;
        let status_after = fx.client.request("daemon.status", json!({})).await.unwrap();
        assert_eq!(
            status_after["session_close_deadline_ms"].as_i64(),
            Some(deadline_2m),
            "trailing Connected=false must not extend or alter 2-minute deadline"
        );
    }
    assert!(
        fx.notif_rx.try_recv().is_err(),
        "trailing Connected=false must not duplicate disconnect notification"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bluez_connected_false_before_device_disconnected_remote_tightens_deadline_and_notifies_immediately()
 {
    let mut fx = setup_streaming_daemon_fixture().await;

    // 1. PropertiesChanged(Connected = false) arrives first -> 15 min deadline, debounced notification
    let t_before = Utc::now().timestamp_millis();
    emit_device_connected_prop(&fx.bluez_conn, &fx.bluez_state, false).await;
    assert!(wait_for_wearable_status(&mut fx.client, "disconnected").await);

    let deadline_15m = wait_for_daemon_status(&mut fx.client, |res| {
        res["session_close_deadline_ms"]
            .as_i64()
            .filter(|&d| d - t_before > 800_000)
    })
    .await
    .expect("Connected=false must initially set ~15-minute close deadline");
    assert!(
        fx.notif_rx.try_recv().is_err(),
        "Connected=false alone must debounce disconnect notification"
    );

    // 2. Device1.Disconnected("org.bluez.Reason.Remote") arrives shortly after -> tightens to 2m & notifies
    emit_device_disconnected_signal(
        &fx.bluez_conn,
        "org.bluez.Reason.Remote",
        "Connection Terminated by Remote User",
    )
    .await;

    let notif = wait_for_notification(&mut fx.notif_rx, "BPMute Wearable Disconnected").await;
    assert!(
        notif.is_some(),
        "follow-up Remote signal must cancel debounce and fire NotifyDisconnected immediately"
    );

    let deadline_2m = wait_for_daemon_status(&mut fx.client, |res| {
        res["session_close_deadline_ms"]
            .as_i64()
            .filter(|&d| d < deadline_15m)
    })
    .await
    .expect("follow-up Remote signal must tighten session_close_deadline_ms");
    assert!(
        deadline_2m - t_before <= 125_000,
        "tightened deadline must be ~2 minutes from disconnect, got delta {} ms",
        deadline_2m - t_before
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bluez_device_disconnected_authentication_emits_pairing_alert_regardless_of_signal_order() {
    let mut fx = setup_streaming_daemon_fixture().await;

    // A: Authentication before Connected=false
    fx.bluez_state.connected.store(false, Ordering::SeqCst);
    fx.bluez_state.notify_started.store(false, Ordering::SeqCst);
    emit_device_disconnected_signal(
        &fx.bluez_conn,
        "org.bluez.Reason.Authentication",
        "Authentication Failure",
    )
    .await;
    assert!(
        wait_for_notification(&mut fx.notif_rx, "BPMute Wearable Pairing Alert")
            .await
            .is_some(),
        "Authentication before Connected=false must emit pairing alert"
    );
    emit_device_connected_prop(&fx.bluez_conn, &fx.bluez_state, false).await;
    assert!(wait_for_wearable_status(&mut fx.client, "disconnected").await);

    // Reconnect
    emit_device_connected_prop(&fx.bluez_conn, &fx.bluez_state, true).await;
    assert!(
        wait_for_wearable_status(&mut fx.client, "acquiring").await
            || wait_for_wearable_status(&mut fx.client, "streaming").await
    );
    let _ = wait_for_notification(&mut fx.notif_rx, "BPMute Wearable Connected").await;

    // B: Connected=false before Authentication
    emit_device_connected_prop(&fx.bluez_conn, &fx.bluez_state, false).await;
    assert!(wait_for_wearable_status(&mut fx.client, "disconnected").await);
    emit_device_disconnected_signal(
        &fx.bluez_conn,
        "org.bluez.Reason.Authentication",
        "Authentication Failure",
    )
    .await;
    assert!(
        wait_for_notification(&mut fx.notif_rx, "BPMute Wearable Pairing Alert")
            .await
            .is_some(),
        "Authentication shortly after Connected=false must emit pairing alert"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bluez_legacy_host_without_device_disconnected_signal_falls_back_to_timeout_window() {
    let mut fx = setup_streaming_daemon_fixture().await;

    let t_before = Utc::now().timestamp_millis();
    emit_device_connected_prop(&fx.bluez_conn, &fx.bluez_state, false).await;
    assert!(wait_for_wearable_status(&mut fx.client, "disconnected").await);

    let deadline = wait_for_daemon_status(&mut fx.client, |res| {
        res["session_close_deadline_ms"]
            .as_i64()
            .filter(|&d| d - t_before > 800_000)
    })
    .await
    .expect("legacy BlueZ Connected=false must fall back to 15-minute close deadline");
    assert!(deadline - t_before <= 905_000);
    assert!(
        fx.notif_rx.try_recv().is_err(),
        "legacy BlueZ Connected=false must retain 15-second RF debounce"
    );
}
