//! Детекция смерти процесса пира (протокол 0.8+: обмен PID в handshake +
//! удерживаемый handle процесса).
//!
//! Пир -- дочерний процесс (этот же тестовый бинарь, перезапущенный с
//! переменной окружения). Родитель убивает его (`TerminateProcess`) и меряет,
//! через сколько его сторона узнаёт о `PeerDied`. Контроль: штатный выход
//! пира должен давать `Graceful`, а не `PeerDied`.

use std::ffi::c_void;
use std::process::{Child, Command};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use xshm::{
    Admission, AutoClient, AutoHandler, AutoOptions, AutoServer, Beacon, ChannelLayout,
    ClientRegistration, DisconnectReason, DispatchClient, DispatchClientHandler,
    DispatchClientOptions, DispatchHandler, DispatchOptions, DispatchServer, MultiClient,
    MultiClientHandler, MultiClientOptions, MultiHandler, MultiOptions, MultiServer, RingConfig,
    SharedClient, SharedServer, ShmError, SpinPolicy,
};

const CHILD_ENV: &str = "XSHM_PEER_DEATH_CHILD";
/// Граница задержки детекции, которую проверяем.
const BOUND: Duration = Duration::from_millis(500);

#[expect(
    clippy::unwrap_used,
    reason = "тестовый помощник: паника -- провал теста"
)]
fn unique_name(tag: &str) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("XSHM_PD_{tag}_{}_{}", std::process::id(), ts % 1_000_000)
}

// ─── дочерний процесс ───────────────────────────────────────────────────────

struct Noop;
impl AutoHandler for Noop {}
impl DispatchHandler for Noop {
    fn on_client_connect(&self, _id: u32, _info: &ClientRegistration) {}
    fn on_client_disconnect(&self, _id: u32) {}
    fn on_message(&self, _id: u32, _data: &[u8]) {}
}
impl DispatchClientHandler for Noop {
    fn on_connect(&self, _id: u32, _channel: &str) {}
    fn on_disconnect(&self) {}
    fn on_message(&self, _data: &[u8]) {}
}
impl MultiHandler for Noop {
    fn on_client_connect(&self, _id: u32) {}
    fn on_client_disconnect(&self, _id: u32) {}
    fn on_message(&self, _id: u32, _data: &[u8]) {}
}
impl MultiClientHandler for Noop {
    fn on_connect(&self, _slot: u32) {}
    fn on_disconnect(&self) {}
    fn on_message(&self, _data: &[u8]) {}
}

/// Маяк «клиент лобби ждёт ответа» (`dispatch_lobby_stall`).
fn admit_marker(name: &str) -> String {
    format!("{name}_ADMIT")
}

/// Обработчик лобби, который не отвечает никогда: `admit` висит.
struct StallingLobby {
    marker: Beacon,
}

impl DispatchHandler for StallingLobby {
    fn admit(&self, _info: &ClientRegistration) -> Admission {
        let _ = self.marker.raise();
        linger(Duration::from_secs(60));
        Admission::Accept
    }
    fn on_client_connect(&self, _id: u32, _info: &ClientRegistration) {}
    fn on_client_disconnect(&self, _id: u32) {}
    fn on_message(&self, _id: u32, _data: &[u8]) {}
}

/// Держит процесс живым, пока его не убьют (с потолком на случай, если
/// родитель сам упал и не убил ребёнка).
fn linger(limit: Duration) {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
}

/// Точка входа дочернего процесса. Без переменной окружения -- no-op.
#[test]
fn child_entry() {
    let Ok(spec) = std::env::var(CHILD_ENV) else {
        return;
    };
    let (mode, name) = spec.split_once('|').unwrap();
    let forever = Duration::from_secs(60);
    let graceful = Duration::from_millis(400);
    match mode {
        "shared_client" | "shared_client_graceful" => {
            let client = SharedClient::connect(name, Duration::from_secs(10)).unwrap();
            if mode == "shared_client" {
                linger(forever);
            }
            thread::sleep(graceful);
            drop(client); // штатно: откат handshake + DISCONNECT
        }
        "shared_server" => {
            let mut server = SharedServer::start(name).unwrap();
            server
                .wait_for_client(Some(Duration::from_secs(10)))
                .unwrap();
            linger(forever);
        }
        "auto_client" | "auto_client_graceful" => {
            let client = AutoClient::connect(name, Arc::new(Noop), AutoOptions::default()).unwrap();
            if mode == "auto_client" {
                linger(forever);
            }
            thread::sleep(graceful);
            drop(client);
        }
        "auto_server" => {
            let _server = AutoServer::start(name, Arc::new(Noop), AutoOptions::default()).unwrap();
            linger(forever);
        }
        "dispatch_client" | "dispatch_client_graceful" => {
            let client = DispatchClient::connect(
                name,
                ClientRegistration::new(std::process::id(), 1, "peer-death-child"),
                Arc::new(Noop),
                DispatchClientOptions::default(),
            )
            .unwrap();
            if mode == "dispatch_client" {
                linger(forever);
            }
            thread::sleep(graceful);
            client.stop();
            drop(client);
            thread::sleep(Duration::from_millis(100));
        }
        "dispatch_server" => {
            let server =
                DispatchServer::start(name, Arc::new(Noop), DispatchOptions::default()).unwrap();
            linger(forever);
            server.stop();
        }
        "multi_client" => {
            let _client =
                MultiClient::connect(name, Arc::new(Noop), MultiClientOptions::default()).unwrap();
            linger(forever);
        }
        "multi_server" => {
            let server = MultiServer::start(name, Arc::new(Noop), MultiOptions::default()).unwrap();
            linger(forever);
            server.stop();
        }
        "dispatch_lobby_stall" => {
            // Лобби, которое никогда не отвечает: `admit` взводит маяк
            // `<имя>_ADMIT` (родитель узнаёт, что клиент уже ждёт ответа) и
            // висит, пока процесс не убьют.
            let marker = Beacon::open(&admit_marker(name)).unwrap();
            let server = DispatchServer::start(
                name,
                Arc::new(StallingLobby { marker }),
                DispatchOptions::default(),
            )
            .unwrap();
            linger(forever);
            server.stop();
        }
        "claim_and_exit" => {
            // Task 12: держатель, умерший до `CLIENT_HELLO` -- захватить
            // claim канала (C1, C2: время своего процесса, затем PID) и
            // выйти, не выставив заявки.
            claim_as_holder(name);
            thread::sleep(graceful);
        }
        other => panic!("unknown child mode {other}"),
    }
}

/// Дочерний процесс, который гарантированно убивается при выходе из теста.
struct Peer(Child);

impl Peer {
    #[expect(
        clippy::expect_used,
        clippy::unwrap_used,
        reason = "тестовый помощник: паника -- провал теста"
    )]
    fn spawn(mode: &str, name: &str) -> Self {
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "child_entry", "--nocapture", "--test-threads=1"])
            .env(CHILD_ENV, format!("{mode}|{name}"))
            .spawn()
            .expect("spawn child");
        Self(child)
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }

    /// Убить (TerminateProcess) и вернуть момент убийства.
    #[expect(
        clippy::expect_used,
        reason = "тестовый помощник: паника -- провал теста"
    )]
    fn kill(&mut self) -> Instant {
        let t0 = Instant::now();
        self.0.kill().expect("kill child");
        t0
    }

    #[expect(
        clippy::unwrap_used,
        reason = "тестовый помощник: паника -- провал теста"
    )]
    fn wait_exit(&mut self, limit: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "дочерний процесс не завершился");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        drop(self.0.kill()); // best-effort в Drop: процесс мог уже выйти
        drop(self.0.wait());
    }
}

fn wait_until(what: &str, limit: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !cond() {
        assert!(Instant::now() < deadline, "таймаут: {what}");
        thread::sleep(Duration::from_millis(5));
    }
}

/// Записывает первую причину отключения и момент её получения.
#[derive(Default)]
struct Recorder {
    connected: AtomicBool,
    client_id: AtomicU32,
    reason: Mutex<Option<(DisconnectReason, Instant)>>,
}

impl Recorder {
    #[expect(
        clippy::unwrap_used,
        reason = "тестовый помощник: паника -- провал теста"
    )]
    fn record(&self, reason: DisconnectReason) {
        let mut slot = self.reason.lock().unwrap();
        if slot.is_none() {
            *slot = Some((reason, Instant::now()));
        }
    }

    #[expect(
        clippy::unwrap_used,
        reason = "тестовый помощник: паника -- провал теста"
    )]
    fn wait_reason(&self, limit: Duration) -> (DisconnectReason, Instant) {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(r) = *self.reason.lock().unwrap() {
                return r;
            }
            assert!(Instant::now() < deadline, "отключение не сообщено");
            thread::sleep(Duration::from_millis(1));
        }
    }
}

impl AutoHandler for Recorder {
    fn on_connect(&self) {
        self.connected.store(true, Ordering::Release);
    }
    fn on_disconnect_reason(&self, reason: DisconnectReason) {
        self.record(reason);
    }
}

impl DispatchHandler for Recorder {
    fn on_client_connect(&self, client_id: u32, _info: &ClientRegistration) {
        self.client_id.store(client_id, Ordering::Release);
        self.connected.store(true, Ordering::Release);
    }
    fn on_client_disconnect(&self, _client_id: u32) {
        unreachable!("сервер обязан звать on_client_disconnect_reason");
    }
    fn on_client_disconnect_reason(&self, _client_id: u32, reason: DisconnectReason) {
        self.record(reason);
    }
    fn on_message(&self, _client_id: u32, _data: &[u8]) {}
}

impl DispatchClientHandler for Recorder {
    fn on_connect(&self, _client_id: u32, _channel: &str) {
        self.connected.store(true, Ordering::Release);
    }
    fn on_disconnect(&self) {
        unreachable!("клиент обязан звать on_disconnect_reason");
    }
    fn on_disconnect_reason(&self, reason: DisconnectReason) {
        self.record(reason);
    }
    fn on_message(&self, _data: &[u8]) {}
}

impl MultiHandler for Recorder {
    fn on_client_connect(&self, client_id: u32) {
        self.client_id.store(client_id, Ordering::Release);
        self.connected.store(true, Ordering::Release);
    }
    fn on_client_disconnect(&self, _client_id: u32) {
        unreachable!("сервер обязан звать on_client_disconnect_reason");
    }
    fn on_client_disconnect_reason(&self, _client_id: u32, reason: DisconnectReason) {
        self.record(reason);
    }
    fn on_message(&self, _client_id: u32, _data: &[u8]) {}
}

impl MultiClientHandler for Recorder {
    fn on_connect(&self, _slot: u32) {
        self.connected.store(true, Ordering::Release);
    }
    fn on_disconnect(&self) {
        unreachable!("клиент обязан звать on_disconnect_reason");
    }
    fn on_disconnect_reason(&self, reason: DisconnectReason) {
        self.record(reason);
    }
    fn on_message(&self, _data: &[u8]) {}
}

fn report(mode: &str, latency: Duration) {
    println!("{mode}: peer death observed {latency:?} after kill");
    assert!(
        latency < BOUND,
        "{mode}: смерть пира замечена через {latency:?} (граница {BOUND:?})"
    );
}

// ─── Shared (single-client) ─────────────────────────────────────────────────

/// Писатель-сервер ждёт места, читатель-клиент убит: `wait_for_space`
/// возвращает `PeerDied` сразу, а не через таймаут; `poll_client` -- тоже.
#[test]
fn shared_server_sees_client_death() {
    let name = unique_name("SH_S");
    let mut server = SharedServer::start(&name).unwrap();
    let mut peer = Peer::spawn("shared_client", &name);
    server
        .wait_for_client(Some(Duration::from_secs(20)))
        .unwrap();
    assert_eq!(server.peer_pid(), Some(peer.pid()));
    assert_eq!(server.is_peer_alive(), Some(true));

    let big = vec![1u8; 60_000];
    while server.try_send_to_client(&big).is_ok() {}
    // `SharedServer` -- `!Sync` (0.10): ждёт в своём потоке и возвращается.
    let waiter = thread::spawn(move || {
        let r = server.wait_for_space(big.len(), Some(Duration::from_secs(20)));
        (r, Instant::now(), server)
    });
    thread::sleep(Duration::from_millis(100)); // писатель уже спит в ожидании
    let t0 = peer.kill();
    let (result, woke, mut server) = waiter.join().unwrap();
    assert_eq!(result, Err(ShmError::PeerDied));
    report("shared server wait_for_space", woke - t0);
    assert_eq!(server.is_peer_alive(), Some(false));
    let t = Instant::now();
    assert_eq!(
        server.poll_client(Some(Duration::from_secs(10))),
        Err(ShmError::PeerDied)
    );
    assert!(t.elapsed() < BOUND);
    // Завершающий исход `poll_client` помечает сервер отключённым и
    // снимает claim умершего (R18): канал сразу принимает нового клиента.
    assert!(!server.is_connected());
    assert_eq!(
        server.poll_client(Some(Duration::from_secs(10))),
        Err(ShmError::NotConnected)
    );
    let acceptor = thread::spawn(move || {
        let r = server.wait_for_client(Some(Duration::from_secs(10)));
        (r, server)
    });
    let client = SharedClient::connect(&name, Duration::from_secs(10)).unwrap();
    let (accepted, server) = acceptor.join().unwrap();
    assert_eq!(accepted, Ok(()));
    assert!(server.is_connected());
    assert_eq!(server.peer_pid(), Some(std::process::id()));
    drop(client);
}

/// Клиент ждёт данных, сервер убит: `poll_server` возвращает `PeerDied`.
#[test]
fn shared_client_sees_server_death() {
    let name = unique_name("SH_C");
    let mut peer = Peer::spawn("shared_server", &name);
    let mut client = None;
    wait_until("child server", Duration::from_secs(20), || {
        client = SharedClient::connect(&name, Duration::from_millis(500)).ok();
        client.is_some()
    });
    let mut client = client.unwrap();
    assert_eq!(client.peer_pid(), Some(peer.pid()));
    // `SharedClient` -- `!Sync` (0.10): ждёт в своём потоке и возвращается.
    let poller = thread::spawn(move || {
        let r = client.poll_server(Some(Duration::from_secs(20)));
        (r, Instant::now(), client)
    });
    thread::sleep(Duration::from_millis(100));
    let t0 = peer.kill();
    let (result, woke, client) = poller.join().unwrap();
    assert_eq!(result, Err(ShmError::PeerDied));
    report("shared client poll_server", woke - t0);
    // Завершающий исход помечает клиента отключённым: наблюдения больше нет.
    assert!(!client.is_connected());
    assert_eq!(client.is_peer_alive(), None);
}

/// Контроль: штатное отключение клиента -- никакого `PeerDied`.
#[test]
fn shared_graceful_disconnect_is_not_peer_death() {
    let name = unique_name("SH_G");
    let mut server = SharedServer::start(&name).unwrap();
    let mut peer = Peer::spawn("shared_client_graceful", &name);
    server
        .wait_for_client(Some(Duration::from_secs(20)))
        .unwrap();
    let events = server.events().unwrap();
    // Штатный Drop клиента сигналит DISCONNECT до выхода процесса.
    assert!(
        events
            .disconnect
            .wait(Some(Duration::from_secs(10)))
            .unwrap()
    );
    assert!(peer.wait_exit(Duration::from_secs(10)).success());
}

// ─── Auto ───────────────────────────────────────────────────────────────────

#[test]
fn auto_server_sees_client_death() {
    let name = unique_name("AU_S");
    let rec = Arc::new(Recorder::default());
    let server = AutoServer::start(&name, rec.clone(), AutoOptions::default()).unwrap();
    let mut peer = Peer::spawn("auto_client", &name);
    wait_until("child connect", Duration::from_secs(20), || {
        rec.connected.load(Ordering::Acquire)
    });
    wait_until("peer watched", Duration::from_secs(5), || {
        server.is_peer_alive() == Some(true)
    });
    assert_eq!(server.peer_pid(), Some(peer.pid()));
    let t0 = peer.kill();
    let (reason, at) = rec.wait_reason(Duration::from_secs(10));
    assert_eq!(reason, DisconnectReason::PeerDied);
    report("auto server on_disconnect_reason", at - t0);
    assert_eq!(server.is_peer_alive(), Some(false));
    assert_eq!(server.peer_pid(), Some(peer.pid()));
}

#[test]
fn auto_client_sees_server_death() {
    let name = unique_name("AU_C");
    let mut peer = Peer::spawn("auto_server", &name);
    let rec = Arc::new(Recorder::default());
    let client = AutoClient::connect(
        &name,
        rec.clone(),
        AutoOptions {
            // Дочерний сервер может подняться не сразу -- переподключаемся часто.
            reconnect_delay: Duration::from_millis(50),
            ..AutoOptions::default()
        },
    )
    .unwrap();
    wait_until("connect", Duration::from_secs(20), || {
        client.is_peer_alive() == Some(true)
    });
    let t0 = peer.kill();
    let (reason, at) = rec.wait_reason(Duration::from_secs(10));
    assert_eq!(reason, DisconnectReason::PeerDied);
    report("auto client on_disconnect_reason", at - t0);
    drop(client);
}

#[test]
fn auto_graceful_disconnect_reports_graceful() {
    let name = unique_name("AU_G");
    let rec = Arc::new(Recorder::default());
    let _server = AutoServer::start(&name, rec.clone(), AutoOptions::default()).unwrap();
    let mut peer = Peer::spawn("auto_client_graceful", &name);
    let (reason, _) = rec.wait_reason(Duration::from_secs(20));
    assert_eq!(reason, DisconnectReason::Graceful);
    assert!(peer.wait_exit(Duration::from_secs(10)).success());
}

// ─── Dispatch ───────────────────────────────────────────────────────────────

/// Viewer (DispatchServer) видит смерть продюсера (DispatchClient).
#[test]
fn dispatch_server_sees_client_death() {
    let name = unique_name("DS_S");
    let rec = Arc::new(Recorder::default());
    let server = DispatchServer::start(&name, rec.clone(), DispatchOptions::default()).unwrap();
    thread::sleep(Duration::from_millis(100));
    let mut peer = Peer::spawn("dispatch_client", &name);
    wait_until("child registered", Duration::from_secs(20), || {
        rec.connected.load(Ordering::Acquire)
    });
    let id = rec.client_id.load(Ordering::Acquire);
    wait_until("client watched", Duration::from_secs(5), || {
        server.is_client_alive(id) == Some(true)
    });
    let t0 = peer.kill();
    let (reason, at) = rec.wait_reason(Duration::from_secs(10));
    assert_eq!(reason, DisconnectReason::PeerDied);
    report("dispatch server on_client_disconnect_reason", at - t0);
    server.stop();
}

/// Продюсер (DispatchClient, как prof-shm) видит смерть viewer-а.
#[test]
fn dispatch_client_sees_server_death() {
    let name = unique_name("DS_C");
    let mut peer = Peer::spawn("dispatch_server", &name);
    let rec = Arc::new(Recorder::default());
    let mut client = None;
    wait_until("child lobby", Duration::from_secs(20), || {
        client = DispatchClient::connect(
            &name,
            ClientRegistration::new(std::process::id(), 1, "peer-death-parent"),
            rec.clone(),
            DispatchClientOptions {
                lobby_timeout: Duration::from_millis(300),
                ..DispatchClientOptions::default()
            },
        )
        .ok();
        client.is_some()
    });
    let client = client.unwrap();
    wait_until("channel watched", Duration::from_secs(10), || {
        client.is_peer_alive() == Some(true)
    });
    assert_eq!(client.server_pid(), Some(peer.pid()));
    let t0 = peer.kill();
    let (reason, at) = rec.wait_reason(Duration::from_secs(10));
    assert_eq!(reason, DisconnectReason::PeerDied);
    report("dispatch client on_disconnect_reason", at - t0);
    assert_eq!(client.disconnect_reason(), Some(DisconnectReason::PeerDied));
    assert_eq!(client.is_peer_alive(), Some(false));
    assert!(!client.is_connected());
    assert_eq!(client.try_send(b"after death"), Err(ShmError::NotReady));
}

/// Task 18: клиент ЛОББИ (регистрация ещё не получила ответа) видит
/// смерть процесса сервера -- `DispatchClient::connect` возвращает
/// `PeerDied` сразу (ожидание `[S2C_DATA, S2C_DISCONNECT, процесс сервера]`,
/// INTEROP §d.2 шаг 7), а не через `response_timeout` (здесь 30 с).
#[test]
fn dispatch_lobby_client_sees_server_death() {
    let name = unique_name("DS_LOBBY");
    let marker = Beacon::open(&admit_marker(&name)).unwrap();
    let mut peer = Peer::spawn("dispatch_lobby_stall", &name);
    wait_until("child lobby", Duration::from_secs(20), || {
        DispatchClient::lobby_exists(&name)
    });
    let client = thread::spawn(move || {
        let result = DispatchClient::connect(
            &name,
            ClientRegistration::new(std::process::id(), 1, "lobby-orphan"),
            Arc::new(Noop),
            DispatchClientOptions {
                response_timeout: Duration::from_secs(30),
                ..DispatchClientOptions::default()
            },
        );
        (result.map(drop), Instant::now())
    });
    // Сервер принял регистрацию и завис в `admit`: клиент ждёт ответа.
    assert!(
        marker.wait(Some(Duration::from_secs(20))).unwrap(),
        "admit не начат"
    );
    let t0 = peer.kill();
    let (result, woke) = client.join().unwrap();
    assert_eq!(result, Err(ShmError::PeerDied));
    report("dispatch lobby client connect", woke - t0);
}

#[test]
fn dispatch_graceful_disconnect_reports_graceful() {
    let name = unique_name("DS_G");
    let rec = Arc::new(Recorder::default());
    let server = DispatchServer::start(&name, rec.clone(), DispatchOptions::default()).unwrap();
    thread::sleep(Duration::from_millis(100));
    let mut peer = Peer::spawn("dispatch_client_graceful", &name);
    let (reason, _) = rec.wait_reason(Duration::from_secs(20));
    assert_eq!(reason, DisconnectReason::Graceful);
    assert!(peer.wait_exit(Duration::from_secs(10)).success());
    server.stop();
}

/// Локальный `disconnect_client` сообщается как `Local`.
#[test]
fn dispatch_local_disconnect_reports_local() {
    let name = unique_name("DS_L");
    let rec = Arc::new(Recorder::default());
    let server = DispatchServer::start(&name, rec.clone(), DispatchOptions::default()).unwrap();
    thread::sleep(Duration::from_millis(100));
    let _peer = Peer::spawn("dispatch_client", &name);
    wait_until("child registered", Duration::from_secs(20), || {
        rec.connected.load(Ordering::Acquire)
    });
    server
        .disconnect_client(rec.client_id.load(Ordering::Acquire))
        .unwrap();
    let (reason, _) = rec.wait_reason(Duration::from_secs(5));
    assert_eq!(reason, DisconnectReason::Local);
    server.stop();
}

// ─── Multi (0.9: смерть по handle процесса, без опроса) ─────────────────────

/// Сервер Multi видит смерть клиента по удерживаемому handle процесса
/// (сразу, а не на тике 50 мс / проверке PID раз в 3 с), слот освобождается.
#[test]
fn multi_server_sees_client_death() {
    let name = unique_name("MU_S");
    let rec = Arc::new(Recorder::default());
    let server = MultiServer::start(
        &name,
        rec.clone(),
        MultiOptions {
            max_clients: 2,
            ..MultiOptions::default()
        },
    )
    .unwrap();
    let mut peer = Peer::spawn("multi_client", &name);
    wait_until("child connect", Duration::from_secs(20), || {
        rec.connected.load(Ordering::Acquire)
    });
    let id = rec.client_id.load(Ordering::Acquire);
    assert_eq!(server.is_client_alive(id), Some(true));
    let t0 = peer.kill();
    let (reason, at) = rec.wait_reason(Duration::from_secs(10));
    assert_eq!(reason, DisconnectReason::PeerDied);
    report("multi server on_client_disconnect_reason", at - t0);
    assert_eq!(
        server.client_count(),
        0,
        "слот мёртвого клиента не освобождён"
    );
    server.stop();
}

/// Клиент Multi видит смерть сервера по handle процесса.
#[test]
fn multi_client_sees_server_death() {
    let name = unique_name("MU_C");
    let mut peer = Peer::spawn("multi_server", &name);
    let rec = Arc::new(Recorder::default());
    let client = MultiClient::connect(
        &name,
        rec.clone(),
        MultiClientOptions {
            // Дочерний сервер может подняться не сразу.
            retry_delay: Duration::from_millis(50),
            ..MultiClientOptions::default()
        },
    )
    .unwrap();
    wait_until("connect", Duration::from_secs(20), || {
        rec.connected.load(Ordering::Acquire)
    });
    let t0 = peer.kill();
    let (reason, at) = rec.wait_reason(Duration::from_secs(10));
    assert_eq!(reason, DisconnectReason::PeerDied);
    report("multi client on_disconnect_reason", at - t0);
    drop(client);
}

/// Журнал событий Multi-сервера по слотам (тест спина).
#[derive(Default)]
struct SlotLog {
    connected: Mutex<Vec<u32>>,
    disconnected: Mutex<Vec<(u32, DisconnectReason)>>,
}

impl SlotLog {
    #[expect(
        clippy::unwrap_used,
        reason = "тестовый помощник: паника -- провал теста"
    )]
    fn connected(&self) -> Vec<u32> {
        self.connected.lock().unwrap().clone()
    }

    #[expect(
        clippy::unwrap_used,
        reason = "тестовый помощник: паника -- провал теста"
    )]
    fn reason(&self, id: u32) -> Option<DisconnectReason> {
        self.disconnected
            .lock()
            .unwrap()
            .iter()
            .find(|(slot, _)| *slot == id)
            .map(|&(_, reason)| reason)
    }
}

impl MultiHandler for SlotLog {
    #[expect(
        clippy::unwrap_used,
        reason = "тестовый помощник: паника -- провал теста"
    )]
    fn on_client_connect(&self, client_id: u32) {
        self.connected.lock().unwrap().push(client_id);
    }
    fn on_client_disconnect(&self, _client_id: u32) {}
    #[expect(
        clippy::unwrap_used,
        reason = "тестовый помощник: паника -- провал теста"
    )]
    fn on_client_disconnect_reason(&self, client_id: u32, reason: DisconnectReason) {
        self.disconnected.lock().unwrap().push((client_id, reason));
    }
    fn on_message(&self, _client_id: u32, _data: &[u8]) {}
}

/// Task 12, fix round 1: спин worker-а группы Multi с бюджетом много длиннее
/// паузы болтливого клиента (сообщение раз в 1..16 мс -- к концу каждого
/// спина кольцо непусто) не заслоняет события остальных слотов группы:
/// новый клиент подключается, штатный уход и смерть процесса другого
/// клиента замечаются. До исправления удачный спин пропускал `wait_any`, и
/// `CONNECT_REQ`/смерть процесса не наблюдались, пока шёл поток.
///
/// Бюджет конечен: в простое (до потока) события замечаются только к концу
/// бюджета, `u32::MAX` растянул бы это на минуты (документированная цена).
#[test]
fn multi_spin_does_not_starve_group_events() {
    // ~0,3..0,5 с спина на этой машине (`pause` ~140 тактов), десятки мс на
    // CPU с коротким `pause` -- всё равно длиннее паузы потока.
    const SPIN: SpinPolicy = SpinPolicy::new(10_000_000);
    let name = unique_name("MU_SPIN");
    let log = Arc::new(SlotLog::default());
    let server = MultiServer::start(
        &name,
        log.clone(),
        MultiOptions {
            max_clients: 4,
            spin: SPIN,
            ..MultiOptions::default()
        },
    )
    .unwrap();

    // Клиент в дочернем процессе: его смерть сервер видит только событием
    // процесса в наборе ожидания.
    let mut peer = Peer::spawn("multi_client", &name);
    wait_until(
        "дочерний клиент",
        Duration::from_secs(20),
        || log.connected().len() == 1,
    );
    let child_id = log.connected()[0];

    // Болтливый клиент: пишет непрерывно, пока идёт тест.
    let chatty = Arc::new(
        MultiClient::connect(&name, Arc::new(Noop), MultiClientOptions::default()).unwrap(),
    );
    wait_until(
        "болтливый клиент",
        Duration::from_secs(5),
        || log.connected().len() == 2,
    );
    let streaming = Arc::new(AtomicBool::new(true));
    let streamer = {
        let (chatty, streaming) = (chatty.clone(), streaming.clone());
        thread::spawn(move || {
            while streaming.load(Ordering::Acquire) {
                let _ = chatty.send(b"tick");
                thread::sleep(Duration::from_millis(1));
            }
        })
    };
    thread::sleep(Duration::from_millis(200));

    // Новый клиент подключается при идущем потоке (CONNECT_REQ замечен).
    let late = MultiClient::connect(&name, Arc::new(Noop), MultiClientOptions::default()).unwrap();
    wait_until(
        "подключение при потоке",
        Duration::from_secs(3),
        || log.connected().len() == 3 && late.is_connected(),
    );
    let late_id = late.slot_id();

    // Штатный уход (DISCONNECT) замечен.
    drop(late);
    wait_until(
        "штатный уход при потоке",
        Duration::from_secs(3),
        || log.reason(late_id) == Some(DisconnectReason::Graceful),
    );

    // Смерть процесса другого клиента замечена.
    let _ = peer.kill();
    wait_until(
        "смерть клиента при потоке",
        Duration::from_secs(3),
        || log.reason(child_id) == Some(DisconnectReason::PeerDied),
    );

    streaming.store(false, Ordering::Release);
    streamer.join().unwrap();
    assert_eq!(
        log.reason(chatty.slot_id()),
        None,
        "болтливый клиент на месте"
    );
    drop(chatty);
    server.stop();
}

// ─── Ревизия 3: claim канала ────────────────────────────────────────────────

/// §7.1 п. 11: канал занят клиентом-процессом (свой клиент -- сразу
/// `AlreadyConnected`); клиент убит в сессии -- сервер освобождает его claim
/// при разборе смерти, и новый клиент подключается сразу, а не через
/// `RESERVE_TIMEOUT` (10 с).
#[test]
fn peer_death_releases_claim_immediately() {
    let name = unique_name("CLAIM");
    let rec = Arc::new(Recorder::default());
    let server = AutoServer::start(&name, rec.clone(), AutoOptions::default()).unwrap();
    let mut peer = Peer::spawn("auto_client", &name);
    wait_until("child connect", Duration::from_secs(20), || {
        rec.connected.load(Ordering::Acquire)
    });
    assert_eq!(
        SharedClient::connect(&name, Duration::from_secs(1)).unwrap_err(),
        ShmError::AlreadyConnected
    );
    let t0 = peer.kill();
    let (reason, _) = rec.wait_reason(Duration::from_secs(10));
    assert_eq!(reason, DisconnectReason::PeerDied);
    let mut client = None;
    wait_until(
        "подключение после смерти",
        Duration::from_secs(3),
        || match SharedClient::connect(&name, Duration::from_millis(500)) {
            Ok(c) => {
                client = Some(c);
                true
            }
            Err(ShmError::AlreadyConnected) => false,
            Err(err) => panic!("подключение: {err:?}"),
        },
    );
    let waited = t0.elapsed();
    println!("peer_death_releases_claim_immediately: канал свободен через {waited:?}");
    assert!(waited < Duration::from_secs(5), "{waited:?}");
    drop(client);
    drop(server);
}

// ─── Task 7: claim подтверждённо умершего после отключения сервером ─────────

#[cfg_attr(
    target_arch = "x86",
    link(
        name = "kernel32",
        kind = "raw-dylib",
        import_name_type = "undecorated"
    )
)]
#[cfg_attr(not(target_arch = "x86"), link(name = "kernel32", kind = "raw-dylib"))]
unsafe extern "system" {
    fn OpenFileMappingW(desired_access: u32, inherit: i32, name: *const u16) -> *mut c_void;
    fn MapViewOfFile(
        mapping: *mut c_void,
        desired_access: u32,
        offset_high: u32,
        offset_low: u32,
        bytes: usize,
    ) -> *mut c_void;
    fn UnmapViewOfFile(base: *const c_void) -> i32;
    fn OpenEventW(desired_access: u32, inherit: i32, name: *const u16) -> *mut c_void;
    fn SetEvent(event: *mut c_void) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

/// `FILE_MAP_ALL_ACCESS`.
const FILE_MAP_ALL_ACCESS: u32 = 0x000F_001F;
/// `EVENT_MODIFY_STATE`.
const EVENT_MODIFY_STATE: u32 = 0x0002;
/// Ёмкость колец тестовых каналов (маленькая -- смещения известны).
const CAP: usize = 4096;
/// Смещение `ControlBlock::slot_claim` (INTEROP §c.1).
const SLOT_CLAIM: usize = 20;
/// Заголовок кольца клиент -> сервер: `ControlBlock` 64 + `LayoutBlock` 64 +
/// заголовок S2C 64 + кольцо S2C; `write_pos` -- первое поле.
const C2S_WRITE_POS: usize = 64 + 64 + 64 + CAP;
/// Размер отображения: до конца заголовка C2S.
const MAPPED: usize = C2S_WRITE_POS + 64;

const fn small_layout() -> ChannelLayout {
    ChannelLayout::symmetric(RingConfig::new(CAP))
}

/// Широкая строка с нулём на конце.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// Сегмент канала `name` «глазами пира» -- второе отображение секции.
struct Segment {
    handle: NonNull<c_void>,
    base: NonNull<u8>,
}

impl Segment {
    fn open(name: &str) -> Self {
        let path = wide(&format!("Local\\{name}"));
        // SAFETY: `path` -- строка с нулём на конце, живёт весь вызов.
        let handle = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, path.as_ptr()) };
        let handle = NonNull::new(handle).unwrap_or_else(|| panic!("OpenFileMappingW({name})"));
        // SAFETY: `handle` -- только что открытая секция; `MAPPED` не больше
        // секции (раскладку `small_layout` задал сервер этого теста).
        let base = unsafe { MapViewOfFile(handle.as_ptr(), FILE_MAP_ALL_ACCESS, 0, 0, MAPPED) };
        let base = NonNull::new(base.cast::<u8>()).unwrap_or_else(|| panic!("MapViewOfFile"));
        Self { handle, base }
    }

    fn word(&self, offset: usize) -> &AtomicU32 {
        assert!(offset.is_multiple_of(4) && offset + 4 <= MAPPED);
        // SAFETY: offset + 4 <= MAPPED -- адрес внутри отображения.
        let ptr = unsafe { self.base.as_ptr().add(offset) };
        // SAFETY: адрес выровнен на 4 (база -- на гранулу выделения), память
        // отображена, пока жив `self`; стороны канала обращаются к словам
        // заголовков атомарно.
        unsafe { AtomicU32::from_ptr(ptr.cast()) }
    }

    fn claim(&self) -> u32 {
        self.word(SLOT_CLAIM).load(Ordering::Acquire)
    }

    /// Порча кольца клиент -> сервер: `write_pos` не кратен 8.
    fn corrupt_c2s(&self) {
        let write_pos = self.word(C2S_WRITE_POS);
        let value = write_pos.load(Ordering::Acquire).wrapping_add(3);
        write_pos.store(value, Ordering::Release);
    }
}

impl Drop for Segment {
    fn drop(&mut self) {
        // SAFETY: база получена от `MapViewOfFile` и ещё не размаплена.
        let _ = unsafe { UnmapViewOfFile(self.base.as_ptr().cast_const().cast()) };
        // SAFETY: handle открыт `OpenFileMappingW` и закрывается один раз.
        let _ = unsafe { CloseHandle(self.handle.as_ptr()) };
    }
}

/// Смещения `ControlBlock` (INTEROP §c.1): `slot_owner_pid`,
/// `owner_start_lo`/`owner_start_hi`.
const SLOT_OWNER_PID: usize = 24;
const OWNER_START_LO: usize = 52;
const OWNER_START_HI: usize = 56;

#[cfg_attr(
    target_arch = "x86",
    link(
        name = "kernel32",
        kind = "raw-dylib",
        import_name_type = "undecorated"
    )
)]
#[cfg_attr(not(target_arch = "x86"), link(name = "kernel32", kind = "raw-dylib"))]
unsafe extern "system" {
    fn GetCurrentProcess() -> *mut c_void;
    fn GetProcessTimes(
        process: *mut c_void,
        creation: *mut u64,
        exit: *mut u64,
        kernel: *mut u64,
        user: *mut u64,
    ) -> i32;
}

/// Время создания своего процесса (FILETIME, как `CreateTime` NT).
fn own_start_time() -> u64 {
    let (mut creation, mut exit, mut kernel, mut user) = (0u64, 0u64, 0u64, 0u64);
    // SAFETY: без аргументов; возвращает псевдо-handle своего процесса.
    let process = unsafe { GetCurrentProcess() };
    // SAFETY: `process` -- псевдо-handle своего процесса; выходные параметры
    // -- локальные `u64` (FILETIME -- 8 байт, выравнивание `u64` не меньше
    // нужного).
    let ok = unsafe {
        GetProcessTimes(
            process,
            &raw mut creation,
            &raw mut exit,
            &raw mut kernel,
            &raw mut user,
        )
    };
    assert_ne!(ok, 0, "GetProcessTimes");
    creation
}

/// Дочерний «держатель»: claim канала `name` своим токеном, затем время
/// своего процесса и PID владельца (шаги C1, C2), без заявки.
fn claim_as_holder(name: &str) {
    let segment = Segment::open(name);
    let start = own_start_time();
    let token = 0xC1A1_0000 | (std::process::id() & 0xFFFF);
    assert!(
        segment
            .word(SLOT_CLAIM)
            .compare_exchange(0, token, Ordering::AcqRel, Ordering::Acquire)
            .is_ok(),
        "канал уже занят"
    );
    let [l0, l1, l2, l3, h0, h1, h2, h3] = start.to_le_bytes();
    segment
        .word(OWNER_START_LO)
        .store(u32::from_le_bytes([l0, l1, l2, l3]), Ordering::Release);
    segment
        .word(OWNER_START_HI)
        .store(u32::from_le_bytes([h0, h1, h2, h3]), Ordering::Release);
    segment
        .word(SLOT_OWNER_PID)
        .store(std::process::id(), Ordering::Release);
}

/// Взвести `C2S_DATA` канала `name`, как это сделал бы писатель-пир.
fn signal_c2s_data(name: &str) {
    let path = wide(&format!("Local\\{name}_C2S_DATA"));
    // SAFETY: `path` -- строка с нулём на конце, живёт весь вызов.
    let event = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, path.as_ptr()) };
    assert!(!event.is_null(), "OpenEventW({name}_C2S_DATA)");
    // SAFETY: `event` -- только что открытое событие.
    assert_ne!(unsafe { SetEvent(event) }, 0, "SetEvent");
    // SAFETY: handle открыт выше и закрывается один раз.
    let _ = unsafe { CloseHandle(event) };
}

/// Журнал Multi-сервера: подключения, причины отключения и ошибки по слотам.
#[derive(Default)]
struct MultiJournal {
    connected: Mutex<Vec<u32>>,
    reasons: Mutex<Vec<(u32, DisconnectReason)>>,
    errors: Mutex<Vec<(Option<u32>, ShmError)>>,
}

impl MultiJournal {
    #[expect(
        clippy::unwrap_used,
        reason = "тестовый помощник: паника -- провал теста"
    )]
    fn reason(&self, slot: u32) -> Option<DisconnectReason> {
        self.reasons
            .lock()
            .unwrap()
            .iter()
            .find(|(id, _)| *id == slot)
            .map(|&(_, reason)| reason)
    }

    #[expect(
        clippy::unwrap_used,
        reason = "тестовый помощник: паника -- провал теста"
    )]
    fn errors(&self) -> Vec<(Option<u32>, ShmError)> {
        self.errors.lock().unwrap().clone()
    }

    #[expect(
        clippy::unwrap_used,
        reason = "тестовый помощник: паника -- провал теста"
    )]
    fn connected(&self) -> Vec<u32> {
        self.connected.lock().unwrap().clone()
    }
}

impl MultiHandler for MultiJournal {
    #[expect(
        clippy::unwrap_used,
        reason = "тестовый помощник: паника -- провал теста"
    )]
    fn on_client_connect(&self, client_id: u32) {
        self.connected.lock().unwrap().push(client_id);
    }
    fn on_client_disconnect(&self, _client_id: u32) {}
    #[expect(
        clippy::unwrap_used,
        reason = "тестовый помощник: паника -- провал теста"
    )]
    fn on_client_disconnect_reason(&self, client_id: u32, reason: DisconnectReason) {
        self.reasons.lock().unwrap().push((client_id, reason));
    }
    fn on_message(&self, _client_id: u32, _data: &[u8]) {}
    #[expect(
        clippy::unwrap_used,
        reason = "тестовый помощник: паника -- провал теста"
    )]
    fn on_error(&self, client_id: Option<u32>, err: ShmError) {
        self.errors.lock().unwrap().push((client_id, err));
    }
}

/// Граница освобождения claim-а умершего: против `RESERVE_TIMEOUT` (10 с).
const RECLAIM_BOUND: Duration = Duration::from_secs(1);

#[expect(
    clippy::unwrap_used,
    reason = "тестовый помощник: паника -- провал теста"
)]
fn multi_server(name: &str, journal: &Arc<MultiJournal>) -> Arc<MultiServer> {
    MultiServer::start(
        name,
        journal.clone(),
        MultiOptions {
            max_clients: 2,
            layout: small_layout(),
            ..MultiOptions::default()
        },
    )
    .unwrap()
}

/// Убить пира, дождаться выхода и проверить: claim слота 0 снят не позже
/// `RECLAIM_BOUND` после убийства, новый `MultiClient` занимает слот 0 не
/// позже `RECLAIM_BOUND` после этого.
#[expect(
    clippy::unwrap_used,
    reason = "тестовый помощник: паника -- провал теста"
)]
fn kill_and_expect_slot0_reclaimed(peer: &mut Peer, segment: &Segment, name: &str) {
    let t0 = peer.kill();
    peer.wait_exit(Duration::from_secs(10));
    wait_until("claim умершего снят", RECLAIM_BOUND, || {
        segment.claim() == 0
    });
    println!("claim умершего снят через {:?}", t0.elapsed());
    let client = MultiClient::connect(name, Arc::new(Noop), MultiClientOptions::default()).unwrap();
    wait_until(
        "новый клиент в слоте 0",
        RECLAIM_BOUND,
        || client.is_connected() && client.slot_id() == 0,
    );
    drop(client);
}

/// Путь A (решение C): смерть клиента замечена первой, а при дочитывании
/// его кольца -- испорченный кадр. Причина -- `PeerDied`, порча --
/// отдельный `on_error(Corrupted)`, claim умершего снят сразу, без
/// `RESERVE_TIMEOUT`.
#[test]
fn multi_dead_client_with_corrupted_frame_frees_slot_at_once() {
    let name = unique_name("MU_CDEAD");
    let journal = Arc::new(MultiJournal::default());
    let server = multi_server(&name, &journal);
    let mut peer = Peer::spawn("multi_client", &name);
    wait_until(
        "дочерний клиент",
        Duration::from_secs(20),
        || journal.connected() == [0],
    );
    let segment = Segment::open(&format!("{name}_0"));
    assert_ne!(segment.claim(), 0);
    // Без `DATA`: порчу заметит только дочитывание после смерти.
    segment.corrupt_c2s();

    kill_and_expect_slot0_reclaimed(&mut peer, &segment, &name);
    assert_eq!(journal.reason(0), Some(DisconnectReason::PeerDied));
    assert_eq!(journal.errors(), [(Some(0), ShmError::Corrupted)]);
    server.stop();
}

/// Путь B: порча замечена у живого клиента (разрыв с `Error`; клиент --
/// `SharedClient` без ожиданий -- `DISCONNECT` не замечает), затем клиент
/// умирает. Сервер держит handle его процесса («задержавшийся владелец») и
/// снимает claim сразу по смерти.
#[test]
fn multi_client_dying_after_error_disconnect_frees_slot_at_once() {
    let name = unique_name("MU_EDEAD");
    let journal = Arc::new(MultiJournal::default());
    let server = multi_server(&name, &journal);
    let slot = format!("{name}_0");
    let mut peer = Peer::spawn("shared_client", &slot);
    wait_until(
        "дочерний клиент",
        Duration::from_secs(20),
        || journal.connected() == [0],
    );
    let segment = Segment::open(&slot);
    segment.corrupt_c2s();
    signal_c2s_data(&slot);
    wait_until("разрыв с Error", Duration::from_secs(5), || {
        journal.reason(0) == Some(DisconnectReason::Error)
    });
    // Живой владелец claim-а его держит (решение O: никогда два писателя).
    thread::sleep(Duration::from_millis(100));
    assert_ne!(segment.claim(), 0, "claim живого клиента снят");

    kill_and_expect_slot0_reclaimed(&mut peer, &segment, &name);
    assert_eq!(journal.errors(), [(Some(0), ShmError::Corrupted)]);
    server.stop();
}

/// Single-client: сервер Auto разорвал сессию по порче (`Error`), клиент
/// этого не заметил и умер. Первый толчок нового клиента освобождает claim
/// умершего сразу -- `AutoClient` подключается за доли секунды, а не через
/// `RESERVE_TIMEOUT`.
#[test]
fn auto_server_client_dying_after_error_frees_channel_at_once() {
    let name = unique_name("AU_EDEAD");
    let rec = Arc::new(Recorder::default());
    let server = AutoServer::start(
        &name,
        rec.clone(),
        AutoOptions {
            layout: small_layout(),
            ..AutoOptions::default()
        },
    )
    .unwrap();
    let mut peer = Peer::spawn("shared_client", &name);
    wait_until(
        "дочерний клиент",
        Duration::from_secs(20),
        || rec.connected.load(Ordering::Acquire),
    );
    let segment = Segment::open(&name);
    segment.corrupt_c2s();
    signal_c2s_data(&name);
    let (reason, _) = rec.wait_reason(Duration::from_secs(5));
    assert_eq!(reason, DisconnectReason::Error);
    thread::sleep(Duration::from_millis(100));
    assert_ne!(segment.claim(), 0, "claim живого клиента снят");

    let t0 = peer.kill();
    peer.wait_exit(Duration::from_secs(10));
    let next = Arc::new(Recorder::default());
    let client = AutoClient::connect(
        &name,
        next.clone(),
        AutoOptions {
            reconnect_delay: Duration::from_millis(50),
            ..AutoOptions::default()
        },
    )
    .unwrap();
    wait_until("новый клиент", RECLAIM_BOUND, || {
        next.connected.load(Ordering::Acquire)
    });
    println!("канал свободен через {:?}", t0.elapsed());
    drop(client);
    drop(server);
}

/// Обработчик `AutoClient` для Task 11: подключения (моменты), причины
/// отключения и ошибки.
#[derive(Default)]
struct PresenceLog {
    connects: Mutex<Vec<Instant>>,
    reasons: Mutex<Vec<DisconnectReason>>,
    errors: Mutex<Vec<ShmError>>,
}

#[expect(
    clippy::unwrap_used,
    reason = "тестовый помощник: паника -- провал теста"
)]
impl AutoHandler for PresenceLog {
    fn on_connect(&self) {
        self.connects.lock().unwrap().push(Instant::now());
    }
    fn on_disconnect_reason(&self, reason: DisconnectReason) {
        self.reasons.lock().unwrap().push(reason);
    }
    fn on_error(&self, err: ShmError) {
        self.errors.lock().unwrap().push(err);
    }
}

/// Task 11 (R19, Task 10 N2): сервер -- дочерний процесс; его убивают.
/// Клиент (`reconnect_delay` = 10 с) получает `PeerDied`, гасит маяк
/// упавшего сервера (`clear_if_stale`) и спит на нём: за секунду -- ни
/// одной ошибки (до Task 11 -- попытка и `on_error` раз в
/// `reconnect_delay`), а новый сервер принимается сразу (≤ 300 мс), а не
/// через `reconnect_delay`. Счётчик пробуждений -- `cfg(test)`, снаружи не
/// виден: отсутствие опроса здесь доказывают ошибки и задержка, а
/// пробуждения -- модульные тесты `auto::presence_tests`.
#[test]
fn auto_client_reconnects_after_server_crash_without_polling() {
    let name = unique_name("AU_PRES");
    let mut peer = Peer::spawn("auto_server", &name);
    let log = Arc::new(PresenceLog::default());
    let client = AutoClient::connect(
        &name,
        log.clone(),
        AutoOptions {
            reconnect_delay: Duration::from_secs(10),
            ..AutoOptions::default()
        },
    )
    .unwrap();
    // Дочерний сервер мог подняться позже клиента -- клиент ждёт его маяк.
    wait_until(
        "подключение к дочернему",
        Duration::from_secs(20),
        || client.is_peer_alive() == Some(true),
    );
    let beacon = Beacon::open(&name).unwrap();
    assert!(beacon.is_raised().unwrap(), "сервер не взвёл маяк");

    peer.kill();
    wait_until("PeerDied", Duration::from_secs(10), || {
        log.reasons
            .lock()
            .unwrap()
            .contains(&DisconnectReason::PeerDied)
    });
    wait_until(
        "маяк упавшего сброшен",
        Duration::from_secs(2),
        || !beacon.is_raised().unwrap(),
    );
    let errors_before = log.errors.lock().unwrap().len();
    thread::sleep(Duration::from_secs(1));
    let errors = log.errors.lock().unwrap().clone();
    assert!(
        errors.len() == errors_before,
        "ошибки без сервера: {errors:?}"
    );

    let started = Instant::now();
    let server = AutoServer::start(&name, Arc::new(Noop), AutoOptions::default()).unwrap();
    wait_until(
        "подключение к новому серверу",
        Duration::from_secs(15),
        || log.connects.lock().unwrap().len() == 2,
    );
    let took = log.connects.lock().unwrap()[1] - started;
    assert!(
        took <= Duration::from_millis(300),
        "подключение через {took:?}"
    );
    drop(client);
    drop(server);
}

// ─── Task 12: уведомление «claim освобождён» ────────────────────────────────

/// Момент подключения `AutoClient`.
#[derive(Default)]
struct ConnectedAt(Mutex<Option<Instant>>);

impl ConnectedAt {
    #[expect(
        clippy::unwrap_used,
        reason = "тестовый помощник: паника -- провал теста"
    )]
    fn get(&self) -> Option<Instant> {
        *self.0.lock().unwrap()
    }
}

impl AutoHandler for ConnectedAt {
    #[expect(
        clippy::unwrap_used,
        reason = "тестовый помощник: паника -- провал теста"
    )]
    fn on_connect(&self) {
        self.0.lock().unwrap().get_or_insert_with(Instant::now);
    }
}

/// Держатель канала -- дочерний процесс в сессии `AutoServer`, `AutoClient`
/// B ждёт освобождения. Держатель убит -- сервер освобождает claim умершего
/// (`mark_disconnected`) и уведомляет (R20): B подключён не позже 500 мс
/// (до Task 12 -- по отступу `BusyBackoff`, до 2 с).
#[test]
fn busy_waiter_wakes_on_holder_crash_in_session() {
    let name = unique_name("FREE_CRASH");
    let rec = Arc::new(Recorder::default());
    let server = AutoServer::start(&name, rec.clone(), AutoOptions::default()).unwrap();
    let mut peer = Peer::spawn("shared_client", &name);
    wait_until(
        "держатель подключён",
        Duration::from_secs(20),
        || rec.connected.load(Ordering::Acquire),
    );
    let b_log = Arc::new(ConnectedAt::default());
    let b = AutoClient::connect(&name, b_log.clone(), AutoOptions::default()).unwrap();
    // B успел получить отказ и заснуть; отступ (до Task 12) дошёл до 2 с.
    thread::sleep(Duration::from_millis(2500));
    assert!(b_log.get().is_none(), "B подключился к занятому каналу");
    let t0 = peer.kill();
    wait_until("B подключён", Duration::from_secs(5), || {
        b_log.get().is_some()
    });
    let took = b_log.get().unwrap() - t0;
    report("busy waiter after holder crash", took);
    assert!(took <= BOUND, "{took:?}");
    drop(b);
    drop(server);
}

/// Держатель (дочерний процесс) захватил claim и вышел, не выставив
/// заявки. Ждущий B просыпается по процессу держателя и толкает сервер,
/// сервер подтверждает смерть и освобождает claim сразу (R18, быстрый
/// путь): B подключён не позже 500 мс (до Task 12 -- через
/// `RESERVE_TIMEOUT` 10 с повторными толчками).
#[test]
fn holder_dead_before_hello_reclaimed_on_first_nudge() {
    let name = unique_name("FREE_NOHELLO");
    let server = AutoServer::start(&name, Arc::new(Noop), AutoOptions::default()).unwrap();
    let mut peer = Peer::spawn("claim_and_exit", &name);
    let segment = Segment::open(&name);
    wait_until("claim держателя", Duration::from_secs(20), || {
        segment.claim() != 0
    });
    let b_log = Arc::new(ConnectedAt::default());
    let b = AutoClient::connect(&name, b_log.clone(), AutoOptions::default()).unwrap();
    let status = peer.wait_exit(Duration::from_secs(20));
    let t0 = Instant::now();
    assert!(status.success(), "{status:?}");
    wait_until("B подключён", Duration::from_secs(15), || {
        b_log.get().is_some()
    });
    let took = b_log.get().unwrap().saturating_duration_since(t0);
    report("busy waiter after holder died before hello", took);
    assert!(took <= BOUND, "{took:?}");
    drop(b);
    drop(segment);
    drop(server);
}
