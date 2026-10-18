#![cfg(feature = "c-core")]
//! Типизированная граница комбинированного ожидания C, без повторения протокола.
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use xshm::c_core::{CancellationToken, ChannelConfig, Client, Error, RingConfig, Server, Wait};

#[test]
fn activity_reports_both_directions_and_does_not_block_receive_on_full_tx() {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let name = format!(
        "xshm_activity_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let mut config = ChannelConfig::default();
    let ring = RingConfig {
        capacity: 4096,
        max_messages: 1,
        max_message_size: 1024,
    };
    config.layout.s2c = ring;
    config.layout.c2s = ring;
    let mut server = Server::bind(&name, config).unwrap();
    let worker = std::thread::spawn(move || {
        Client::connect(&name, Wait::For(Duration::from_secs(5)), None).unwrap()
    });
    let mut session = server
        .accept(Wait::For(Duration::from_secs(5)), None)
        .unwrap();
    let mut client = worker.join().unwrap();
    let ready = client
        .wait_activity(Some(4), Wait::Immediate, None)
        .unwrap();
    assert!(ready.writable() && !ready.readable());
    session.try_send(b"data").unwrap();
    let ready = client
        .wait_activity(Some(4), Wait::Immediate, None)
        .unwrap();
    assert!(ready.writable() && ready.readable());
    let mut bytes = [0; 4];
    assert_eq!(client.receive_into(&mut bytes).unwrap(), 4);
    assert_eq!(&bytes, b"data");
    client.try_send(b"full").unwrap();
    assert!(matches!(client.try_send(b"lost"), Err(Error::Full)));
    let cancel = CancellationToken::new().unwrap();
    cancel.cancel().unwrap();
    assert!(matches!(
        client.wait_activity(Some(4), Wait::Infinite, Some(&cancel)),
        Err(Error::Cancelled)
    ));
    session.try_send(b"read").unwrap();
    let ready = client
        .wait_activity(Some(4), Wait::Infinite, Some(&cancel))
        .unwrap();
    assert!(ready.readable() && !ready.writable()); // готовые данные выигрывают отмену
    assert_eq!(client.receive_into(&mut bytes).unwrap(), 4);
    assert_eq!(&bytes, b"read");
    let ready = session
        .wait_activity(Some(4), Wait::Immediate, None)
        .unwrap();
    assert!(ready.readable() && ready.writable());
    assert_eq!(session.receive_into(&mut bytes).unwrap(), 4);
    assert_eq!(&bytes, b"full");
    let ready = client
        .wait_activity(Some(4), Wait::Immediate, None)
        .unwrap();
    assert!(ready.writable() && !ready.readable());
    assert!(matches!(
        client.wait_activity(Some(0), Wait::Immediate, None),
        Err(Error::MessageTooSmall)
    ));
    assert!(matches!(
        client.wait_activity(Some(1025), Wait::Immediate, None),
        Err(Error::MessageTooLarge)
    ));
    assert!(matches!(
        client.wait_activity(None, Wait::For(Duration::MAX), None),
        Err(Error::TimeoutTooLarge)
    ));
    assert!(matches!(
        client.wait_activity(None, Wait::Immediate, None),
        Err(Error::Timeout)
    ));
}
