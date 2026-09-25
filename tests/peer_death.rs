//! Детекция смерти процесса пира (протокол 0.8+: обмен PID в handshake +
//! удерживаемый handle процесса).
//!
//! Пир -- дочерний процесс (этот же тестовый бинарь, перезапущенный с
//! переменной окружения). Родитель убивает его (`TerminateProcess`) и меряет,
//! через сколько его сторона узнаёт о `PeerDied`. Контроль: штатный выход
//! пира должен давать `Graceful`, а не `PeerDied`.

use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use xshm::{
    AutoClient, AutoHandler, AutoOptions, AutoServer, ClientRegistration, DisconnectReason,
    DispatchClient, DispatchClientHandler, DispatchClientOptions, DispatchHandler, DispatchOptions,
    DispatchServer, MultiClient, MultiClientHandler, MultiClientOptions, MultiHandler,
    MultiOptions, MultiServer, SharedClient, SharedServer, ShmError,
};

const CHILD_ENV: &str = "XSHM_PEER_DEATH_CHILD";
/// Граница задержки детекции, которую проверяем.
const BOUND: Duration = Duration::from_millis(500);

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
                ClientRegistration {
                    pid: std::process::id(),
                    revision: 1,
                    name: "peer-death-child".into(),
                },
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
        other => panic!("unknown child mode {other}"),
    }
}

/// Дочерний процесс, который гарантированно убивается при выходе из теста.
struct Peer(Child);

impl Peer {
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
    fn kill(&mut self) -> Instant {
        let t0 = Instant::now();
        self.0.kill().expect("kill child");
        t0
    }

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
        let _ = self.0.kill();
        let _ = self.0.wait();
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
    fn record(&self, reason: DisconnectReason) {
        let mut slot = self.reason.lock().unwrap();
        if slot.is_none() {
            *slot = Some((reason, Instant::now()));
        }
    }

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
    let server = Arc::new(server);
    let waiter = {
        let server = server.clone();
        thread::spawn(move || {
            let r = server.wait_for_space(big.len(), Some(Duration::from_secs(20)));
            (r, Instant::now())
        })
    };
    thread::sleep(Duration::from_millis(100)); // писатель уже спит в ожидании
    let t0 = peer.kill();
    let (result, woke) = waiter.join().unwrap();
    assert_eq!(result, Err(ShmError::PeerDied));
    report("shared server wait_for_space", woke - t0);
    assert_eq!(server.is_peer_alive(), Some(false));
    let t = Instant::now();
    assert_eq!(
        server.poll_client(Some(Duration::from_secs(10))),
        Err(ShmError::PeerDied)
    );
    assert!(t.elapsed() < BOUND);
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
    let client = Arc::new(client.unwrap());
    assert_eq!(client.peer_pid(), Some(peer.pid()));
    let poller = {
        let client = client.clone();
        thread::spawn(move || {
            let r = client.poll_server(Some(Duration::from_secs(20)));
            (r, Instant::now())
        })
    };
    thread::sleep(Duration::from_millis(100));
    let t0 = peer.kill();
    let (result, woke) = poller.join().unwrap();
    assert_eq!(result, Err(ShmError::PeerDied));
    report("shared client poll_server", woke - t0);
    assert_eq!(client.is_peer_alive(), Some(false));
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
            ClientRegistration {
                pid: std::process::id(),
                revision: 1,
                name: "peer-death-parent".into(),
            },
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
