#![cfg(feature = "c-core")]
//! Требования реального профайлера: ранний хук, владение политикой и обнаружение.
use std::cell::Cell;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;
use xshm::c_core::{
    Client, ClientEvent, ClientWorker, DispatchClientOptions, DispatchEvent, DispatchOptions,
    DispatchWorker, Error, QueueLimits, Security, lobby_exists,
};
thread_local! { static MARKED: Cell<bool> = const { Cell::new(false) }; }
fn mark() {
    MARKED.set(true);
}
fn fail_hook() {
    panic!("проверка ошибки хука");
}
fn name() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "xshm_lifecycle_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
fn connect(name: &str) -> xshm::c_core::Result<Client> {
    Client::connect_dispatch(name, 1, "profiler", DispatchClientOptions::default(), None)
        .map(|result| result.0)
}
#[test]
fn both_workers_run_creator_hook_before_callbacks_and_restore_no_global_policy() {
    let name = name();
    xshm::set_thread_start_hook(Some(mark));
    let (tx, rx) = mpsc::channel();
    let mut server = DispatchWorker::start(
        &name,
        DispatchOptions::default(),
        QueueLimits::default(),
        move |_, event| {
            if let DispatchEvent::Connected { .. } = event {
                tx.send(MARKED.get()).unwrap();
            }
        },
    )
    .unwrap();
    let client = connect(&name).unwrap();
    let (tx, client_rx) = mpsc::channel();
    let mut worker = ClientWorker::spawn(client, QueueLimits::default(), move |_, event| {
        if let ClientEvent::Connected { .. } = event {
            tx.send(MARKED.get()).unwrap();
        }
    })
    .unwrap();
    xshm::set_thread_start_hook(None);
    assert!(rx.recv_timeout(Duration::from_secs(5)).unwrap());
    assert!(client_rx.recv_timeout(Duration::from_secs(5)).unwrap());
    worker.stop().unwrap();
    server.stop().unwrap();
    assert!(!MARKED.get(), "хук меняет только дочерний поток");
}
#[test]
fn startup_hook_panic_is_reported_and_does_not_leave_lobby_or_session() {
    let name = name();
    xshm::set_thread_start_hook(Some(fail_hook));
    let result = DispatchWorker::start(
        &name,
        DispatchOptions::default(),
        QueueLimits::default(),
        |_, _| {},
    );
    xshm::set_thread_start_hook(None);
    assert!(matches!(result, Err(Error::WorkerPanicked)));
    assert!(!lobby_exists(&name, None).unwrap());
    let mut server = DispatchWorker::start(
        &name,
        DispatchOptions::default(),
        QueueLimits::default(),
        |_, _| {},
    )
    .unwrap();
    let client = connect(&name).unwrap();
    xshm::set_thread_start_hook(Some(fail_hook));
    let result = ClientWorker::spawn(client, QueueLimits::default(), |_, _| {});
    xshm::set_thread_start_hook(None);
    assert!(matches!(result, Err(Error::WorkerPanicked)));
    // Следующий клиент регистрируется после освобождения предыдущей сессии.
    drop(connect(&name).unwrap());
    server.stop().unwrap();
    assert!(!lobby_exists(&name, None).unwrap());
}
#[test]
fn dispatch_copies_custom_descriptor_before_future_channel_creation() {
    let name = name();
    let mut bytes = vec![0; 20];
    bytes[0] = 1;
    bytes[2] = 4;
    bytes[3] = 0x80;
    let descriptor: Arc<[u8]> = bytes.into();
    let weak = Arc::downgrade(&descriptor);
    let mut options = DispatchOptions::default();
    options.channel.security = Security::Descriptor(descriptor);
    let (tx, rx) = mpsc::channel();
    let mut server =
        DispatchWorker::start(&name, options, QueueLimits::default(), move |_, event| {
            if let DispatchEvent::Connected { .. } = event {
                tx.send(()).unwrap();
            }
        })
        .unwrap();
    assert!(
        weak.upgrade().is_none(),
        "Rust-источник политики уже освобождён"
    );
    assert!(lobby_exists(&name, None).unwrap());
    drop(connect(&name).unwrap());
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    server.stop().unwrap();
    assert!(!lobby_exists(&name, None).unwrap());
}
#[test]
fn malformed_descriptor_and_nul_lobby_are_errors() {
    let mut options = DispatchOptions::default();
    options.channel.security = Security::Descriptor(Arc::from([0u8; 3]));
    assert!(matches!(
        DispatchWorker::start(&name(), options, QueueLimits::default(), |_, _| {}),
        Err(Error::InvalidConfig)
    ));
    assert!(matches!(
        lobby_exists("bad\0name", None),
        Err(Error::NameContainsNul(_))
    ));
}

#[test]
fn completion_timeout_preserves_command_and_rejects_wait_from_callback() {
    let name = name();
    let (peer_tx, peer_rx) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let (self_tx, self_rx) = mpsc::channel();
    let mut server = DispatchWorker::start(
        &name,
        DispatchOptions::default(),
        QueueLimits::default(),
        move |sender, event| {
            if let DispatchEvent::Connected { peer, .. } = event {
                peer_tx.send(peer.clone()).unwrap();
                gate.recv_timeout(Duration::from_secs(5)).unwrap();
                let completion = sender.submit_try_send(&peer, b"second").unwrap();
                self_tx
                    .send(matches!(
                        completion.wait_timeout(Duration::ZERO),
                        Err(Error::WouldDeadlock)
                    ))
                    .unwrap();
            }
        },
    )
    .unwrap();
    let mut client = connect(&name).unwrap();
    let peer = peer_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let completion = server.sender().submit_try_send(&peer, b"first").unwrap();
    assert!(matches!(
        completion.wait_timeout(Duration::ZERO),
        Err(Error::Timeout)
    ));
    release.send(()).unwrap();
    assert!(self_rx.recv_timeout(Duration::from_secs(5)).unwrap());
    let mut bytes = [0; 16];
    for expected in [b"first".as_slice(), b"second".as_slice()] {
        client
            .wait_readable(xshm::c_core::Wait::For(Duration::from_secs(5)), None)
            .unwrap();
        let len = client.receive_into(&mut bytes).unwrap();
        assert_eq!(&bytes[..len], expected);
    }
    server.stop().unwrap();
}
