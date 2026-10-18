//! Ожидание процессов и доступности/места выполняется только C-ядром.
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use xshm::{
    Beacon, CancellationToken, ChannelConfig, Client, ClientConnectOptions, ConnectBudget, Error,
    ProcessExit, RingConfig, Server, Wait, wait_any,
};

fn name() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "xshm_wait_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
const fn finite() -> Wait {
    Wait::For(Duration::from_secs(5))
}

#[test]
fn process_exit_pins_identity_and_mixed_wait_uses_first_signalled_index() {
    use std::io::Read;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CHILD: &str = "XSHM_C_CORE_EXIT_WAIT_TEST_CHILD";
    if std::env::var_os(CHILD).as_deref() == Some(std::ffi::OsStr::new("1")) {
        let mut input = Vec::new();
        std::io::stdin().read_to_end(&mut input).unwrap();
        return;
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "process_exit_pins_identity_and_mixed_wait_uses_first_signalled_index",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x08000000) // CREATE_NO_WINDOW: тест без отдельного окна
        .spawn()
        .unwrap();
    let process = ProcessExit::open(child.id()).unwrap();
    let retained = process.clone();
    drop(process);
    assert!(matches!(
        retained.wait(Wait::Immediate),
        Err(Error::Timeout)
    ));
    let beacon = Beacon::unnamed().unwrap();
    assert!(matches!(
        wait_any(&[&beacon, &retained], Wait::Immediate),
        Err(Error::Timeout)
    ));
    drop(child.stdin.take()); // EOF завершает именно этот дочерний процесс
    assert!(child.wait().unwrap().success());
    assert_eq!(wait_any(&[&beacon, &retained], finite()).unwrap(), 1);
    beacon.raise().unwrap();
    assert_eq!(wait_any(&[&beacon, &retained], Wait::Immediate).unwrap(), 0);
    let cancel = CancellationToken::new().unwrap();
    cancel.cancel().unwrap();
    assert_eq!(
        wait_any(&[&cancel, &beacon, &retained], Wait::Immediate).unwrap(),
        0
    );
    assert!(matches!(ProcessExit::open(0), Err(Error::InvalidArgument)));
    assert!(matches!(
        wait_any(&[], Wait::Immediate),
        Err(Error::Timeout)
    ));
    let object: &dyn std::os::windows::io::AsHandle = &beacon;
    let too_many = vec![object; 65];
    assert!(matches!(
        wait_any(&too_many, Wait::Immediate),
        Err(Error::InvalidArgument)
    ));
}

#[test]
fn single_availability_can_start_before_server_and_is_cancellable() {
    let name = name();
    let client_name = name.clone();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let started = barrier.clone();
    let worker = std::thread::spawn(move || {
        started.wait();
        Client::connect_when_available(
            &client_name,
            ClientConnectOptions {
                discovery_fallback: None,
                ..ClientConnectOptions::default()
            },
            ConnectBudget::Infinite,
            None,
        )
        .unwrap()
    });
    barrier.wait();
    let mut server = Server::bind(&name, ChannelConfig::default()).unwrap();
    let mut session = server.accept(finite(), None).unwrap();
    let mut client = worker.join().unwrap();
    assert!(client.is_connected());
    assert!(session.is_connected());
    assert_eq!(client.peer_pid(), Some(std::process::id()));
    assert_eq!(session.peer_pid(), Some(std::process::id()));
    assert_eq!(client.layout().unwrap().c2s.max_message_size, 65535);
    let cancel = CancellationToken::new().unwrap();
    cancel.cancel().unwrap();
    assert!(matches!(
        Client::connect_when_available(
            &name,
            ClientConnectOptions::default(),
            ConnectBudget::Infinite,
            Some(&cancel)
        ),
        Err(Error::Cancelled)
    ));
    assert!(matches!(
        Client::connect_when_available(
            &name,
            ClientConnectOptions::default(),
            ConnectBudget::For(Duration::ZERO),
            None
        ),
        Err(Error::Timeout)
    ));
}

#[test]
fn single_wait_for_space_preserves_frame_and_resumes_after_peer_reads() {
    let name = name();
    let mut config = ChannelConfig::default();
    config.layout.c2s = RingConfig {
        capacity: 4096,
        max_messages: 1,
        max_message_size: 1024,
    };
    let mut server = Server::bind(&name, config).unwrap();
    let (read_tx, read_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut session = server.accept(finite(), None).unwrap();
        read_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut bytes = [0; 4];
        for expected in [b"one!", b"two!"] {
            session.wait_readable(finite(), None).unwrap();
            assert_eq!(session.receive_into(&mut bytes).unwrap(), bytes.len());
            assert_eq!(&bytes, expected);
        }
    });
    let mut client = Client::connect(&name, finite(), None).unwrap();
    client.try_send(b"one!").unwrap();
    assert!(matches!(client.try_send(b"two!"), Err(Error::Full)));
    assert!(matches!(
        client.wait_for_space(4, Wait::Immediate, None),
        Err(Error::Timeout)
    ));
    read_tx.send(()).unwrap();
    client.wait_for_space(4, finite(), None).unwrap();
    client.try_send(b"two!").unwrap();
    worker.join().unwrap();
}
