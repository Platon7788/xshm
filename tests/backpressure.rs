//! Запись без перезаписи (`try_send*`/`free_space`/`wait_for_space`):
//! межпроцессные и межпоточные сценарии «писатель быстрее читателя».
//!
//! Межпроцессный тест перезапускает этот же тестовый бинарь дочерним
//! процессом (`current_exe` + фильтр по имени теста + переменная окружения),
//! без внешних зависимостей.

use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use xshm::{
    ClientRegistration, DispatchClient, DispatchClientHandler, DispatchClientOptions,
    DispatchHandler, DispatchOptions, DispatchServer, FreeSpace, MAX_MESSAGE_SIZE, RING_CAPACITY,
    SharedClient, SharedServer, ShmError,
};

const CHILD_ENV: &str = "XSHM_BACKPRESSURE_CHILD";
const CHILD_TOTAL_ENV: &str = "XSHM_BACKPRESSURE_TOTAL";

fn unique_name(tag: &str) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("XSHM_BP_{tag}_{}_{}", std::process::id(), ts % 1_000_000)
}

/// Детерминированная длина сообщения по номеру: от 12 байт до ~60 КиБ,
/// с периодическими крайними значениями.
const fn len_for(seq: u32) -> usize {
    match seq % 97 {
        0 => MAX_MESSAGE_SIZE,
        1 => 12,
        _ => 12 + (seq.wrapping_mul(2_654_435_761) % 40_000) as usize,
    }
}

/// Payload, по которому видно потерю/перестановку (seq) и порчу (тело).
fn make_payload(seq: u32, len: usize) -> Vec<u8> {
    let mut v: Vec<u8> = (0..len)
        .map(|i| (i as u32).wrapping_mul(131).wrapping_add(seq) as u8)
        .collect();
    let s = seq.to_le_bytes();
    v[..4].copy_from_slice(&s);
    v[len - 4..].copy_from_slice(&s);
    v
}

fn check_payload(expected_seq: u32, msg: &[u8]) {
    assert!(msg.len() >= 12, "слишком короткое сообщение: {}", msg.len());
    let seq = u32::from_le_bytes(msg[..4].try_into().unwrap());
    assert_eq!(seq, expected_seq, "потеря или перестановка сообщения");
    assert_eq!(msg.len(), len_for(seq), "длина сообщения {seq}");
    assert!(
        msg == make_payload(seq, msg.len()).as_slice(),
        "содержимое сообщения {seq} повреждено"
    );
}

// ─── межпроцессный сценарий ─────────────────────────────────────────────────

/// Точка входа ДОЧЕРНЕГО процесса: медленный читатель. В обычном прогоне
/// (без переменной окружения) ничего не делает.
#[test]
fn child_slow_consumer_entry() {
    let Ok(name) = std::env::var(CHILD_ENV) else {
        return;
    };
    let total: u32 = std::env::var(CHILD_TOTAL_ENV).unwrap().parse().unwrap();
    let client = SharedClient::connect(&name, Duration::from_secs(10)).expect("child connect");
    let mut buf = Vec::new();
    let mut expected = 0u32;
    let deadline = Instant::now() + Duration::from_secs(60);
    while expected < total {
        assert!(
            Instant::now() < deadline,
            "child: таймаут на {expected}/{total}"
        );
        if !client
            .poll_server(Some(Duration::from_millis(100)))
            .expect("child poll")
        {
            continue;
        }
        loop {
            match client.receive_from_server(&mut buf) {
                Ok(len) => {
                    check_payload(expected, &buf[..len]);
                    expected += 1;
                    // Читатель намеренно медленнее писателя.
                    if expected.is_multiple_of(16) {
                        thread::sleep(Duration::from_millis(1));
                    }
                }
                Err(ShmError::QueueEmpty) => break,
                Err(err) => panic!("child receive: {err:?}"),
            }
        }
    }
    client
        .send_to_server(format!("OK:{expected}").as_bytes())
        .expect("child ack");
    // Даём родителю прочитать подтверждение до разрыва (Drop сигналит DISCONNECT).
    thread::sleep(Duration::from_millis(300));
}

fn spawn_child(name: &str, total: u32) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "child_slow_consumer_entry",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, name)
        .env(CHILD_TOTAL_ENV, total.to_string())
        .spawn()
        .expect("spawn child process")
}

/// Писатель (этот процесс) быстрее читателя (дочерний процесс):
/// `try_send_to_client` упирается в `QueueFull`, писатель ждёт через
/// `wait_for_space`, ни одно сообщение не теряется и не перезаписывается.
#[test]
fn cross_process_lossless_backpressure() {
    const TOTAL: u32 = 3_000;
    let name = unique_name("XPROC");
    let mut server = SharedServer::start(&name).expect("server start");
    let mut child = spawn_child(&name, TOTAL);

    server
        .wait_for_client(Some(Duration::from_secs(20)))
        .expect("child did not connect");

    let mut fulls = 0u32;
    let mut waited = Duration::ZERO;
    let start = Instant::now();
    for seq in 0..TOTAL {
        let payload = make_payload(seq, len_for(seq));
        loop {
            // Нижняя граница: если free_space() говорит «влезет», try_send обязан пройти.
            let promised = server.free_space().fits(payload.len());
            match server.try_send_to_client(&payload) {
                Ok(outcome) => {
                    assert_eq!(outcome.overwritten, 0);
                    break;
                }
                Err(ShmError::QueueFull) => {
                    assert!(!promised, "free_space() обещал место, а try_send отказал");
                    fulls += 1;
                    let t = Instant::now();
                    let ok = server
                        .wait_for_space(payload.len(), Some(Duration::from_secs(10)))
                        .expect("wait_for_space");
                    waited += t.elapsed();
                    if !ok {
                        let status = child.try_wait().unwrap();
                        panic!("места нет 10 с (seq={seq}), дочерний процесс: {status:?}");
                    }
                }
                Err(err) => panic!("try_send_to_client: {err:?}"),
            }
        }
    }
    let send_time = start.elapsed();

    // Подтверждение от ребёнка: он получил ВСЕ сообщения по порядку.
    let mut buf = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    let ack = loop {
        assert!(Instant::now() < deadline, "нет подтверждения от ребёнка");
        if server
            .poll_client(Some(Duration::from_millis(100)))
            .unwrap()
        {
            let len = server.receive_from_client(&mut buf).unwrap();
            break String::from_utf8_lossy(&buf[..len]).into_owned();
        }
    };
    assert_eq!(ack, format!("OK:{TOTAL}"));

    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("дочерний процесс не завершился");
        }
        thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "дочерний процесс упал: {status:?}");
    assert!(
        fulls > 0,
        "писатель ни разу не упёрся в полное кольцо -- тест ничего не проверил"
    );
    println!(
        "cross-process: {TOTAL} msgs in {send_time:?}, QueueFull {fulls} times, \
         waited {waited:?} in wait_for_space"
    );
}

// ─── внутрипроцессные сценарии ──────────────────────────────────────────────

/// Читатель, освободивший место, будит `wait_for_space` событием SPACE
/// сразу, а не по 50-мс страховочному опросу.
#[test]
fn wait_for_space_is_woken_by_reader_event() {
    // Пробуждение событием занимает доли миллисекунды, а пробуждение по
    // 50-мс срезу опроса при чтении «между срезами» -- ~25 мс. Под нагрузкой
    // (полный прогон тестов) единичный замер может «поплыть», поэтому берём
    // лучший из трёх: опрос не даст < 20 мс ни в одной попытке.
    let mut best = Duration::MAX;
    for attempt in 0..3 {
        let name = unique_name(&format!("WAKE{attempt}"));
        let server_thread = thread::spawn({
            let name = name.clone();
            move || {
                let mut server = SharedServer::start(&name).unwrap();
                server
                    .wait_for_client(Some(Duration::from_secs(5)))
                    .unwrap();
                let big = vec![7u8; 60_000];
                while server.try_send_to_client(&big).is_ok() {}
                assert!(!server.free_space().fits(big.len()));
                assert!(server.free_space().max_payload() < big.len());
                let ok = server
                    .wait_for_space(big.len(), Some(Duration::from_secs(5)))
                    .unwrap();
                // Момент пробуждения -- внутри писателя, без учёта join.
                (ok, Instant::now(), server)
            }
        });
        thread::sleep(Duration::from_millis(50));
        let client = SharedClient::connect(&name, Duration::from_secs(5)).unwrap();
        // Читаем между двумя 50-мс срезами ожидания писателя.
        thread::sleep(Duration::from_millis(125));
        let mut buf = Vec::new();
        let read_at = Instant::now();
        client.receive_from_server(&mut buf).unwrap();
        let (ok, woke_at, server) = server_thread.join().unwrap();
        assert!(ok, "место так и не дождались");
        assert!(server.free_space().fits(60_000));
        let latency = woke_at.saturating_duration_since(read_at);
        println!("wait_for_space wake latency after read: {latency:?}");
        best = best.min(latency);
        drop(client);
        if best < Duration::from_millis(20) {
            break;
        }
    }
    assert!(
        best < Duration::from_millis(20),
        "лучшее пробуждение через {best:?} -- похоже на опрос, а не на событие"
    );
}

/// 0.9: штатное отключение читателя будит `wait_for_space` без таймаута
/// (`DISCONNECT` в наборе ожидания) -- `Err(NotConnected)`, а не вечное
/// ожидание места, которое уже никто не освободит. В обе стороны.
#[test]
fn wait_for_space_is_woken_by_graceful_disconnect() {
    let big = vec![7u8; 60_000];

    // Сервер ждёт места, клиент уходит.
    let name = unique_name("DISC_S");
    let mut server = SharedServer::start(&name).unwrap();
    let client_thread = thread::spawn(move || {
        let client = SharedClient::connect(&name, Duration::from_secs(5)).unwrap();
        thread::sleep(Duration::from_millis(300));
        let left_at = Instant::now();
        drop(client); // откат handshake + DISCONNECT
        left_at
    });
    server
        .wait_for_client(Some(Duration::from_secs(5)))
        .unwrap();
    while server.try_send_to_client(&big).is_ok() {}
    assert_eq!(
        server.wait_for_space(big.len(), None),
        Err(ShmError::NotConnected)
    );
    let latency = Instant::now().saturating_duration_since(client_thread.join().unwrap());
    assert!(
        latency < Duration::from_millis(200),
        "сервер проснулся через {latency:?}"
    );

    // Клиент ждёт места, сервер уходит.
    let name = unique_name("DISC_C");
    let server_thread = thread::spawn({
        let name = name.clone();
        move || {
            let mut server = SharedServer::start(&name).unwrap();
            server
                .wait_for_client(Some(Duration::from_secs(5)))
                .unwrap();
            thread::sleep(Duration::from_millis(300));
            let left_at = Instant::now();
            drop(server); // IDLE + DISCONNECT
            left_at
        }
    });
    thread::sleep(Duration::from_millis(50));
    let client = SharedClient::connect(&name, Duration::from_secs(5)).unwrap();
    while client.try_send_to_server(&big).is_ok() {}
    assert_eq!(
        client.wait_for_space(big.len(), None),
        Err(ShmError::NotConnected)
    );
    let latency = Instant::now().saturating_duration_since(server_thread.join().unwrap());
    assert!(
        latency < Duration::from_millis(200),
        "клиент проснулся через {latency:?}"
    );
}

/// `free_space()` вне подключения -- ноль, `try_send` -- NotConnected.
#[test]
fn not_connected_has_no_space() {
    let server = SharedServer::start(&unique_name("NC")).unwrap();
    assert_eq!(server.free_space(), FreeSpace::ZERO);
    assert_eq!(
        server.try_send_to_client(b"hello").err(),
        Some(ShmError::NotConnected)
    );
    assert_eq!(
        server.wait_for_space(10, Some(Duration::ZERO)),
        Err(ShmError::NotConnected)
    );
}

// ─── Dispatch / Auto: lossless поверх очереди worker-а ──────────────────────

struct SlowServer {
    received: Mutex<Vec<u32>>,
    connected: AtomicU32,
    bad: AtomicBool,
    delay: Duration,
}

impl DispatchHandler for SlowServer {
    fn on_client_connect(&self, _client_id: u32, _info: &ClientRegistration) {
        self.connected.fetch_add(1, Ordering::AcqRel);
    }
    fn on_client_disconnect(&self, _client_id: u32) {}
    fn on_message(&self, _client_id: u32, data: &[u8]) {
        let mut received = self.received.lock().unwrap();
        let expected = received.len() as u32;
        let seq = u32::from_le_bytes(data[..4].try_into().unwrap());
        if seq != expected || data != make_payload(seq, len_for(seq)).as_slice() {
            self.bad.store(true, Ordering::Release);
        }
        received.push(seq);
        drop(received);
        thread::sleep(self.delay);
    }
}

#[derive(Default)]
struct RecordingClient {
    received: AtomicU32,
    bad: AtomicBool,
}

impl DispatchClientHandler for RecordingClient {
    fn on_connect(&self, _client_id: u32, _channel_name: &str) {}
    fn on_disconnect(&self) {}
    fn on_message(&self, data: &[u8]) {
        let expected = self.received.load(Ordering::Acquire);
        let seq = u32::from_le_bytes(data[..4].try_into().unwrap());
        if seq != expected || data != make_payload(seq, len_for(seq)).as_slice() {
            self.bad.store(true, Ordering::Release);
        }
        self.received.fetch_add(1, Ordering::AcqRel);
        // Клиент-читатель тоже медленный.
        if expected.is_multiple_of(4) {
            thread::sleep(Duration::from_millis(1));
        }
    }
}

fn wait_until(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !cond() {
        assert!(Instant::now() < deadline, "таймаут: {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Продюсер-клиент (как `prof-shm`) пишет через `DispatchClient::try_send`
/// быстрее, чем сервер-viewer читает: `QueueFull` возвращается вызывающему,
/// а сервер получает все сообщения по порядку и без порчи.
#[test]
fn dispatch_client_try_send_is_lossless() {
    const TOTAL: u32 = 1_200;
    let name = unique_name("DSP");
    let server_handler = Arc::new(SlowServer {
        received: Mutex::new(Vec::new()),
        connected: AtomicU32::new(0),
        bad: AtomicBool::new(false),
        delay: Duration::from_micros(300),
    });
    let server =
        DispatchServer::start(&name, server_handler.clone(), DispatchOptions::default()).unwrap();
    thread::sleep(Duration::from_millis(100));

    let client_handler = Arc::new(RecordingClient::default());
    let client = DispatchClient::connect(
        &name,
        ClientRegistration {
            pid: std::process::id(),
            revision: 1,
            name: "bp-producer".into(),
        },
        client_handler,
        DispatchClientOptions {
            max_send_queue: 16,
            ..DispatchClientOptions::default()
        },
    )
    .unwrap();
    wait_until("server connect", Duration::from_secs(5), || {
        server_handler.connected.load(Ordering::Acquire) == 1
    });
    wait_until("client free space", Duration::from_secs(5), || {
        client.free_space().bytes > 0
    });
    let initial = client.free_space();
    assert!(initial.bytes <= RING_CAPACITY && initial.messages <= 16);

    let mut fulls = 0u64;
    for seq in 0..TOTAL {
        let payload = make_payload(seq, len_for(seq));
        loop {
            let promised = client.free_space().fits(payload.len());
            match client.try_send(&payload) {
                Ok(()) => break,
                Err(ShmError::QueueFull) => {
                    assert!(!promised, "free_space() обещал место, а try_send отказал");
                    fulls += 1;
                    thread::sleep(Duration::from_micros(200));
                }
                Err(err) => panic!("try_send: {err:?}"),
            }
        }
    }
    assert_eq!(
        client.try_send(&[0u8; MAX_MESSAGE_SIZE + 1]),
        Err(ShmError::MessageTooLarge)
    );

    wait_until("all delivered", Duration::from_secs(60), || {
        server_handler.received.lock().unwrap().len() as u32 >= TOTAL
    });
    let received = server_handler.received.lock().unwrap().clone();
    assert_eq!(received.len() as u32, TOTAL, "лишние сообщения");
    assert!(
        received.iter().copied().eq(0..TOTAL),
        "потеря или перестановка"
    );
    assert!(!server_handler.bad.load(Ordering::Acquire), "порча данных");
    assert!(fulls > 0, "backpressure ни разу не сработал");
    // После разгрузки оценка места вернулась к «почти пустому кольцу».
    wait_until("space restored", Duration::from_secs(5), || {
        client.free_space().bytes == RING_CAPACITY
    });
    println!("dispatch client->server: {TOTAL} msgs, QueueFull {fulls} times");

    client.stop();
    server.stop();
}

/// Обратное направление: `DispatchServer::try_send_to` к медленному клиенту.
#[test]
fn dispatch_server_try_send_to_is_lossless() {
    const TOTAL: u32 = 800;
    let name = unique_name("DSPS");
    let server_handler = Arc::new(SlowServer {
        received: Mutex::new(Vec::new()),
        connected: AtomicU32::new(0),
        bad: AtomicBool::new(false),
        delay: Duration::ZERO,
    });
    let server = DispatchServer::start(&name, server_handler, DispatchOptions::default()).unwrap();
    thread::sleep(Duration::from_millis(100));
    let client_handler = Arc::new(RecordingClient::default());
    let client = DispatchClient::connect(
        &name,
        ClientRegistration {
            pid: std::process::id(),
            revision: 1,
            name: "bp-viewer-side".into(),
        },
        client_handler.clone(),
        DispatchClientOptions::default(),
    )
    .unwrap();
    wait_until("server connect", Duration::from_secs(5), || {
        server.client_count() == 1
    });
    let id = server.connected_clients()[0];
    assert!(server.free_space(id).is_some());
    assert_eq!(server.free_space(id + 1000), None);

    let fulls = AtomicU64::new(0);
    for seq in 0..TOTAL {
        let payload = make_payload(seq, len_for(seq));
        while let Err(err) = server.try_send_to(id, &payload) {
            assert_eq!(err, ShmError::QueueFull);
            fulls.fetch_add(1, Ordering::Relaxed);
            thread::sleep(Duration::from_micros(200));
        }
    }
    wait_until("all delivered", Duration::from_secs(60), || {
        client_handler.received.load(Ordering::Acquire) >= TOTAL
    });
    assert_eq!(client_handler.received.load(Ordering::Acquire), TOTAL);
    assert!(!client_handler.bad.load(Ordering::Acquire), "потеря/порча");
    println!(
        "dispatch server->client: {TOTAL} msgs, QueueFull {} times",
        fulls.load(Ordering::Relaxed)
    );
    client.stop();
    server.stop();
}
