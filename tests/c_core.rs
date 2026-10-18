#![cfg(feature = "c-core")]
//! Поведенческие проверки безопасной границы над реальным C-ядром.
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use xshm::c_core::{CancellationToken, ChannelConfig, Client, Error, Server, Wait};

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
