//! Настраиваемая раскладка канала (формат `0x0002_0000`): размеры колец,
//! лимиты сообщений по каждому направлению, сообщения больше 64 КиБ --
//! во всех режимах (Shared, Auto, Multi, Dispatch).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use xshm::{
    Admission, AutoClient, AutoHandler, AutoOptions, AutoServer, ChannelKind, ChannelLayout,
    ClientRegistration, DispatchClient, DispatchClientHandler, DispatchClientOptions,
    DispatchHandler, DispatchOptions, DispatchServer, MultiClient, MultiClientHandler,
    MultiClientOptions, MultiHandler, MultiOptions, MultiServer, RingConfig, SharedClient,
    SharedServer, ShmError,
};

fn unique_name(tag: &str) -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    format!(
        "XSHM_LAYOUT_{tag}_{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "таймаут: {what}");
        thread::sleep(Duration::from_millis(5));
    }
}

/// Сервер шлёт крупные снимки, клиент -- короткие команды.
const fn asymmetric() -> ChannelLayout {
    ChannelLayout {
        server_to_client: RingConfig::new(8 << 20).max_message_size(2 << 20),
        client_to_server: RingConfig::new(16 << 10)
            .max_messages(4)
            .max_message_size(256),
    }
}

fn payload(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

#[test]
fn shared_asymmetric_layout_roundtrip_with_large_messages() {
    let name = unique_name("SHARED");
    let layout = asymmetric();
    let mut server = SharedServer::start_with_layout(&name, layout).unwrap();
    assert_eq!(*server.layout(), layout);

    let client_thread = thread::spawn(move || SharedClient::connect(&name, Duration::from_secs(5)));
    server
        .wait_for_client(Some(Duration::from_secs(5)))
        .unwrap();
    let client = client_thread.join().unwrap().unwrap();
    assert_eq!(
        *client.layout(),
        layout,
        "клиент читает раскладку из сегмента"
    );

    // Сервер -> клиент: сообщение 2 МиБ (больше прежнего предела 64 КиБ).
    let big = payload(2 << 20, 7);
    server.send_to_client(&big).unwrap();
    assert_eq!(
        server.send_to_client(&payload((2 << 20) + 1, 0)),
        Err(ShmError::MessageTooLarge)
    );
    let mut out = Vec::new();
    assert_eq!(client.receive_from_server(&mut out).unwrap(), big.len());
    assert_eq!(out, big);

    // Клиент -> сервер: лимит 256 байт и 4 сообщения на кольцо.
    assert_eq!(
        client.try_send_to_server(&[1u8; 257]),
        Err(ShmError::MessageTooLarge)
    );
    for i in 0..4u8 {
        client.try_send_to_server(&[i; 256]).unwrap();
    }
    assert_eq!(
        client.try_send_to_server(&[9u8; 8]),
        Err(ShmError::QueueFull),
        "лимит сообщений направления"
    );
    let free = client.free_space();
    assert_eq!(free.messages, 0);
    assert_eq!(free.max_message_size, 256);
    for i in 0..4u8 {
        assert_eq!(server.receive_from_client(&mut out).unwrap(), 256);
        assert_eq!(out, [i; 256]);
    }
}

#[test]
fn invalid_layout_is_rejected_before_creating_objects() {
    let name = unique_name("INVALID");
    let bad = ChannelLayout::symmetric(RingConfig::new(3 * 4096));
    assert!(matches!(
        SharedServer::start_with_layout(&name, bad),
        Err(ShmError::InvalidConfig(_))
    ));
    // Имя не занято: отказ случился до создания секции.
    SharedServer::start(&name).unwrap();
}

#[derive(Default)]
struct AutoRecorder {
    connected: AtomicBool,
    received: Mutex<Vec<Vec<u8>>>,
}

impl AutoHandler for AutoRecorder {
    fn on_connect(&self) {
        self.connected.store(true, Ordering::Release);
    }
    #[expect(
        clippy::unwrap_used,
        reason = "тестовый помощник: паника -- провал теста"
    )]
    fn on_message(&self, _direction: ChannelKind, payload: &[u8]) {
        self.received.lock().unwrap().push(payload.to_vec());
    }
}

#[test]
fn auto_client_adopts_server_layout() {
    let name = unique_name("AUTO");
    let layout = asymmetric();
    let server_h = Arc::new(AutoRecorder::default());
    let client_h = Arc::new(AutoRecorder::default());
    let server = AutoServer::start(
        &name,
        server_h.clone(),
        AutoOptions {
            layout,
            ..AutoOptions::default()
        },
    )
    .unwrap();
    let client = AutoClient::connect(&name, client_h.clone(), AutoOptions::default()).unwrap();
    wait_until("подключение", || {
        server_h.connected.load(Ordering::Acquire) && client_h.connected.load(Ordering::Acquire)
    });
    assert_eq!(*server.layout(), layout);
    assert_eq!(client.layout(), Some(layout));

    // Лимит исходящего кольца известен -- длина проверяется синхронно.
    assert_eq!(client.send(&[0u8; 257]), Err(ShmError::MessageTooLarge));
    assert_eq!(
        server.send(&payload((2 << 20) + 1, 0)),
        Err(ShmError::MessageTooLarge)
    );

    let big = payload(1 << 20, 3);
    server.try_send(&big).unwrap();
    client.try_send(b"cmd").unwrap();
    wait_until("доставка", || {
        !client_h.received.lock().unwrap().is_empty()
            && !server_h.received.lock().unwrap().is_empty()
    });
    assert_eq!(client_h.received.lock().unwrap()[0], big);
    assert_eq!(server_h.received.lock().unwrap()[0], b"cmd");

    // stop() синхронный: после возврата колбэков нет.
    client.stop();
    server.stop();
}

struct MultiServerRecorder {
    connected: AtomicUsize,
    received: Mutex<Vec<Vec<u8>>>,
}

impl MultiHandler for MultiServerRecorder {
    fn on_client_connect(&self, _client_id: u32) {
        self.connected.fetch_add(1, Ordering::AcqRel);
    }
    fn on_client_disconnect(&self, _client_id: u32) {}
    #[expect(
        clippy::unwrap_used,
        reason = "тестовый помощник: паника -- провал теста"
    )]
    fn on_message(&self, _client_id: u32, data: &[u8]) {
        self.received.lock().unwrap().push(data.to_vec());
    }
}

#[derive(Default)]
struct MultiClientRecorder {
    connected: AtomicBool,
    received: Mutex<Vec<Vec<u8>>>,
}

impl MultiClientHandler for MultiClientRecorder {
    fn on_connect(&self, _slot_id: u32) {
        self.connected.store(true, Ordering::Release);
    }
    fn on_disconnect(&self) {}
    #[expect(
        clippy::unwrap_used,
        reason = "тестовый помощник: паника -- провал теста"
    )]
    fn on_message(&self, data: &[u8]) {
        self.received.lock().unwrap().push(data.to_vec());
    }
}

#[test]
fn multi_slots_use_configured_layout() {
    let name = unique_name("MULTI");
    // 4 слота по 2 × 64 КиБ вместо 4 × 4 МиБ commit-а.
    let layout = ChannelLayout::symmetric(RingConfig::new(64 << 10).max_messages(64));
    let server_h = Arc::new(MultiServerRecorder {
        connected: AtomicUsize::new(0),
        received: Mutex::new(Vec::new()),
    });
    let server = MultiServer::start(
        &name,
        server_h.clone(),
        MultiOptions {
            max_clients: 4,
            layout,
            ..MultiOptions::default()
        },
    )
    .unwrap();
    let client_h = Arc::new(MultiClientRecorder::default());
    let client =
        MultiClient::connect(&name, client_h.clone(), MultiClientOptions::default()).unwrap();
    wait_until("подключение", || {
        client_h.connected.load(Ordering::Acquire)
            && server_h.connected.load(Ordering::Acquire) == 1
    });
    assert_eq!(client.layout(), Some(layout));

    // Сообщение больше 64 КиБ не помещается в кольцо 64 КиБ -- синхронный отказ.
    assert_eq!(
        client.send(&[0u8; 64 << 10]),
        Err(ShmError::MessageTooLarge)
    );
    let msg = payload((64 << 10) - 8, 1);
    client.send(&msg).unwrap();
    let id = client.slot_id();
    server.send_to(id, b"hello").unwrap();
    wait_until("доставка", || {
        !server_h.received.lock().unwrap().is_empty()
            && !client_h.received.lock().unwrap().is_empty()
    });
    assert_eq!(server_h.received.lock().unwrap()[0], msg);
    assert_eq!(client_h.received.lock().unwrap()[0], b"hello");
    client.stop();
}

/// Регрессия (аудит 2026-10-16): worker-ы держали сильную ссылку на сервер,
/// и `Arc<MultiServer>`, отпущенный без `stop()`, жил вечно -- имя слотов
/// оставалось занятым до конца процесса.
#[test]
fn dropping_multi_server_without_stop_releases_it() {
    let name = unique_name("MULTI_DROP");
    let handler = Arc::new(MultiServerRecorder {
        connected: AtomicUsize::new(0),
        received: Mutex::new(Vec::new()),
    });
    let options = MultiOptions {
        max_clients: 2,
        layout: ChannelLayout::symmetric(RingConfig::new(16 << 10)),
        ..MultiOptions::default()
    };
    let server = MultiServer::start(&name, handler.clone(), options.clone()).unwrap();
    drop(server);
    MultiServer::start(&name, handler, options).expect("имя должно освободиться после Drop");
}

struct DispatchPolicy {
    connected: AtomicUsize,
    received: Mutex<Vec<Vec<u8>>>,
}

impl DispatchHandler for DispatchPolicy {
    fn admit(&self, info: &ClientRegistration) -> Admission {
        match info.name.as_str() {
            "banned" => Admission::Reject,
            "bulk" => Admission::AcceptWithLayout(asymmetric()),
            _ => Admission::Accept,
        }
    }
    fn on_client_connect(&self, _client_id: u32, _info: &ClientRegistration) {
        self.connected.fetch_add(1, Ordering::AcqRel);
    }
    fn on_client_disconnect(&self, _client_id: u32) {}
    #[expect(
        clippy::unwrap_used,
        reason = "тестовый помощник: паника -- провал теста"
    )]
    fn on_message(&self, _client_id: u32, data: &[u8]) {
        self.received.lock().unwrap().push(data.to_vec());
    }
}

#[derive(Default)]
struct DispatchClientRecorder {
    connected: AtomicBool,
    received: Mutex<Vec<Vec<u8>>>,
}

impl DispatchClientHandler for DispatchClientRecorder {
    fn on_connect(&self, _client_id: u32, _channel_name: &str) {
        self.connected.store(true, Ordering::Release);
    }
    fn on_disconnect(&self) {}
    #[expect(
        clippy::unwrap_used,
        reason = "тестовый помощник: паника -- провал теста"
    )]
    fn on_message(&self, data: &[u8]) {
        self.received.lock().unwrap().push(data.to_vec());
    }
}

fn registration(name: &str) -> ClientRegistration {
    ClientRegistration::new(std::process::id(), 1, name.to_owned())
}

#[test]
fn dispatch_admission_controls_channels_and_layouts() {
    let lobby = unique_name("DISPATCH");
    let policy = Arc::new(DispatchPolicy {
        connected: AtomicUsize::new(0),
        received: Mutex::new(Vec::new()),
    });
    let server = DispatchServer::start(
        &lobby,
        policy.clone(),
        DispatchOptions {
            max_clients: 2,
            ..DispatchOptions::default()
        },
    )
    .unwrap();

    // Отказ обработчика: канал не создаётся.
    let rejected = DispatchClient::connect(
        &lobby,
        registration("banned"),
        Arc::new(DispatchClientRecorder::default()),
        DispatchClientOptions::default(),
    );
    assert_eq!(rejected.err(), Some(ShmError::HandshakeFailed));

    // Раскладка канала, выбранная для клиента.
    let bulk_h = Arc::new(DispatchClientRecorder::default());
    let bulk = DispatchClient::connect(
        &lobby,
        registration("bulk"),
        bulk_h.clone(),
        DispatchClientOptions::default(),
    )
    .unwrap();
    wait_until("подключение bulk", || {
        bulk_h.connected.load(Ordering::Acquire)
    });
    let big = payload(1 << 20, 9);
    server.try_send_to(bulk.client_id(), &big).unwrap();
    wait_until("снимок", || {
        !bulk_h.received.lock().unwrap().is_empty()
    });
    assert_eq!(bulk_h.received.lock().unwrap()[0], big);
    assert_eq!(bulk.send(&[0u8; 257]), Err(ShmError::MessageTooLarge));

    // Второй клиент -- раскладка по умолчанию; третий упирается в лимит.
    let plain_h = Arc::new(DispatchClientRecorder::default());
    let plain = DispatchClient::connect(
        &lobby,
        registration("plain"),
        plain_h.clone(),
        DispatchClientOptions::default(),
    )
    .unwrap();
    wait_until("подключение plain", || {
        plain_h.connected.load(Ordering::Acquire)
    });
    let over = DispatchClient::connect(
        &lobby,
        registration("third"),
        Arc::new(DispatchClientRecorder::default()),
        DispatchClientOptions::default(),
    );
    assert_eq!(
        over.err(),
        Some(ShmError::HandshakeFailed),
        "лимит max_clients"
    );
    assert_eq!(policy.connected.load(Ordering::Acquire), 2);

    drop(plain);
    drop(bulk);
    server.stop();
}

/// Регрессия (аудит 2026-10-16): поток лобби держал сильную ссылку на
/// сервер -- `Arc<DispatchServer>`, отпущенный без `stop()`, жил вечно.
#[test]
fn dropping_dispatch_server_without_stop_releases_it() {
    let lobby = unique_name("DISPATCH_DROP");
    let policy = Arc::new(DispatchPolicy {
        connected: AtomicUsize::new(0),
        received: Mutex::new(Vec::new()),
    });
    let server = DispatchServer::start(&lobby, policy.clone(), DispatchOptions::default()).unwrap();
    drop(server);
    DispatchServer::start(&lobby, policy, DispatchOptions::default())
        .expect("лобби должно освободиться после Drop");
}
