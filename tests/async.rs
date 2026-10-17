//! Async API single-client каналов (0.10, Task 16): `accept_async`/
//! `connect_async`/`recv_async`/`send_async` поверх реактора IOCP.
//!
//! Исполнитель -- свой минимальный `block_on` (`Wake` + `thread::park`):
//! крейт исполнитель-агностичен, tokio не нужен даже в dev-зависимостях.
//! Future досыпают только до пробуждения реактора (`thread::park` без
//! таймаута, никаких переопросов по времени); общий предел ожидания держит
//! сторожевой поток (`Watchdog`), который роняет процесс тестов, -- так
//! потерянное пробуждение проваливает тест, а не маскируется переопросом.

use std::future::Future;
use std::pin::{Pin, pin};
use std::process::{Child, Command};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use xshm::{ChannelConfig, ChannelLayout, RingConfig, SharedClient, SharedServer, ShmError};

/// Предел любого ожидания в тесте: шум хоста высокий, поэтому с запасом.
const DEADLINE: Duration = Duration::from_secs(10);
const CHILD_ENV: &str = "XSHM_ASYNC_CHILD";

#[expect(
    clippy::unwrap_used,
    reason = "тестовый помощник: паника -- провал теста"
)]
fn unique_name(tag: &str) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("XSHM_ASYNC_{tag}_{}_{}", std::process::id(), ts % 1_000_000)
}

// ─── исполнитель ────────────────────────────────────────────────────────────

/// `Waker`, который будит поток-владельца и считает пробуждения.
struct Unpark {
    thread: Thread,
    wakes: AtomicUsize,
}

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
        self.thread.unpark();
    }
}

fn unpark_waker() -> (Arc<Unpark>, Waker) {
    let state = Arc::new(Unpark {
        thread: thread::current(),
        wakes: AtomicUsize::new(0),
    });
    let waker = Waker::from(Arc::clone(&state));
    (state, waker)
}

/// Сторож общего предела: не снят за `DEADLINE` -- сообщение и `abort`
/// процесса тестов (паника в чужом потоке тест не провалила бы). `Drop`
/// (в т.ч. при панике теста) снимает сторожа.
struct Watchdog {
    _cancel: mpsc::Sender<()>,
}

impl Watchdog {
    fn arm(what: &'static str) -> Self {
        let (cancel, cancelled) = mpsc::channel::<()>();
        thread::spawn(move || {
            if cancelled.recv_timeout(DEADLINE) == Err(RecvTimeoutError::Timeout) {
                eprintln!("watchdog: {what} не готов за {DEADLINE:?} -- потерянное пробуждение?");
                std::process::abort();
            }
        });
        Self { _cancel: cancel }
    }
}

/// Опрашивать future с `waker` до готовности, засыпая только до его
/// пробуждения (`thread::park` без таймаута; ложный возврат `park`
/// безвреден). Вернуть результат и число опросов.
fn drive<F: Future>(mut future: Pin<&mut F>, waker: &Waker) -> (F::Output, usize) {
    let _watchdog = Watchdog::arm("future");
    let mut cx = Context::from_waker(waker);
    let mut polls = 0;
    loop {
        polls += 1;
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return (value, polls);
        }
        thread::park();
    }
}

/// Выполнить future до готовности на текущем потоке; вернуть результат и
/// число вызовов `poll`.
fn block_on_counted<F: Future>(future: F) -> (F::Output, usize) {
    let (_state, waker) = unpark_waker();
    drive(pin!(future), &waker)
}

fn block_on<F: Future>(future: F) -> F::Output {
    block_on_counted(future).0
}

/// Один `poll` с данным `Waker`.
fn poll_once<F: Future>(future: Pin<&mut F>, waker: &Waker) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(waker))
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    while !cond() {
        assert!(Instant::now() < deadline, "таймаут: {what}");
        thread::sleep(Duration::from_millis(2));
    }
}

/// Сервер `name` и подключённый к нему синхронный клиент.
#[expect(
    clippy::unwrap_used,
    reason = "тестовый помощник: паника -- провал теста"
)]
fn connected_pair(name: &str, config: &ChannelConfig) -> (SharedServer, SharedClient) {
    let mut server = SharedServer::start_with(name, config).unwrap();
    let client_name = name.to_owned();
    let client = thread::spawn(move || SharedClient::connect(&client_name, DEADLINE).unwrap());
    block_on(server.accept_async()).unwrap();
    (server, client.join().unwrap())
}

const fn assert_send<T: Send>(_: &T) {}

// ─── дочерний процесс ───────────────────────────────────────────────────────

/// Точка входа дочернего процесса (`режим|имя`). Без переменной окружения
/// -- no-op. Режимы: `client` -- подключиться и жить; `client_send` --
/// подключиться, отправить `last-words` и жить; `server` -- поднять сервер и
/// жить, не принимая клиентов.
#[test]
fn child_entry() {
    let Ok(spec) = std::env::var(CHILD_ENV) else {
        return;
    };
    let (mode, name) = spec.split_once('|').unwrap();
    let mut _server = None;
    let mut _client = None;
    match mode {
        "client" => _client = Some(SharedClient::connect(name, DEADLINE).unwrap()),
        "client_send" => {
            let client = SharedClient::connect(name, DEADLINE).unwrap();
            client.try_send_to_server(b"last-words").unwrap();
            _client = Some(client);
        }
        "server" => _server = Some(SharedServer::start(name).unwrap()),
        other => panic!("неизвестный режим {other}"),
    }
    // Живёт, пока его не убьют (потолок -- если родитель упал сам).
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
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
}

impl Drop for Peer {
    fn drop(&mut self) {
        drop(self.0.kill()); // best-effort: процесс мог уже выйти
        drop(self.0.wait());
    }
}

// ─── тесты ──────────────────────────────────────────────────────────────────

/// Полный обмен через async API с обеих сторон: рукопожатие
/// `accept_async`/`connect_async`, затем запрос-ответ.
#[test]
fn async_roundtrip() {
    let client_name = unique_name("RT");
    let mut server = SharedServer::start(&client_name).unwrap();
    let client = thread::spawn(move || {
        let mut client = block_on(SharedClient::connect_async(&client_name)).unwrap();
        assert!(client.is_connected());
        block_on(client.send_async(b"ping")).unwrap();
        let mut buf = Vec::new();
        let len = block_on(client.recv_async(&mut buf)).unwrap();
        assert_eq!(&buf[..len], b"pong");
    });
    block_on(server.accept_async()).unwrap();
    assert!(server.is_connected());
    let mut buf = Vec::new();
    let len = block_on(server.recv_async(&mut buf)).unwrap();
    assert_eq!(&buf[..len], b"ping");
    block_on(server.send_async(b"pong")).unwrap();
    client.join().unwrap();
}

/// `send_async` в заполненное кольцо ждёт места (без перезаписи) и
/// завершается, когда клиент читает.
#[test]
fn async_send_waits_for_space() {
    let name = unique_name("SPACE");
    let config = ChannelConfig::default().layout(ChannelLayout::symmetric(RingConfig::new(4096)));
    let (mut server, mut client) = connected_pair(&name, &config);
    let payload = [7u8; 1000];
    let mut sent = 0usize;
    while server.try_send_to_client(&payload).is_ok() {
        sent += 1;
    }
    assert!(sent > 0);

    let (state, waker) = unpark_waker();
    let mut future = pin!(server.send_async(&payload));
    assert!(poll_once(future.as_mut(), &waker).is_pending(), "места нет");

    let reader = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        let mut buf = Vec::new();
        let mut read = 0usize;
        let deadline = Instant::now() + DEADLINE;
        while read < sent + 1 {
            assert!(Instant::now() < deadline, "клиент не дочитал");
            match client.receive_from_server(&mut buf) {
                Ok(len) => {
                    assert_eq!(&buf[..len], &payload[..]);
                    read += 1;
                }
                Err(ShmError::QueueEmpty) => {
                    let _ = client.poll_server(Some(Duration::from_millis(50)));
                }
                Err(err) => panic!("клиент: {err:?}"),
            }
        }
        client
    });
    // Только пробуждение реактора: потерянный SPACE -- провал по сторожу.
    drive(future.as_mut(), &waker).0.unwrap();
    assert!(state.wakes.load(Ordering::SeqCst) >= 1, "разбудил SPACE");
    drop(reader.join().unwrap());
}

/// Клиент отправил прощальное сообщение и ушёл: `recv_async` сначала отдаёт
/// сообщение, потом `Err(NotConnected)`.
#[test]
fn async_recv_sees_disconnect() {
    let name = unique_name("DISC");
    let (mut server, client) = connected_pair(&name, &ChannelConfig::default());
    let leaver = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        client.try_send_to_server(b"bye").unwrap();
        drop(client); // IDLE -> DISCONNECT
    });
    let mut buf = Vec::new();
    let len = block_on(server.recv_async(&mut buf)).unwrap();
    assert_eq!(&buf[..len], b"bye");
    leaver.join().unwrap();
    assert_eq!(
        block_on(server.recv_async(&mut buf)),
        Err(ShmError::NotConnected)
    );
    assert!(!server.is_connected(), "сервер отметил отключение");
}

/// Процесс клиента убит: `recv_async` -- `Err(PeerDied)`.
#[test]
fn async_recv_sees_peer_death() {
    let name = unique_name("DEATH");
    let mut server = SharedServer::start(&name).unwrap();
    let mut peer = Peer::spawn("client", &name);
    block_on(server.accept_async()).unwrap();
    assert_eq!(server.peer_pid(), Some(peer.0.id()));

    let (_state, waker) = unpark_waker();
    let mut buf = Vec::new();
    let mut future = pin!(server.recv_async(&mut buf));
    assert!(poll_once(future.as_mut(), &waker).is_pending());
    peer.0.kill().unwrap();
    // Только пробуждение реактора по процессу пира.
    assert_eq!(drive(future.as_mut(), &waker).0, Err(ShmError::PeerDied));
}

/// Данные, записанные пиром до смерти, `recv_async` отдаёт раньше
/// `Err(PeerDied)` («сначала дочитать кольцо»).
#[test]
fn async_recv_drains_before_peer_death() {
    let name = unique_name("DRAIN");
    let mut server = SharedServer::start(&name).unwrap();
    let mut peer = Peer::spawn("client_send", &name);
    block_on(server.accept_async()).unwrap();
    // Сообщение в кольце (по позициям; сигнал DATA не важен).
    wait_until("клиент записал сообщение", || {
        server.poll_client(Some(Duration::ZERO)).unwrap_or(false)
    });
    peer.0.kill().unwrap();
    peer.0.wait().unwrap();
    let mut buf = Vec::new();
    let len = block_on(server.recv_async(&mut buf)).unwrap();
    assert_eq!(&buf[..len], b"last-words");
    assert_eq!(
        block_on(server.recv_async(&mut buf)),
        Err(ShmError::PeerDied)
    );
    assert!(!server.is_connected());
}

/// Заполнить кольцо сервер -> клиент сообщениями `payload`.
fn fill_to_client(server: &SharedServer, payload: &[u8]) {
    let mut sent = 0usize;
    while server.try_send_to_client(payload).is_ok() {
        sent += 1;
    }
    assert!(sent > 0);
}

/// Ожидающий места `send_async`: клиент ушёл штатно -- `Err(NotConnected)`.
#[test]
fn async_send_sees_disconnect() {
    let name = unique_name("SENDDISC");
    let config = ChannelConfig::default().layout(ChannelLayout::symmetric(RingConfig::new(4096)));
    let (mut server, client) = connected_pair(&name, &config);
    let payload = [3u8; 1000];
    fill_to_client(&server, &payload);
    let (_state, waker) = unpark_waker();
    let mut future = pin!(server.send_async(&payload));
    assert!(poll_once(future.as_mut(), &waker).is_pending(), "места нет");
    let leaver = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        drop(client); // IDLE -> DISCONNECT, кольцо не читано
    });
    assert_eq!(
        drive(future.as_mut(), &waker).0,
        Err(ShmError::NotConnected)
    );
    leaver.join().unwrap();
}

/// Ожидающий места `send_async`: процесс клиента убит -- `Err(PeerDied)`.
#[test]
fn async_send_sees_peer_death() {
    let name = unique_name("SENDDEATH");
    let config = ChannelConfig::default().layout(ChannelLayout::symmetric(RingConfig::new(4096)));
    let mut server = SharedServer::start_with(&name, &config).unwrap();
    let mut peer = Peer::spawn("client", &name);
    block_on(server.accept_async()).unwrap();
    let payload = [5u8; 1000];
    fill_to_client(&server, &payload);
    let (_state, waker) = unpark_waker();
    let mut future = pin!(server.send_async(&payload));
    assert!(poll_once(future.as_mut(), &waker).is_pending(), "места нет");
    peer.0.kill().unwrap();
    assert_eq!(drive(future.as_mut(), &waker).0, Err(ShmError::PeerDied));
}

/// Сервер умер посреди рукопожатия (заявка выставлена, ответа нет):
/// `connect_async` -- `Err(PeerDied)`, а не вечное ожидание.
#[test]
fn async_connect_sees_server_death() {
    let name = unique_name("CONNDEATH");
    let mut peer = Peer::spawn("server", &name);
    let (_state, waker) = unpark_waker();
    // Сервер поднимается в дочернем процессе: пока его нет -- открытие
    // секции/событий отказывает, пробуем снова.
    let started = Instant::now();
    let mut future = loop {
        let mut future = Box::pin(SharedClient::connect_async(&name));
        match poll_once(future.as_mut(), &waker) {
            Poll::Pending => break future,
            Poll::Ready(Ok(_)) => panic!("сервер не принимает клиентов"),
            Poll::Ready(Err(_)) => {
                assert!(started.elapsed() < DEADLINE, "сервер не поднялся");
                thread::sleep(Duration::from_millis(20));
            }
        }
    };
    peer.0.kill().unwrap();
    assert_eq!(
        drive(future.as_mut(), &waker).0.err(),
        Some(ShmError::PeerDied)
    );
}

/// Смена `Waker` между опросами (регистрации живут между опросами, им
/// только обновляется `Waker`): срабатывание будит последний, а не первый.
#[test]
fn rewake_uses_latest_waker() {
    let name = unique_name("REWAKE");
    let (mut server, client) = connected_pair(&name, &ChannelConfig::default());
    let (state_a, waker_a) = unpark_waker();
    let (state_b, waker_b) = unpark_waker();
    let mut buf = Vec::new();
    let len = {
        let mut future = pin!(server.recv_async(&mut buf));
        assert!(poll_once(future.as_mut(), &waker_a).is_pending());
        assert!(poll_once(future.as_mut(), &waker_b).is_pending());
        client.try_send_to_server(b"to-b").unwrap();
        wait_until("DATA разбудил waker B", || {
            state_b.wakes.load(Ordering::SeqCst) >= 1
        });
        assert_eq!(
            state_a.wakes.load(Ordering::SeqCst),
            0,
            "разбужен прежний waker"
        );
        drive(future.as_mut(), &waker_b).0.unwrap()
    };
    assert_eq!(&buf[..len], b"to-b");
}

/// Брошенный future снимает свои регистрации: данные, пришедшие после
/// `Drop`, его `Waker` не будят; следующий `recv_async` их получает.
#[test]
fn dropping_future_cancels_registration() {
    let name = unique_name("DROP");
    let (mut server, client) = connected_pair(&name, &ChannelConfig::default());
    let (state, waker) = unpark_waker();
    let mut buf = Vec::new();
    {
        let mut future = pin!(server.recv_async(&mut buf));
        assert!(poll_once(future.as_mut(), &waker).is_pending());
    }
    client.try_send_to_server(b"after-drop").unwrap();
    thread::sleep(Duration::from_millis(200));
    assert_eq!(
        state.wakes.load(Ordering::SeqCst),
        0,
        "регистрация брошенного future осталась в реакторе"
    );
    let len = block_on(server.recv_async(&mut buf)).unwrap();
    assert_eq!(&buf[..len], b"after-drop");
}

/// Регрессия потерянного сигнала: future потребил `DATA` (регистрация
/// сработала, `Waker` разбужен) и брошен, не будучи опрошен. Сигнал
/// возвращается событию, данные получает следующий приём.
#[test]
fn dropped_future_keeps_consumed_data_signal() {
    let name = unique_name("KEEPDATA");
    let (mut server, client) = connected_pair(&name, &ChannelConfig::default());
    let (state, waker) = unpark_waker();
    let mut buf = Vec::new();
    {
        let mut future = pin!(server.recv_async(&mut buf));
        assert!(poll_once(future.as_mut(), &waker).is_pending());
        client.try_send_to_server(b"consumed").unwrap();
        wait_until("DATA разбудил future", || {
            state.wakes.load(Ordering::SeqCst) >= 1
        });
    }
    let data = &server.events().unwrap().c2s.data;
    assert!(
        data.wait(Some(Duration::ZERO)).unwrap(),
        "потреблённый сигнал DATA не возвращён событию"
    );
    let len = block_on(server.recv_async(&mut buf)).unwrap();
    assert_eq!(&buf[..len], b"consumed");
}

/// То же для `DISCONNECT`: future потребил сигнал ухода клиента и брошен --
/// следующий `recv_async` всё равно узнаёт об отключении (процесс клиента
/// жив -- это этот же процесс, -- так что без возврата сигнала он ждал бы
/// вечно).
#[test]
fn dropped_future_keeps_consumed_disconnect_signal() {
    let name = unique_name("KEEPDISC");
    let (mut server, client) = connected_pair(&name, &ChannelConfig::default());
    let (state, waker) = unpark_waker();
    let mut buf = Vec::new();
    {
        let mut future = pin!(server.recv_async(&mut buf));
        assert!(poll_once(future.as_mut(), &waker).is_pending());
        drop(client); // IDLE -> DISCONNECT
        wait_until("DISCONNECT разбудил future", || {
            state.wakes.load(Ordering::SeqCst) >= 1
        });
    }
    assert_eq!(
        block_on(server.recv_async(&mut buf)),
        Err(ShmError::NotConnected)
    );
}

/// Простой без занятого ожидания: ожидающий `recv_async` за 200 мс
/// тишины не будится ни разу; всего опросов -- 2 (первый и по данным).
#[test]
fn pending_recv_does_not_busy_wait() {
    let name = unique_name("IDLE");
    let (mut server, client) = connected_pair(&name, &ChannelConfig::default());
    let sender = thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        client.try_send_to_server(b"late").unwrap();
        client
    });
    let mut buf = Vec::new();
    let (result, polls) = block_on_counted(server.recv_async(&mut buf));
    let len = result.unwrap();
    assert_eq!(&buf[..len], b"late");
    assert!(
        polls <= 2,
        "занятое ожидание: {polls} опросов за 200 мс простоя"
    );
    drop(sender.join().unwrap());
}

/// `Drop` при живом реакторе (Review Focus 2): ожидающие future обеих
/// сторон брошены, затем уничтожены сами стороны (их handle закрыты).
/// Старые `Waker` больше не будятся, общий реактор продолжает обслуживать
/// новый канал.
#[test]
fn endpoints_dropped_after_futures_reactor_keeps_serving() {
    let (state, waker) = unpark_waker();
    {
        let name = unique_name("DROPALL");
        let (mut server, mut client) = connected_pair(&name, &ChannelConfig::default());
        let mut server_buf = Vec::new();
        let mut client_buf = Vec::new();
        {
            let mut recv_server = pin!(server.recv_async(&mut server_buf));
            assert!(poll_once(recv_server.as_mut(), &waker).is_pending());
            let mut recv_client = pin!(client.recv_async(&mut client_buf));
            assert!(poll_once(recv_client.as_mut(), &waker).is_pending());
        }
        drop(client);
        drop(server);
    }
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        state.wakes.load(Ordering::SeqCst),
        0,
        "Waker брошенного future разбужен"
    );

    let name = unique_name("DROPALL2");
    let (mut server, mut client) = connected_pair(&name, &ChannelConfig::default());
    block_on(client.send_async(b"alive")).unwrap();
    let mut buf = Vec::new();
    let len = block_on(server.recv_async(&mut buf)).unwrap();
    assert_eq!(&buf[..len], b"alive");
}

/// Брошенный `connect_async` с выставленной заявкой откатывает её: сервер
/// не принимает «клиента», которого уже нет.
#[test]
fn dropped_connect_withdraws_hello() {
    let name = unique_name("CONNDROP");
    let mut server = SharedServer::start(&name).unwrap();
    let (_state, waker) = unpark_waker();
    {
        let mut future = pin!(SharedClient::connect_async(&name));
        assert!(poll_once(future.as_mut(), &waker).is_pending());
    }
    // Заявки нет: ожидание клиента кончается таймаутом, а не рукопожатием.
    assert_eq!(
        server.wait_for_client(Some(Duration::from_millis(200))),
        Err(ShmError::Timeout)
    );
    assert!(!server.is_connected());
}

/// Брошенный `connect_async`, который сервер уже принял (CAS сервера
/// выиграл), уходит штатно: сервер видит отключение, а не вечную сессию.
#[test]
fn dropped_connect_after_accept_disconnects() {
    let name = unique_name("CONNACC");
    let mut server = SharedServer::start(&name).unwrap();
    let (_state, waker) = unpark_waker();
    {
        let mut future = pin!(SharedClient::connect_async(&name));
        assert!(poll_once(future.as_mut(), &waker).is_pending());
        // Сервер фиксирует рукопожатие, а клиентский future так и не
        // опрошен снова.
        server
            .wait_for_client(Some(Duration::from_secs(5)))
            .unwrap();
    }
    let mut buf = Vec::new();
    assert_eq!(
        block_on(server.recv_async(&mut buf)),
        Err(ShmError::NotConnected)
    );
}

/// Брошенный `accept_async` возвращает потреблённый `CONNECT_REQ`:
/// следующее ожидание клиента его принимает.
#[test]
fn dropped_accept_keeps_connect_request() {
    let client_name = unique_name("ACCDROP");
    let mut server = SharedServer::start(&client_name).unwrap();
    let (state, waker) = unpark_waker();
    let client = {
        let mut future = pin!(server.accept_async());
        assert!(poll_once(future.as_mut(), &waker).is_pending());
        let client = thread::spawn(move || SharedClient::connect(&client_name, DEADLINE));
        wait_until("CONNECT_REQ разбудил future", || {
            state.wakes.load(Ordering::SeqCst) >= 1
        });
        client
    };
    block_on(server.accept_async()).unwrap();
    assert!(client.join().unwrap().unwrap().is_connected());
}

/// Каждый future async API -- `Send` (годится для `tokio::spawn`).
#[test]
fn futures_are_send() {
    let name = unique_name("SEND");
    let (mut server, mut client) = connected_pair(&name, &ChannelConfig::default());
    let mut buf = Vec::new();
    assert_send(&SharedClient::connect_async(&name));
    assert_send(&server.accept_async());
    assert_send(&server.recv_async(&mut buf));
    assert_send(&server.send_async(b"x1"));
    assert_send(&client.recv_async(&mut buf));
    assert_send(&client.send_async(b"x1"));
}

/// Anonymous-канал без событий: async API -- явная ошибка конфигурации.
#[test]
fn anonymous_server_rejects_async() {
    let mut server = SharedServer::start_anonymous().unwrap();
    assert!(matches!(
        block_on(server.accept_async()),
        Err(ShmError::InvalidConfig(_))
    ));
}

// ─── поток пустых CONNECT_REQ (Task 10) ─────────────────────────────────────

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
    fn OpenEventW(desired_access: u32, inherit: i32, name: *const u16) -> *mut std::ffi::c_void;
    fn SetEvent(event: *mut std::ffi::c_void) -> i32;
    fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
}

/// Закрепление (Task 10, решение B): поток пустых `CONNECT_REQ` (чужой
/// процесс -- kernel32) не мешает `accept_async` принять настоящего
/// клиента -- ≤ 500 мс.
#[test]
fn accept_async_under_flood_accepts() {
    use std::sync::atomic::AtomicBool;
    let name = unique_name("ACCFLOOD");
    let mut server = SharedServer::start(&name).unwrap();
    let wide: Vec<u16> = format!("Local\\{name}_C2S_CONNECT_REQ")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    // SAFETY: `wide` -- строка с нулём на конце, живёт весь вызов;
    // `EVENT_MODIFY_STATE` (2) -- право на `SetEvent`.
    let raw = unsafe { OpenEventW(2, 0, wide.as_ptr()) };
    assert!(!raw.is_null(), "OpenEventW не удался");
    let handle = raw.addr();
    let run = Arc::new(AtomicBool::new(true));
    let attacker = {
        let run = run.clone();
        thread::spawn(move || {
            let event = std::ptr::without_provenance_mut::<std::ffi::c_void>(handle);
            while run.load(Ordering::Acquire) {
                // SAFETY: handle события открыт выше и закрывается только
                // после join этого потока.
                let _ = unsafe { SetEvent(event) };
            }
        })
    };
    thread::sleep(Duration::from_millis(50));
    let started = Instant::now();
    let client_name = name;
    let client = thread::spawn(move || SharedClient::connect(&client_name, DEADLINE));
    let accepted = block_on(server.accept_async());
    let elapsed = started.elapsed();
    let client = client.join().unwrap();
    run.store(false, Ordering::Release);
    attacker.join().unwrap();
    // SAFETY: handle открыт `OpenEventW`, поток-атакующий завершён.
    let _ = unsafe { CloseHandle(raw) };
    accepted.unwrap();
    assert!(client.unwrap().is_connected() && server.is_connected());
    assert!(elapsed <= Duration::from_millis(500), "{elapsed:?}");
}

// ─── T11c: устаревший DISCONNECT -- не уход ────────────────────────────────

/// Взвести `DISCONNECT` канала `name` через второй handle (сторона канала
/// заимствована future-ом).
#[expect(
    clippy::unwrap_used,
    reason = "тестовый помощник: паника -- провал теста"
)]
fn stale_disconnect(name: &str) {
    xshm::events::SharedEvents::open(name)
        .unwrap()
        .disconnect
        .set()
        .unwrap();
}

/// T11c (решение AD): ожидающий `recv_async` сервера, разбуженный
/// устаревшим `DISCONNECT` (клиент в сессии), поглощает его и остаётся
/// `Pending`; данные клиента потом доставляются, а настоящий уход клиента
/// -- `Err(NotConnected)`.
#[test]
fn async_server_recv_absorbs_stale_disconnect() {
    let name = unique_name("STALEDISC_S");
    let (mut server, client) = connected_pair(&name, &ChannelConfig::default());
    let (state, waker) = unpark_waker();
    let mut buf = Vec::new();
    {
        let mut future = pin!(server.recv_async(&mut buf));
        assert!(poll_once(future.as_mut(), &waker).is_pending());
        stale_disconnect(&name);
        wait_until("DISCONNECT разбудил future", || {
            state.wakes.load(Ordering::SeqCst) >= 1
        });
        assert!(
            poll_once(future.as_mut(), &waker).is_pending(),
            "устаревший DISCONNECT завершил приём"
        );
        client.try_send_to_server(b"after-stale").unwrap();
        let (result, _) = drive(future.as_mut(), &waker);
        assert_eq!(result, Ok(11));
    }
    assert_eq!(&buf[..11], b"after-stale");
    drop(client); // IDLE -> DISCONNECT
    assert_eq!(
        block_on(server.recv_async(&mut buf)),
        Err(ShmError::NotConnected)
    );
}

/// То же у клиента (`server_state == SERVER_READY`, `generation`
/// прежний): `recv_async` клиента поглощает устаревший сигнал; уход
/// сервера (`disconnect`) -- `Err(NotConnected)`.
#[test]
fn async_client_recv_absorbs_stale_disconnect() {
    let name = unique_name("STALEDISC_C");
    let (mut server, mut client) = connected_pair(&name, &ChannelConfig::default());
    let (state, waker) = unpark_waker();
    let mut buf = Vec::new();
    {
        let mut future = pin!(client.recv_async(&mut buf));
        assert!(poll_once(future.as_mut(), &waker).is_pending());
        stale_disconnect(&name);
        wait_until("DISCONNECT разбудил future", || {
            state.wakes.load(Ordering::SeqCst) >= 1
        });
        assert!(
            poll_once(future.as_mut(), &waker).is_pending(),
            "устаревший DISCONNECT завершил приём"
        );
        server.try_send_to_client(b"after-stale").unwrap();
        let (result, _) = drive(future.as_mut(), &waker);
        assert_eq!(result, Ok(11));
    }
    server.disconnect().unwrap();
    assert_eq!(
        block_on(client.recv_async(&mut buf)),
        Err(ShmError::NotConnected)
    );
}

/// Ожидающий места `send_async` поглощает устаревший `DISCONNECT` и
/// завершается, когда читатель освобождает место.
#[test]
fn async_send_absorbs_stale_disconnect() {
    let name = unique_name("STALEDISC_SEND");
    let config = ChannelConfig::default().layout(ChannelLayout::symmetric(RingConfig::new(4096)));
    let (mut server, client) = connected_pair(&name, &config);
    let payload = [3u8; 1000];
    fill_to_client(&server, &payload);
    let (state, waker) = unpark_waker();
    let mut future = pin!(server.send_async(&payload));
    assert!(poll_once(future.as_mut(), &waker).is_pending(), "места нет");
    stale_disconnect(&name);
    wait_until("DISCONNECT разбудил future", || {
        state.wakes.load(Ordering::SeqCst) >= 1
    });
    assert!(
        poll_once(future.as_mut(), &waker).is_pending(),
        "устаревший DISCONNECT завершил отправку"
    );
    let mut buf = Vec::new();
    client.receive_from_server(&mut buf).unwrap();
    assert_eq!(drive(future.as_mut(), &waker).0, Ok(()));
}
