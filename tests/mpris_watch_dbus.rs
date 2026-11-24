mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bpmute::actions::SystemActionExecutor;
use bpmute::actions::traits::ActionExecutor;
use common::dbus::{PrivateDbusGuard, spawn_private_session_dbus};
use tempfile::TempDir;
use tokio::sync::watch;
use zbus::interface;

const MPRIS_OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";

struct MockPlayerState {
    playback_status: Mutex<String>,
    can_pause: AtomicBool,
    pause_calls: AtomicUsize,
    emitting_signal: AtomicBool,
    status_reads_tx: watch::Sender<usize>,
}

impl MockPlayerState {
    fn new(initial_status: &str, can_pause: bool) -> Arc<Self> {
        let (status_reads_tx, _rx) = watch::channel(0_usize);
        Arc::new(Self {
            playback_status: Mutex::new(initial_status.to_string()),
            can_pause: AtomicBool::new(can_pause),
            pause_calls: AtomicUsize::new(0),
            emitting_signal: AtomicBool::new(false),
            status_reads_tx,
        })
    }

    fn status_reads(&self) -> usize {
        *self.status_reads_tx.borrow()
    }
}

struct MockMprisPlayer {
    state: Arc<MockPlayerState>,
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl MockMprisPlayer {
    /// Models browsers that resolve `Pause()` before asynchronously updating `PlaybackStatus`.
    async fn pause(&self) {
        self.state.pause_calls.fetch_add(1, Ordering::SeqCst);
    }

    #[zbus(property)]
    async fn playback_status(&self) -> String {
        let status = self
            .state
            .playback_status
            .lock()
            .expect("mock playback_status mutex poisoned")
            .clone();
        if !self.state.emitting_signal.load(Ordering::SeqCst) {
            self.state.status_reads_tx.send_modify(|count| *count += 1);
        }
        status
    }

    #[zbus(property)]
    async fn can_pause(&self) -> bool {
        self.state.can_pause.load(Ordering::SeqCst)
    }
}

struct MprisHarness {
    _temp: TempDir,
    _bus_guard: PrivateDbusGuard,
    bus_addr: String,
}

impl MprisHarness {
    fn new() -> Self {
        let temp = TempDir::new().expect("create temp dir for private D-Bus");
        let (bus_guard, bus_addr) = spawn_private_session_dbus(&temp);
        Self {
            _temp: temp,
            _bus_guard: bus_guard,
            bus_addr,
        }
    }

    async fn register_player(
        &self,
        bus_name: &str,
        initial_status: &str,
        can_pause: bool,
    ) -> (zbus::Connection, Arc<MockPlayerState>) {
        let state = MockPlayerState::new(initial_status, can_pause);
        let conn = zbus::connection::Builder::address(self.bus_addr.as_str())
            .expect("valid private bus address")
            .name(bus_name)
            .expect("valid MPRIS bus name")
            .serve_at(
                MPRIS_OBJECT_PATH,
                MockMprisPlayer {
                    state: Arc::clone(&state),
                },
            )
            .expect("serve mock MPRIS player")
            .build()
            .await
            .expect("connect mock MPRIS player to private bus");
        (conn, state)
    }

    fn executor(&self, watch_timeout: Duration) -> SystemActionExecutor {
        SystemActionExecutor::with_session_bus_address(&self.bus_addr)
            .with_mpris_watch_timeout(watch_timeout)
    }

    fn spawn_watch(
        &self,
        watch_timeout: Duration,
        targets: Vec<String>,
    ) -> tokio::task::JoinHandle<bool> {
        let executor = self.executor(watch_timeout);
        tokio::spawn(async move { executor.watch_mpris_resume(&targets).await })
    }
}

async fn wait_for_status_reads(state: &MockPlayerState, target: usize) {
    let mut rx = state.status_reads_tx.subscribe();
    tokio::time::timeout(
        Duration::from_secs(3),
        rx.wait_for(|&count| count >= target),
    )
    .await
    .expect("timed out waiting for PlaybackStatus property read")
    .expect("status_reads watch channel closed");
}

async fn set_playback_status(conn: &zbus::Connection, state: &MockPlayerState, new_status: &str) {
    {
        let mut guard = state
            .playback_status
            .lock()
            .expect("mock playback_status mutex poisoned");
        *guard = new_status.to_string();
    }
    let iface_ref = conn
        .object_server()
        .interface::<_, MockMprisPlayer>(MPRIS_OBJECT_PATH)
        .await
        .expect("mock MPRIS interface must be registered");
    state.emitting_signal.store(true, Ordering::SeqCst);
    let res = iface_ref
        .get()
        .await
        .playback_status_changed(iface_ref.signal_context())
        .await;
    state.emitting_signal.store(false, Ordering::SeqCst);
    res.expect("emit PlaybackStatus PropertiesChanged signal");
}

async fn await_watch_bounded(handle: tokio::task::JoinHandle<bool>) -> bool {
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("watch_mpris_resume timed out")
        .expect("watch_mpris_resume task panicked")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_mpris_resume_ignores_stale_initial_playing_followed_by_paused() {
    let harness = MprisHarness::new();
    let bus_name = "org.mpris.MediaPlayer2.browser";
    let (server_conn, state) = harness.register_player(bus_name, "Playing", true).await;

    let watch_handle = harness.spawn_watch(Duration::from_millis(400), vec![bus_name.to_string()]);

    wait_for_status_reads(&state, 1).await;
    set_playback_status(&server_conn, &state, "Paused").await;

    assert!(
        !await_watch_bounded(watch_handle).await,
        "stale initial Playing followed by Paused must not trigger rapid-wake FP"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_mpris_resume_triggers_after_paused_then_playing() {
    let harness = MprisHarness::new();
    let bus_name = "org.mpris.MediaPlayer2.browser";
    let (server_conn, state) = harness.register_player(bus_name, "Playing", true).await;

    let watch_handle = harness.spawn_watch(Duration::from_secs(5), vec![bus_name.to_string()]);

    wait_for_status_reads(&state, 1).await;
    set_playback_status(&server_conn, &state, "Paused").await;
    set_playback_status(&server_conn, &state, "Playing").await;

    assert!(
        await_watch_bounded(watch_handle).await,
        "Playing -> Paused -> Playing transition must trigger resume"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_mpris_resume_triggers_after_stopped_then_playing() {
    let harness = MprisHarness::new();
    let bus_name = "org.mpris.MediaPlayer2.browser";
    let (server_conn, state) = harness.register_player(bus_name, "Playing", true).await;

    let watch_handle = harness.spawn_watch(Duration::from_secs(5), vec![bus_name.to_string()]);

    wait_for_status_reads(&state, 1).await;
    set_playback_status(&server_conn, &state, "Stopped").await;
    set_playback_status(&server_conn, &state, "Playing").await;

    assert!(
        await_watch_bounded(watch_handle).await,
        "Playing -> Stopped -> Playing transition must trigger resume"
    );
}

async fn assert_resume_from_initial_non_playing(initial_status: &str) {
    let harness = MprisHarness::new();
    let bus_name = "org.mpris.MediaPlayer2.player";
    let (server_conn, state) = harness
        .register_player(bus_name, initial_status, true)
        .await;

    let watch_handle = harness.spawn_watch(Duration::from_secs(5), vec![bus_name.to_string()]);

    wait_for_status_reads(&state, 1).await;
    set_playback_status(&server_conn, &state, "Playing").await;

    assert!(
        await_watch_bounded(watch_handle).await,
        "initial {initial_status} followed by Playing must trigger resume"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_mpris_resume_triggers_from_already_paused_and_stopped_states() {
    assert_resume_from_initial_non_playing("Paused").await;
    assert_resume_from_initial_non_playing("Stopped").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_mpris_resume_ignores_player_stuck_in_playing() {
    let harness = MprisHarness::new();
    let bus_name = "org.mpris.MediaPlayer2.stuck";
    let (server_conn, state) = harness.register_player(bus_name, "Playing", true).await;

    let watch_handle = harness.spawn_watch(Duration::from_millis(400), vec![bus_name.to_string()]);

    wait_for_status_reads(&state, 1).await;
    // Re-emit Playing without ever transitioning through Paused or Stopped.
    set_playback_status(&server_conn, &state, "Playing").await;

    assert!(
        !await_watch_bounded(watch_handle).await,
        "player that never leaves Playing must not trigger bogus resume"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_mpris_resume_ignores_malformed_intermediate_status() {
    let harness = MprisHarness::new();
    let bus_name = "org.mpris.MediaPlayer2.quirky";
    let (server_conn, state) = harness.register_player(bus_name, "Playing", true).await;

    let watch_handle = harness.spawn_watch(Duration::from_millis(400), vec![bus_name.to_string()]);

    wait_for_status_reads(&state, 1).await;
    set_playback_status(&server_conn, &state, "").await;
    set_playback_status(&server_conn, &state, "Playing").await;

    assert!(
        !await_watch_bounded(watch_handle).await,
        "malformed intermediate status must not be treated as Paused/Stopped"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_mpris_players_then_watch_mpris_resume_handles_async_pause_race() {
    let harness = MprisHarness::new();
    let bus_name = "org.mpris.MediaPlayer2.firefox";
    let (server_conn, state) = harness.register_player(bus_name, "Playing", true).await;

    let executor = harness.executor(Duration::from_millis(400));
    let paused = executor.pause_mpris_players(&[]).await.unwrap();
    assert_eq!(paused, vec![bus_name.to_string()]);
    assert_eq!(state.pause_calls.load(Ordering::SeqCst), 1);

    let reads_before_watch = state.status_reads();
    let watch_handle = tokio::spawn(async move { executor.watch_mpris_resume(&paused).await });

    wait_for_status_reads(&state, reads_before_watch + 1).await;
    set_playback_status(&server_conn, &state, "Paused").await;

    assert!(
        !await_watch_bounded(watch_handle).await,
        "async browser Pause() settling to Paused after watch starts must not trigger FP"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_mpris_resume_tracks_non_playing_state_independently_per_player() {
    let harness = MprisHarness::new();
    let stuck_name = "org.mpris.MediaPlayer2.stuck";
    let browser_name = "org.mpris.MediaPlayer2.browser";
    let (stuck_conn, stuck_state) = harness.register_player(stuck_name, "Playing", true).await;
    let (browser_conn, browser_state) =
        harness.register_player(browser_name, "Playing", true).await;

    let targets = vec![stuck_name.to_string(), browser_name.to_string()];

    // 1) browser pauses while stuck remains Playing (and emits Playing again) -> false
    let watch1 = harness.spawn_watch(Duration::from_millis(400), targets.clone());

    wait_for_status_reads(&stuck_state, 1).await;
    wait_for_status_reads(&browser_state, 1).await;
    set_playback_status(&browser_conn, &browser_state, "Paused").await;
    set_playback_status(&stuck_conn, &stuck_state, "Playing").await;
    assert!(
        !await_watch_bounded(watch1).await,
        "stuck player remaining Playing must not trigger FP while browser stays Paused"
    );

    // 2) Reset browser to Playing; browser pauses then resumes while stuck stays Playing -> true
    set_playback_status(&browser_conn, &browser_state, "Playing").await;
    let stuck_reads = stuck_state.status_reads();
    let browser_reads = browser_state.status_reads();

    let watch2 = harness.spawn_watch(Duration::from_secs(5), targets);

    wait_for_status_reads(&stuck_state, stuck_reads + 1).await;
    wait_for_status_reads(&browser_state, browser_reads + 1).await;
    set_playback_status(&browser_conn, &browser_state, "Paused").await;
    set_playback_status(&browser_conn, &browser_state, "Playing").await;
    assert!(
        await_watch_bounded(watch2).await,
        "browser pausing and resuming must trigger true even when another player stays Playing"
    );
}
