//! Частное пространство имён (0.10): каналы, лобби, слоты и маяки вне
//! общего `\BaseNamedObjects` -- найти и занять их может только процесс с
//! той же границей (тот же пользователь).

use std::future::Future;
use std::pin::pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use std::task::{Context, Poll, Wake, Waker};
use std::thread;
use std::time::Duration;

use xshm::{
    AutoClient, AutoHandler, AutoOptions, AutoServer, Beacon, ChannelConfig, ChannelKind,
    ClientRegistration, DispatchClient, DispatchClientHandler, DispatchClientOptions,
    DispatchHandler, DispatchOptions, DispatchServer, MultiClient, MultiClientHandler,
    MultiClientOptions, MultiHandler, MultiOptions, MultiServer, ObjectSecurity, PrivateNamespace,
    SharedClient, SharedServer, ShmError,
};

/// `STATUS_OBJECT_NAME_NOT_FOUND`.
const STATUS_OBJECT_NAME_NOT_FOUND: u32 = 0xC000_0034;

fn unique(tag: &str) -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    format!(
        "XSHM_NS_{tag}_{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

#[expect(
    clippy::unwrap_used,
    reason = "тестовый помощник: паника -- провал теста"
)]
fn namespace(boundary: &str) -> Arc<PrivateNamespace> {
    PrivateNamespace::open_or_create(boundary, "xshm", &ObjectSecurity::default()).unwrap()
}

const fn is_not_found(result: &Result<SharedClient, ShmError>) -> bool {
    matches!(
        result,
        Err(ShmError::WindowsError { code, .. }) if *code == STATUS_OBJECT_NAME_NOT_FOUND
    )
}

#[test]
fn channel_in_namespace_is_invisible_outside() {
    let ns = namespace(&unique("B_INVISIBLE"));
    let name = unique("INVISIBLE");
    let _server =
        SharedServer::start_with(&name, &ChannelConfig::default().namespace(Arc::clone(&ns)))
            .unwrap();

    // Ни неявный `Local\`, ни явный -- канала там нет.
    let outside = SharedClient::connect(&name, Duration::from_millis(200));
    assert!(is_not_found(&outside), "{outside:?}");
    let local = SharedClient::connect(&format!("Local\\{name}"), Duration::from_millis(200));
    assert!(is_not_found(&local), "{local:?}");
}

#[test]
fn channel_in_namespace_connects_inside() {
    let boundary = unique("B_INSIDE");
    let name = unique("INSIDE");
    let server_ns = namespace(&boundary);
    let mut server = SharedServer::start_with(
        &name,
        &ChannelConfig::default().namespace(Arc::clone(&server_ns)),
    )
    .unwrap();

    // Клиент находит пространство сам -- по той же границе.
    let client_ns = namespace(&boundary);
    assert!(!client_ns.created());
    let client = thread::spawn(move || {
        let client = SharedClient::connect_in(&client_ns, &name, Duration::from_secs(5)).unwrap();
        client.send_to_server(b"ping").unwrap();
        client
    });
    server
        .wait_for_client(Some(Duration::from_secs(5)))
        .unwrap();
    let client = client.join().unwrap();

    assert!(server.poll_client(Some(Duration::from_secs(5))).unwrap());
    let mut buffer = Vec::new();
    let len = server.receive_from_client(&mut buffer).unwrap();
    assert_eq!(&buffer[..len], b"ping");
    server.send_to_client(b"pong").unwrap();
    let len = client.receive_from_server(&mut buffer).unwrap();
    assert_eq!(&buffer[..len], b"pong");
}

#[test]
fn prefixed_name_in_namespace_is_rejected() {
    let ns = namespace(&unique("B_PREFIX"));
    let config = ChannelConfig::default().namespace(Arc::clone(&ns));
    for name in [
        "Local\\Chan",
        "Global\\Chan",
        "\\BaseNamedObjects\\Chan",
        "",
    ] {
        assert!(
            matches!(
                SharedServer::start_with(name, &config),
                Err(ShmError::InvalidConfig(_))
            ),
            "server: {name:?}"
        );
        assert!(
            matches!(
                SharedClient::connect_in(&ns, name, Duration::from_millis(100)),
                Err(ShmError::InvalidConfig(_))
            ),
            "client: {name:?}"
        );
    }
}

/// Объект под тем же именем в общем `Local\` (захватчик) не мешает каналу
/// в пространстве: имена не пересекаются (находка 13).
#[test]
fn squatter_outside_does_not_block_namespace_channel() {
    let name = unique("SQUAT");
    let _squatter = SharedServer::start(&name).unwrap();
    let ns = namespace(&unique("B_SQUAT"));
    assert!(SharedServer::start_with(&name, &ChannelConfig::default().namespace(ns)).is_ok());
}

struct Lobby(mpsc::Sender<Vec<u8>>);

impl DispatchHandler for Lobby {
    fn on_client_connect(&self, _client_id: u32, _info: &ClientRegistration) {}
    fn on_client_disconnect(&self, _client_id: u32) {}
    fn on_message(&self, _client_id: u32, data: &[u8]) {
        self.0.send(data.to_vec()).ok();
    }
}

struct Channel(mpsc::Sender<String>);

impl DispatchClientHandler for Channel {
    fn on_connect(&self, _client_id: u32, channel_name: &str) {
        self.0.send(channel_name.to_owned()).ok();
    }
    fn on_disconnect(&self) {}
    fn on_message(&self, _data: &[u8]) {}
}

#[test]
fn dispatch_channels_inherit_lobby_namespace() {
    let boundary = unique("B_DISPATCH");
    let lobby = unique("LOBBY");
    let (messages, received) = mpsc::channel();
    let server = DispatchServer::start(
        &lobby,
        Arc::new(Lobby(messages)),
        DispatchOptions {
            namespace: Some(namespace(&boundary)),
            ..DispatchOptions::default()
        },
    )
    .unwrap();

    // Без пространства лобби не найти.
    assert!(!DispatchClient::lobby_exists(&lobby));
    let outside = DispatchClient::connect(
        &lobby,
        ClientRegistration::new(std::process::id(), 1, "outside"),
        Arc::new(Channel(mpsc::channel().0)),
        DispatchClientOptions {
            lobby_timeout: Duration::from_millis(300),
            ..DispatchClientOptions::default()
        },
    );
    assert!(outside.is_err());

    let ns = namespace(&boundary);
    assert!(DispatchClient::lobby_exists_in(&ns, &lobby));
    let (connected, channel) = mpsc::channel();
    let client = DispatchClient::connect(
        &lobby,
        ClientRegistration::new(std::process::id(), 1, "inside"),
        Arc::new(Channel(connected)),
        DispatchClientOptions {
            namespace: Some(Arc::clone(&ns)),
            ..DispatchClientOptions::default()
        },
    )
    .unwrap();
    let channel_name = channel.recv_timeout(Duration::from_secs(10)).unwrap();
    client.send(b"hello").unwrap();
    assert_eq!(
        received.recv_timeout(Duration::from_secs(10)).unwrap(),
        b"hello"
    );

    // Канал -- относительное имя в том же пространстве, снаружи его нет.
    assert!(!channel_name.contains('\\'), "{channel_name}");
    let outside = SharedClient::connect(&channel_name, Duration::from_millis(200));
    assert!(is_not_found(&outside), "{outside:?}");

    client.stop();
    server.stop();
}

struct Inbox(mpsc::Sender<Vec<u8>>);

impl AutoHandler for Inbox {
    fn on_message(&self, _direction: ChannelKind, payload: &[u8]) {
        self.0.send(payload.to_vec()).ok();
    }
}

/// Сервер перезапущен (его handle пространства закрыт), а клиент держит
/// свой: пространство живо, новый сервер открывает ТО ЖЕ, и клиент
/// переподключается. Поэтому `Drop` не удаляет пространство
/// (`NtDeletePrivateNamespace`), а только закрывает handle.
#[test]
fn auto_client_reconnects_to_restarted_server_in_namespace() {
    let boundary = unique("B_RESTART");
    let name = unique("RESTART");
    let options = |ns: Arc<PrivateNamespace>| AutoOptions {
        namespace: Some(ns),
        reconnect_delay: Duration::from_millis(20),
        ..AutoOptions::default()
    };

    let (to_server, server_inbox) = mpsc::channel();
    let server = AutoServer::start(
        &name,
        Arc::new(Inbox(to_server.clone())),
        options(namespace(&boundary)),
    )
    .unwrap();
    let client = AutoClient::connect(
        &name,
        Arc::new(Inbox(mpsc::channel().0)),
        options(namespace(&boundary)),
    )
    .unwrap();
    client.send(b"first").unwrap();
    assert_eq!(
        server_inbox.recv_timeout(Duration::from_secs(10)).unwrap(),
        b"first"
    );

    server.stop();
    drop(server);
    let restarted = AutoServer::start(
        &name,
        Arc::new(Inbox(to_server)),
        options(namespace(&boundary)),
    )
    .unwrap();
    // Сообщение могло уйти в старый сервер до разрыва -- шлём, пока не дойдёт.
    let mut delivered = false;
    for _ in 0..200 {
        let _ = client.send(b"second");
        if let Ok(message) = server_inbox.recv_timeout(Duration::from_millis(50))
            && message == b"second"
        {
            delivered = true;
            break;
        }
    }
    assert!(delivered, "клиент не нашёл перезапущенный сервер");
    restarted.stop();
}

struct Slots(mpsc::Sender<Vec<u8>>);

impl MultiHandler for Slots {
    fn on_client_connect(&self, _client_id: u32) {}
    fn on_client_disconnect(&self, _client_id: u32) {}
    fn on_message(&self, _client_id: u32, data: &[u8]) {
        self.0.send(data.to_vec()).ok();
    }
}

struct SlotClient(mpsc::Sender<ShmError>);

impl MultiClientHandler for SlotClient {
    fn on_connect(&self, _slot_id: u32) {}
    fn on_disconnect(&self) {}
    fn on_message(&self, _data: &[u8]) {}
    fn on_error(&self, err: ShmError) {
        self.0.send(err).ok();
    }
}

#[test]
fn multi_slots_live_in_namespace() {
    let boundary = unique("B_MULTI");
    let name = unique("MULTI");
    let (messages, received) = mpsc::channel();
    let server = MultiServer::start(
        &name,
        Arc::new(Slots(messages)),
        MultiOptions {
            max_clients: 2,
            namespace: Some(namespace(&boundary)),
            ..MultiOptions::default()
        },
    )
    .unwrap();

    // Снаружи слотов нет -- «сервер не запущен».
    let (errors, outside_errors) = mpsc::channel();
    let outside = MultiClient::connect(
        &name,
        Arc::new(SlotClient(errors)),
        MultiClientOptions::default(),
    )
    .unwrap();
    assert_eq!(
        outside_errors.recv_timeout(Duration::from_secs(5)).unwrap(),
        ShmError::NotConnected
    );
    outside.stop();

    let client = MultiClient::connect(
        &name,
        Arc::new(SlotClient(mpsc::channel().0)),
        MultiClientOptions {
            namespace: Some(namespace(&boundary)),
            ..MultiClientOptions::default()
        },
    )
    .unwrap();
    let mut delivered = false;
    for _ in 0..200 {
        let _ = client.send(b"slot");
        if received.recv_timeout(Duration::from_millis(50)).is_ok() {
            delivered = true;
            break;
        }
    }
    assert!(delivered);
    client.stop();
    server.stop();
}

#[test]
fn beacon_in_namespace_is_separate_from_shared_one() {
    let boundary = unique("B_BEACON");
    let name = unique("BEACON");
    let inside = Beacon::open_in(&namespace(&boundary), &name, &ObjectSecurity::default()).unwrap();
    let outside = Beacon::open(&name).unwrap();
    let peer = Beacon::open_in(&namespace(&boundary), &name, &ObjectSecurity::default()).unwrap();

    inside.raise().unwrap();
    assert!(peer.is_raised().unwrap(), "тот же маяк внутри пространства");
    assert!(!outside.is_raised().unwrap(), "общий маяк -- другой объект");
    inside.lower().unwrap();
    assert!(!peer.is_raised().unwrap());
}

// ─── async в пространстве имён (Task 3 надёжности, пункты 2 и 10) ───────────

/// `Waker`, будящий поток-владельца.
struct Unpark(thread::Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Минимальный исполнитель: опрос, пока не готово, сон до `wake`
/// (`thread::park` без таймаута; ложный возврат `park` безвреден).
fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
        thread::park();
    }
}

/// Рукопожатие и эхо целиком через async API в частном пространстве:
/// `accept_async` ждёт на событиях, открытых `start_with` в пространстве,
/// `connect_async_in` находит канал по той же границе.
#[test]
fn async_connect_in_namespace_echo() {
    let boundary = unique("B_ASYNC");
    let name = unique("ASYNC");
    let ns = namespace(&boundary);
    let mut server =
        SharedServer::start_with(&name, &ChannelConfig::default().namespace(Arc::clone(&ns)))
            .unwrap();
    let server = thread::spawn(move || {
        block_on(server.accept_async()).unwrap();
        let mut buffer = Vec::new();
        let len = block_on(server.recv_async(&mut buffer)).unwrap();
        block_on(server.send_async(&buffer[..len])).unwrap();
        server
    });

    let ns2 = namespace(&boundary);
    assert!(!ns2.created());
    let mut client = block_on(SharedClient::connect_async_in(Arc::clone(&ns2), &name)).unwrap();
    let payload: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
    block_on(client.send_async(&payload)).unwrap();
    let mut buffer = Vec::new();
    let len = block_on(client.recv_async(&mut buffer)).unwrap();
    assert_eq!(&buffer[..len], &payload[..]);
    let _server = server.join().unwrap();
}

/// Без пространства (`connect_async`) канал в пространстве не виден.
#[test]
fn async_connect_outside_namespace_is_not_found() {
    let ns = namespace(&unique("B_ASYNC_OUT"));
    let name = unique("ASYNC_OUT");
    let _server =
        SharedServer::start_with(&name, &ChannelConfig::default().namespace(Arc::clone(&ns)))
            .unwrap();
    let outside = block_on(SharedClient::connect_async(&name));
    assert!(is_not_found(&outside), "{outside:?}");
}

/// Префикс или `\` в имени внутри пространства -- `InvalidConfig` на первом
/// опросе (как у `connect_in`).
#[test]
fn async_connect_in_rejects_prefixed_name() {
    let ns = namespace(&unique("B_ASYNC_PREFIX"));
    for name in ["Local\\X", "Global\\X", "\\BaseNamedObjects\\X", ""] {
        let mut future = pin!(SharedClient::connect_async_in(Arc::clone(&ns), name));
        let waker = Waker::from(Arc::new(Unpark(thread::current())));
        let mut cx = Context::from_waker(&waker);
        assert!(
            matches!(
                future.as_mut().poll(&mut cx),
                Poll::Ready(Err(ShmError::InvalidConfig(_)))
            ),
            "{name:?}"
        );
    }
}

/// Пункт 10 (решение 12): `Drop` создателя не удаляет пространство. Пока
/// handle держит пир (`ns_b`), пространство с той же границей -- то же
/// самое: новый `open_or_create` его открывает (`created() == false`; если
/// бы `Drop` вызывал `NtDeletePrivateNamespace`, создалось бы другое --
/// `true`), и канал, поднятый через новый handle, виден через handle пира.
#[test]
fn creator_drop_keeps_namespace_for_peers() {
    let boundary = unique("B_KEEP");
    let ns_a = namespace(&boundary);
    assert!(ns_a.created());
    let ns_b = namespace(&boundary);
    assert!(!ns_b.created());

    let first = unique("KEEP_A");
    let server = SharedServer::start_with(
        &first,
        &ChannelConfig::default().namespace(Arc::clone(&ns_a)),
    )
    .unwrap();
    drop(server);
    assert_eq!(Arc::strong_count(&ns_a), 1, "сервер отпустил пространство");
    drop(ns_a);

    let ns_c = namespace(&boundary);
    assert!(!ns_c.created(), "пространство пережило Drop создателя");
    let name = unique("KEEP_C");
    let mut server = SharedServer::start_with(
        &name,
        &ChannelConfig::default().namespace(Arc::clone(&ns_c)),
    )
    .unwrap();
    let client = thread::spawn(move || {
        SharedClient::connect_in(&ns_b, &name, Duration::from_secs(2)).map(|_client| ())
    });
    server
        .wait_for_client(Some(Duration::from_secs(5)))
        .unwrap();
    client.join().unwrap().unwrap();
}
