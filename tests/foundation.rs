//! Поведенческие проверки безопасной границы над реальным C-ядром.
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use xshm::{
    Beacon, CancellationToken, ChannelConfig, Client, ConnectBudget, Error, MultiClientOptions,
    MultiEvent, MultiOptions, MultiServer, PrivateNamespace, Security, Server, Wait,
};

fn name() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "xshm_c_core_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
const fn finite() -> Wait {
    Wait::For(Duration::from_secs(5))
}

#[test]
fn exact_buffers_session_drop_and_listener_reuse() {
    fn requires_send<T: Send>() {}
    requires_send::<Client>();
    requires_send::<Server>();
    let name = name();
    let mut server = Server::bind(&name, ChannelConfig::default()).unwrap();
    let worker = std::thread::spawn(move || {
        for _ in 0..2 {
            let mut session = server.accept(finite(), None).unwrap();
            for size in [1, 7, 8, 9, 65535] {
                session.wait_readable(finite(), None).unwrap();
                let mut empty = [];
                assert!(
                    matches!(session.receive_into(&mut empty), Err(Error::BufferTooSmall { required }) if required == size)
                );
                let mut data = vec![0; size];
                assert_eq!(session.receive_into(&mut data).unwrap(), size);
                session.try_send(&data).unwrap();
            }
            // Session отпускает слушатель для следующего клиента.
        }
    });
    for round in 0..2 {
        let mut client = Client::connect(&name, finite(), None).unwrap();
        assert!(matches!(client.try_send(&[]), Err(Error::MessageTooSmall)));
        for size in [1, 7, 8, 9, 65535] {
            let data = vec![round; size];
            client.try_send(&data).unwrap();
            client.wait_readable(finite(), None).unwrap();
            let mut reply = vec![0; size];
            assert_eq!(client.receive_into(&mut reply).unwrap(), size);
            assert_eq!(reply, data);
        }
    }
    worker.join().unwrap();
}

#[test]
fn cancellation_clone_keeps_handle_alive_and_interrupts_infinite_accept() {
    let mut server = Server::bind(&name(), ChannelConfig::default()).unwrap();
    let token = CancellationToken::new().unwrap();
    let other = token.clone();
    drop(token);
    let cancel = other.clone();
    let worker = std::thread::spawn(move || {
        assert!(matches!(
            server.accept(Wait::Infinite, Some(&other)),
            Err(Error::Cancelled)
        ));
    });
    cancel.cancel().unwrap();
    cancel.cancel().unwrap();
    worker.join().unwrap();
}

#[test]
fn last_frame_is_drained_after_client_drop() {
    let name = name();
    let mut server = Server::bind(&name, ChannelConfig::default()).unwrap();
    let worker = std::thread::spawn(move || {
        let mut client = Client::connect(&name, finite(), None).unwrap();
        client.try_send(b"last frame").unwrap();
    });
    let mut session = server.accept(finite(), None).unwrap();
    worker.join().unwrap();
    session.wait_readable(finite(), None).unwrap();
    let mut bytes = [0; 10];
    assert_eq!(session.receive_into(&mut bytes).unwrap(), bytes.len());
    assert_eq!(&bytes, b"last frame");
    assert!(matches!(
        session.wait_readable(finite(), None),
        Err(Error::Disconnected(_))
    ));
}

#[test]
fn invalid_rust_inputs_are_rejected_without_panics() {
    assert!(matches!(
        Client::connect("has\0nul", finite(), None),
        Err(Error::NameContainsNul(_))
    ));
    assert!(matches!(
        Client::connect(&name(), Wait::For(Duration::MAX), None),
        Err(Error::TimeoutTooLarge)
    ));
    let mut config = ChannelConfig::default();
    config.layout.s2c.capacity = 3;
    assert!(matches!(
        Server::bind(&name(), config),
        Err(Error::InvalidConfig)
    ));
}

#[test]
fn failed_creation_does_not_destroy_existing_server() {
    let name = name();
    let mut server = Server::bind(&name, ChannelConfig::default()).unwrap();
    assert!(matches!(
        Server::bind(&name, ChannelConfig::default()),
        Err(Error::NameCollision)
    ));
    let worker = std::thread::spawn(move || Client::connect(&name, finite(), None).unwrap());
    let session = server.accept(finite(), None).unwrap();
    let client = worker.join().unwrap();
    drop(session);
    drop(client);
}

#[test]
fn repeated_raii_cleanup_keeps_handle_count_stable() {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    // SAFETY: стандартные Windows ABI-сигнатуры, только для измерения теста.
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn GetProcessHandleCount(process: *mut c_void, count: *mut u32) -> i32;
    }
    fn handles() -> u32 {
        let mut count = 0;
        // SAFETY: псевдодескриптор процесса всегда валиден и не закрывается.
        let process = unsafe { GetCurrentProcess() };
        // SAFETY: count доступен для записи всё время вызова.
        assert_ne!(unsafe { GetProcessHandleCount(process, &raw mut count) }, 0);
        count
    }
    // Одноразовая инициализация C-ядра — вне базовой линии.
    drop(Server::bind(&name(), ChannelConfig::default()).unwrap());
    let before = handles();
    for _ in 0..32 {
        let server = Server::bind(&name(), ChannelConfig::default()).unwrap();
        let token = CancellationToken::new().unwrap();
        let clone = token.clone();
        drop(token);
        clone.cancel().unwrap();
        drop(clone);
        drop(server);
    }
    assert_eq!(handles(), before);
}

#[test]
fn private_namespace_is_retained_after_original_owner_is_dropped() {
    let namespace = PrivateNamespace::open(&name(), Security::CurrentUser).unwrap();
    let client_namespace = namespace.clone();
    let config = ChannelConfig {
        namespace: Some(namespace.clone()),
        ..ChannelConfig::default()
    };
    let mut server = Server::bind("channel", config).unwrap();
    drop(namespace);
    let worker = std::thread::spawn(move || {
        let mut client = Client::connect_in("channel", &client_namespace, finite(), None).unwrap();
        drop(client_namespace); // теперь пространство удерживает только канал
        client.try_send(b"ns").unwrap();
        client.wait_readable(finite(), None).unwrap();
        let mut reply = [0; 2];
        assert_eq!(client.receive_into(&mut reply).unwrap(), 2);
        assert_eq!(&reply, b"ok");
    });
    let mut session = server.accept(finite(), None).unwrap();
    session.wait_readable(finite(), None).unwrap();
    let mut bytes = [0; 2];
    assert_eq!(session.receive_into(&mut bytes).unwrap(), 2);
    assert_eq!(&bytes, b"ns");
    session.try_send(b"ok").unwrap();
    drop(session);
    drop(server);
    worker.join().unwrap();
}

#[test]
fn beacon_clone_keeps_namespace_and_handle_live() {
    let namespace = PrivateNamespace::open(&name(), Security::CurrentUser).unwrap();
    let beacon = Beacon::open_with("presence", Security::CurrentUser, Some(namespace)).unwrap();
    let publisher = beacon.clone();
    let worker = std::thread::spawn(move || {
        publisher.raise().unwrap();
        // Сам исходный Namespace и этот клон могут исчезнуть до ожидания.
    });
    worker.join().unwrap();
    beacon.wait(finite(), None).unwrap();
    assert!(beacon.is_raised().unwrap());
    assert!(!beacon.clear_if_stale().unwrap()); // живой сервер не гасится
    beacon.lower().unwrap();
    assert!(!beacon.is_raised().unwrap());
}

#[test]
fn beacon_cancellation_is_one_shot_and_independent_of_beacon_reset() {
    let beacon = Beacon::unnamed().unwrap();
    let token = CancellationToken::new().unwrap();
    token.cancel().unwrap();
    assert!(matches!(
        beacon.wait(Wait::Infinite, Some(&token)),
        Err(Error::Cancelled)
    ));
    beacon.raise().unwrap();
    beacon.wait(Wait::Immediate, Some(&token)).unwrap(); // поднятый маяк выигрывает
    beacon.lower().unwrap();
    assert!(matches!(
        beacon.wait(Wait::Immediate, Some(&token)),
        Err(Error::Cancelled)
    ));
}

#[test]
fn multi_copies_messages_reuses_slot_and_rejects_stale_or_foreign_peer() {
    let base = name();
    let mut server = MultiServer::bind(&base, 1, MultiOptions::default()).unwrap();
    let mut buffer = vec![0; 65535];
    let mut stale = None;
    for round in 0..2 {
        let client_name = base.clone();
        let worker = std::thread::spawn(move || {
            Client::connect_multi(
                &client_name,
                MultiClientOptions::default(),
                ConnectBudget::For(Duration::from_secs(5)),
                None,
            )
            .unwrap()
        });
        let peer = match server.next_event(&mut buffer, finite()).unwrap() {
            MultiEvent::Connected(peer) => peer,
            event => panic!("ожидался Connect, получен {event:?}"),
        };
        let (mut client, slot) = worker.join().unwrap();
        assert_eq!(slot, 0);
        assert_eq!(peer.slot(), 0);
        if let Some(old) = &stale {
            assert_ne!(old, &peer);
            assert!(matches!(
                server.try_send(old, b"stale"),
                Err(Error::Disconnected(_))
            ));
            assert!(matches!(
                server.disconnect(old),
                Err(Error::Disconnected(_))
            ));
        }
        let mut foreign = MultiServer::bind(&name(), 1, MultiOptions::default()).unwrap();
        assert!(matches!(
            foreign.try_send(&peer, b"foreign"),
            Err(Error::Disconnected(_))
        ));
        assert!(matches!(
            server.next_event(&mut [], Wait::Immediate),
            Err(Error::InvalidArgument)
        ));
        client.try_send(&[round; 17]).unwrap();
        match server.next_event(&mut buffer, finite()).unwrap() {
            MultiEvent::Message { peer: sender, data } => {
                assert_eq!(sender, peer);
                assert_eq!(data, &[round; 17]);
                // Буфер заимствован отдельно от сервера: отвечаем без копии.
                server.try_send(&sender, data).unwrap();
            }
            event => panic!("ожидалось Message, получено {event:?}"),
        }
        client.wait_readable(finite(), None).unwrap();
        let mut reply = [0; 17];
        assert_eq!(client.receive_into(&mut reply).unwrap(), 17);
        assert_eq!(reply, [round; 17]);
        client.try_send(b"last").unwrap();
        drop(client);
        match server.next_event(&mut buffer, finite()).unwrap() {
            MultiEvent::Message { peer: sender, data } => {
                assert_eq!(sender, peer);
                assert_eq!(data, b"last");
            }
            event => panic!("последний кадр потерян: {event:?}"),
        }
        match server.next_event(&mut buffer, finite()).unwrap() {
            MultiEvent::Disconnected { peer: departed, .. } => assert_eq!(departed, peer),
            event => panic!("ожидалось Disconnected, получено {event:?}"),
        }
        stale = Some(peer);
    }
}

#[test]
fn multi_wake_is_safe_during_and_after_owner_drop() {
    let mut server = MultiServer::bind(&name(), 1, MultiOptions::default()).unwrap();
    let wake = server.wake_handle();
    let sender = wake.clone();
    let worker = std::thread::spawn(move || sender.wake().unwrap());
    let mut buffer = vec![0; 65535];
    assert!(matches!(
        server.next_event(&mut buffer, finite()).unwrap(),
        MultiEvent::Wake
    ));
    worker.join().unwrap();
    let sender = wake.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        sender.wake().unwrap();
        ready_tx.send(()).unwrap();
        loop {
            match sender.wake() {
                Ok(()) => std::thread::yield_now(),
                Err(Error::Disconnected(_)) => break,
                result => panic!("неожиданный результат wake: {result:?}"),
            }
        }
    });
    ready_rx.recv().unwrap();
    drop(server);
    worker.join().unwrap();
    assert!(matches!(wake.wake(), Err(Error::Disconnected(_))));
}

#[test]
fn multi_rejects_invalid_budgets_and_can_cancel_discovery() {
    assert!(matches!(
        MultiServer::bind(&name(), 0, MultiOptions::default()),
        Err(Error::InvalidConfig)
    ));
    assert!(matches!(
        MultiServer::bind(&name(), 257, MultiOptions::default()),
        Err(Error::InvalidConfig)
    ));
    assert!(matches!(
        Client::connect_multi(
            &name(),
            MultiClientOptions::default(),
            ConnectBudget::For(Duration::ZERO),
            None
        ),
        Err(Error::Timeout)
    ));
    let options = MultiClientOptions {
        discovery_fallback: Some(Duration::ZERO),
        ..MultiClientOptions::default()
    };
    assert!(matches!(
        Client::connect_multi(&name(), options, ConnectBudget::Attempt, None),
        Err(Error::InvalidConfig)
    ));
    let cancel = CancellationToken::new().unwrap();
    cancel.cancel().unwrap();
    assert!(matches!(
        Client::connect_multi(
            &name(),
            MultiClientOptions::default(),
            ConnectBudget::Infinite,
            Some(&cancel)
        ),
        Err(Error::Cancelled)
    ));
}
