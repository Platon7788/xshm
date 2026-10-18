#![cfg(feature = "c-core")]
//! Реальное C-лобби, выделенные каналы и граница admission callback.
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Duration;
use xshm::c_core::{
    Client, DispatchClientOptions, DispatchEvent, DispatchOptions, DispatchServer, Error,
    PrivateNamespace, Security, Wait,
};

fn name() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "xshm_dispatch_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
const fn finite() -> Wait {
    Wait::For(Duration::from_secs(5))
}
fn options() -> DispatchOptions {
    DispatchOptions {
        max_clients: 1,
        ..DispatchOptions::default()
    }
}

#[test]
fn dispatch_owns_registration_and_namespace_drains_last_frame_and_rejects_old_peers() {
    let namespace = PrivateNamespace::open(&name(), Security::CurrentUser).unwrap();
    let mut opts = options();
    opts.channel.namespace = Some(namespace.clone());
    let mut server = DispatchServer::bind("lobby", opts).unwrap();
    let client_namespace = namespace.clone();
    drop(namespace);
    let mut buffer = vec![0; 65535];
    let mut old = None;
    for round in 0..2 {
        let client_namespace = client_namespace.clone();
        let worker = std::thread::spawn(move || {
            Client::connect_dispatch(
                "lobby",
                7,
                &"я".repeat(40),
                DispatchClientOptions {
                    namespace: Some(client_namespace),
                    ..DispatchClientOptions::default()
                },
                None,
            )
            .unwrap()
        });
        let (peer, registration) = match server.next_event(&mut buffer, finite()).unwrap() {
            DispatchEvent::Connected { peer, registration } => (peer, registration),
            event => panic!("ожидался Connect: {event:?}"),
        };
        assert_eq!(registration.pid, std::process::id());
        assert_eq!(registration.revision, 7);
        assert_eq!(registration.name, "я".repeat(32));
        let (mut client, info) = worker.join().unwrap();
        assert_eq!(info.client_id, peer.client_id());
        assert!(!info.channel_name.is_empty());
        if let Some(old) = &old {
            assert_ne!(old, &peer);
            assert!(matches!(
                server.try_send(old, b"old"),
                Err(Error::Disconnected(_))
            ));
        }
        let mut foreign = DispatchServer::bind(&name(), options()).unwrap();
        assert!(matches!(
            foreign.try_send(&peer, b"foreign"),
            Err(Error::Disconnected(_))
        ));
        assert!(matches!(
            server.next_event(&mut [], Wait::Immediate),
            Err(Error::InvalidArgument)
        ));
        client.try_send(&[round; 9]).unwrap();
        match server.next_event(&mut buffer, finite()).unwrap() {
            DispatchEvent::Message {
                peer: sender,
                data,
                registration: metadata,
            } => {
                assert_eq!(metadata, registration);
                assert!(Arc::ptr_eq(&metadata, &registration));
                assert_eq!(sender, peer);
                assert_eq!(data, &[round; 9]);
                server.try_send(&sender, data).unwrap();
            }
            event => panic!("ожидалось Message: {event:?}"),
        }
        client.wait_readable(finite(), None).unwrap();
        let mut reply = [0; 9];
        assert_eq!(client.receive_into(&mut reply).unwrap(), reply.len());
        assert_eq!(reply, [round; 9]);
        client.try_send(b"last").unwrap();
        drop(client);
        assert!(matches!(
            server.next_event(&mut buffer, finite()).unwrap(),
            DispatchEvent::Message { data: b"last", .. }
        ));
        match server.next_event(&mut buffer, finite()).unwrap() {
            DispatchEvent::Disconnected {
                peer: departed,
                registration: metadata,
                ..
            } => {
                assert_eq!(departed, peer);
                assert_eq!(metadata, registration);
                assert!(Arc::ptr_eq(&metadata, &registration));
            }
            event => panic!("ожидалось Disconnected: {event:?}"),
        }
        // Регистрация не заимствовала reg C-события, пережила следующие next.
        assert_eq!(registration.name, "я".repeat(32));
        old = Some(peer);
    }
}

#[test]
fn dispatch_admission_rejects_then_accepts_without_breaking_lobby() {
    let lobby = name();
    let rejected = Arc::new(AtomicBool::new(false));
    let rejected_flag = rejected.clone();
    let mut server = DispatchServer::bind_with_admission(&lobby, options(), move |reg| {
        let allowed = reg.revision == 7;
        if !allowed {
            rejected_flag.store(true, Ordering::Release);
        }
        allowed
    })
    .unwrap();
    let worker = std::thread::spawn(move || {
        assert!(matches!(
            Client::connect_dispatch(&lobby, 6, "reject", DispatchClientOptions::default(), None),
            Err(Error::Rejected)
        ));
        Client::connect_dispatch(&lobby, 7, "accept", DispatchClientOptions::default(), None)
            .unwrap()
    });
    let mut buffer = vec![0; 65535];
    let peer = match server.next_event(&mut buffer, finite()).unwrap() {
        DispatchEvent::Connected { peer, .. } => peer,
        event => panic!("отказ повредил лобби: {event:?}"),
    };
    assert!(rejected.load(Ordering::Acquire));
    let (mut client, _) = worker.join().unwrap();
    server.disconnect(&peer).unwrap();
    assert!(matches!(
        client.wait_readable(finite(), None),
        Err(Error::Disconnected(_))
    ));
    assert!(matches!(
        server.next_event(&mut buffer, finite()).unwrap(),
        DispatchEvent::Disconnected { .. }
    ));
    assert!(matches!(
        server.try_send(&peer, b"closed"),
        Err(Error::Disconnected(_))
    ));
}

#[test]
#[cfg(panic = "unwind")]
fn dispatch_admission_panic_cannot_unwind_through_c_or_strand_infinite_next() {
    let lobby = name();
    let server_lobby = lobby.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut server = DispatchServer::bind_with_admission(&server_lobby, options(), |_| {
            panic!("проверка границы C callback")
        })
        .unwrap();
        ready_tx.send(server.wake_handle()).unwrap();
        let mut buffer = vec![0; 65535];
        assert!(matches!(
            server.next_event(&mut buffer, Wait::Infinite),
            Err(Error::AdmissionPanicked)
        ));
        // Отравленный обработчик больше не вызывается, ошибка сохраняется.
        assert!(matches!(
            server.next_event(&mut buffer, Wait::Immediate),
            Err(Error::AdmissionPanicked)
        ));
        done_tx.send(()).unwrap();
    });
    let wake = ready_rx.recv().unwrap();
    assert!(matches!(
        Client::connect_dispatch(&lobby, 7, "panic", DispatchClientOptions::default(), None),
        Err(Error::Rejected)
    ));
    let completed = done_rx.recv_timeout(Duration::from_secs(3)).is_ok();
    if !completed {
        let _cleanup = wake.wake();
    }
    worker.join().unwrap();
    assert!(
        completed,
        "Infinite next не вернул ошибку callback без внешнего wake"
    );
    assert!(matches!(wake.wake(), Err(Error::Disconnected(_))));
}

#[test]
fn dispatch_wake_is_safe_after_owner_drop_and_failed_creation_keeps_first_lobby() {
    let lobby = name();
    let mut server = DispatchServer::bind(&lobby, options()).unwrap();
    assert!(matches!(
        DispatchServer::bind(&lobby, options()),
        Err(Error::NameCollision)
    ));
    let wake = server.wake_handle();
    let sender = wake.clone();
    let worker = std::thread::spawn(move || sender.wake().unwrap());
    let mut buffer = vec![0; 65535];
    assert!(matches!(
        server.next_event(&mut buffer, finite()).unwrap(),
        DispatchEvent::Wake
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
                result => panic!("неожиданный wake: {result:?}"),
            }
        }
    });
    ready_rx.recv().unwrap();
    drop(server);
    worker.join().unwrap();
    assert!(matches!(wake.wake(), Err(Error::Disconnected(_))));
    drop(DispatchServer::bind(&lobby, options()).unwrap());
}

#[test]
fn dispatch_validates_rust_inputs_before_entering_c() {
    let opts = DispatchOptions {
        max_clients: 257,
        ..options()
    };
    assert!(matches!(
        DispatchServer::bind(&name(), opts),
        Err(Error::InvalidConfig)
    ));
    let opts = DispatchOptions {
        lobby_timeout: Duration::MAX,
        ..options()
    };
    assert!(matches!(
        DispatchServer::bind(&name(), opts),
        Err(Error::TimeoutTooLarge)
    ));
    assert!(matches!(
        Client::connect_dispatch("lobby", 7, "a\0b", DispatchClientOptions::default(), None),
        Err(Error::NameContainsNul(_))
    ));
    let opts = DispatchClientOptions {
        channel_timeout: Wait::For(Duration::MAX),
        ..DispatchClientOptions::default()
    };
    assert!(matches!(
        Client::connect_dispatch("lobby", 7, "name", opts, None),
        Err(Error::TimeoutTooLarge)
    ));
}
