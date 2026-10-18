//! Один C-владелец принимает данные, даже когда исходящее кольцо заполнено.
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;
use xshm::{
    ChannelConfig, Client, ClientEvent, ClientWorker, Error, QueueLimits, Result, RingConfig,
    Server, Wait,
};

const fn finite() -> Wait {
    Wait::For(Duration::from_secs(5))
}
fn listener(messages: u32) -> Result<(Server, JoinHandle<Result<Client>>)> {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let name = format!(
        "xshm_client_worker_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let mut config = ChannelConfig::default();
    let ring = RingConfig {
        capacity: 4096,
        max_messages: messages,
        max_message_size: 1024,
    };
    config.layout.s2c = ring;
    config.layout.c2s = ring;
    let server = Server::bind(&name, config)?;
    let client = std::thread::spawn(move || Client::connect(&name, finite(), None));
    Ok((server, client))
}

#[test]
fn client_worker_receives_while_lossless_head_waits_and_bounds_inflight_payload() {
    let (mut server, connecting) = listener(1).unwrap();
    let mut session = server.accept(finite(), None).unwrap();
    let mut client = connecting.join().unwrap().unwrap();
    client.try_send(b"full").unwrap();
    let (received_tx, received_rx) = mpsc::channel();
    let mut worker = ClientWorker::spawn(
        client,
        QueueLimits {
            commands: 1,
            bytes: 4,
        },
        move |_, event| {
            if let ClientEvent::Message { data } = event {
                received_tx.send(data.to_vec()).unwrap();
            }
        },
    )
    .unwrap();
    let sender = worker.sender();
    let pending = sender.submit_lossless(b"next").unwrap();
    session.try_send(b"ping").unwrap();
    assert_eq!(
        received_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        b"ping"
    );
    assert!(pending.try_result().unwrap().is_none());
    assert!(matches!(sender.submit_lossless(b"more"), Err(Error::Full)));
    let mut bytes = [0; 4];
    assert_eq!(session.receive_into(&mut bytes).unwrap(), 4);
    assert_eq!(&bytes, b"full");
    pending.wait().unwrap();
    session.wait_readable(finite(), None).unwrap();
    assert_eq!(session.receive_into(&mut bytes).unwrap(), 4);
    assert_eq!(&bytes, b"next");
    sender.submit_lossless(b"done").unwrap().wait().unwrap();
    assert_eq!(session.receive_into(&mut bytes).unwrap(), 4);
    assert_eq!(&bytes, b"done");
    worker.stop().unwrap();
    assert!(matches!(
        sender.submit_lossless(b"late"),
        Err(Error::WorkerStopped)
    ));
}

#[test]
fn client_callback_gets_sender_immediately_and_frames_precede_peer_disconnect() {
    let (mut server, connecting) = listener(8).unwrap();
    let mut session = server.accept(finite(), None).unwrap();
    let client = connecting.join().unwrap().unwrap();
    let (completion_tx, completion_rx) = mpsc::channel();
    let (event_tx, event_rx) = mpsc::channel();
    let mut worker =
        ClientWorker::spawn(
            client,
            QueueLimits::default(),
            move |sender, event| match event {
                ClientEvent::Connected { .. } => {
                    completion_tx
                        .send(sender.submit_lossless(b"init").unwrap())
                        .unwrap();
                }
                ClientEvent::Message { data } => event_tx.send(data.to_vec()).unwrap(),
                ClientEvent::Disconnected { .. } => event_tx.send(b"closed".to_vec()).unwrap(),
                _ => {}
            },
        )
        .unwrap();
    completion_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .wait()
        .unwrap();
    session.wait_readable(finite(), None).unwrap();
    let mut bytes = [0; 4];
    assert_eq!(session.receive_into(&mut bytes).unwrap(), 4);
    assert_eq!(&bytes, b"init");
    session.try_send(b"one!").unwrap();
    session.try_send(b"last").unwrap();
    drop(session);
    for expected in [b"one!".as_slice(), b"last", b"closed"] {
        assert_eq!(
            event_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            expected
        );
    }
    worker.stop().unwrap();
}

#[test]
fn client_stop_interrupts_full_tx_and_cancels_lossless_pending_command() {
    let (mut server, connecting) = listener(1).unwrap();
    let mut session = server.accept(finite(), None).unwrap();
    let mut client = connecting.join().unwrap().unwrap();
    client.try_send(b"full").unwrap();
    let mut worker = ClientWorker::spawn(client, QueueLimits::default(), |_, _| {}).unwrap();
    let pending = worker.sender().submit_lossless(b"next").unwrap();
    worker.stop().unwrap();
    worker.stop().unwrap();
    assert!(matches!(pending.wait(), Err(Error::WorkerStopped)));
    let mut bytes = [0; 4];
    session.wait_readable(finite(), None).unwrap();
    assert_eq!(session.receive_into(&mut bytes).unwrap(), 4);
    assert_eq!(&bytes, b"full");
    assert!(matches!(
        session.wait_readable(finite(), None),
        Err(Error::Disconnected(_))
    ));
}

#[test]
fn client_try_send_reports_full_then_one_writable_notification() {
    let (mut server, connecting) = listener(1).unwrap();
    let mut session = server.accept(finite(), None).unwrap();
    let mut client = connecting.join().unwrap().unwrap();
    client.try_send(b"full").unwrap();
    let (writable_tx, writable_rx) = mpsc::channel();
    let mut worker = ClientWorker::spawn(client, QueueLimits::default(), move |_, event| {
        if let ClientEvent::Writable { payload_len } = event {
            writable_tx.send(payload_len).unwrap();
        }
    })
    .unwrap();
    assert!(matches!(
        worker.sender().submit_try_send(b"lost").unwrap().wait(),
        Err(Error::Full)
    ));
    let mut bytes = [0; 4];
    assert_eq!(session.receive_into(&mut bytes).unwrap(), 4);
    assert_eq!(&bytes, b"full");
    assert_eq!(writable_rx.recv_timeout(Duration::from_secs(5)).unwrap(), 4);
    worker
        .sender()
        .submit_try_send(b"next")
        .unwrap()
        .wait()
        .unwrap();
    worker.stop().unwrap();
    assert!(matches!(
        writable_rx.try_recv(),
        Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected)
    ));
}

#[test]
fn client_self_stop_does_not_invoke_further_callbacks_from_the_same_batch() {
    let (mut server, connecting) = listener(8).unwrap();
    let mut session = server.accept(finite(), None).unwrap();
    let client = connecting.join().unwrap().unwrap();
    for bytes in [b"one!", b"two!", b"last"] {
        session.try_send(bytes).unwrap();
    }
    let messages = Arc::new(AtomicU32::new(0));
    let observed = messages.clone();
    let disconnected = Arc::new(AtomicU32::new(0));
    let ends = disconnected.clone();
    let mut worker =
        ClientWorker::spawn(
            client,
            QueueLimits::default(),
            move |sender, event| match event {
                ClientEvent::Message { .. } => {
                    observed.fetch_add(1, Ordering::Relaxed);
                    sender.request_stop().unwrap();
                }
                ClientEvent::Disconnected { .. } => {
                    ends.fetch_add(1, Ordering::Relaxed);
                }
                _ => {}
            },
        )
        .unwrap();
    assert!(matches!(
        session.wait_readable(finite(), None),
        Err(Error::Disconnected(_))
    ));
    worker.stop().unwrap();
    assert_eq!(messages.load(Ordering::Relaxed), 1);
    assert_eq!(disconnected.load(Ordering::Relaxed), 0);
}

#[test]
fn client_lossless_queue_delivers_fifo_without_moving_into_a_new_session() {
    let (mut server, connecting) = listener(1).unwrap();
    let mut session = server.accept(finite(), None).unwrap();
    let client = connecting.join().unwrap().unwrap();
    let mut worker = ClientWorker::spawn(
        client,
        QueueLimits {
            commands: 8,
            bytes: 32,
        },
        |_, _| {},
    )
    .unwrap();
    let sender = worker.sender();
    let completions: Vec<_> = (0_u32..8)
        .map(|n| sender.submit_lossless(&n.to_le_bytes()).unwrap())
        .collect();
    let mut bytes = [0; 4];
    for (n, completion) in (0_u32..8).zip(completions) {
        session.wait_readable(finite(), None).unwrap();
        assert_eq!(session.receive_into(&mut bytes).unwrap(), 4);
        assert_eq!(u32::from_le_bytes(bytes), n);
        completion.wait().unwrap();
    }
    worker.stop().unwrap();
    drop(session);
    assert!(matches!(
        sender.submit_lossless(b"old!"),
        Err(Error::WorkerStopped)
    ));
}

#[test]
#[cfg(panic = "unwind")]
fn client_callback_panic_is_joined_and_reported() {
    let (mut server, connecting) = listener(1).unwrap();
    let mut session = server.accept(finite(), None).unwrap();
    let client = connecting.join().unwrap().unwrap();
    let mut worker = ClientWorker::spawn(client, QueueLimits::default(), |_, event| {
        if matches!(event, ClientEvent::Message { .. }) {
            panic!("ошибка обработчика клиента");
        }
    })
    .unwrap();
    session.try_send(b"test").unwrap();
    assert!(matches!(
        session.wait_readable(finite(), None),
        Err(Error::Disconnected(_))
    ));
    assert!(
        matches!(worker.stop(), Err(Error::WorkerFailed(error)) if matches!(error.as_ref(), Error::WorkerPanicked))
    );
}

#[test]
fn client_owner_can_be_dropped_from_its_callback_without_self_join() {
    let (mut server, connecting) = listener(1).unwrap();
    let mut session = server.accept(finite(), None).unwrap();
    let client = connecting.join().unwrap().unwrap();
    let owner = Arc::new(std::sync::Mutex::new(None::<ClientWorker>));
    let callback_owner = owner.clone();
    let (dropped_tx, dropped_rx) = mpsc::channel();
    let worker = ClientWorker::spawn(client, QueueLimits::default(), move |_, event| {
        if matches!(event, ClientEvent::Message { .. }) {
            let worker = callback_owner.lock().unwrap().take().unwrap();
            drop(worker);
            dropped_tx.send(()).unwrap();
        }
    })
    .unwrap();
    let sender = worker.sender();
    *owner.lock().unwrap() = Some(worker);
    session.try_send(b"drop").unwrap();
    dropped_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(
        session.wait_readable(finite(), None),
        Err(Error::Disconnected(_))
    ));
    assert!(owner.lock().unwrap().is_none());
    assert!(matches!(
        sender.submit_lossless(b"late"),
        Err(Error::WorkerStopped)
    ));
}

/// Отказ отправки не закрывает очередь раньше callback последнего входящего
/// кадра. Иначе приложение могло потерять BYE, отреагировав на WorkerStopped.
#[test]
fn peer_close_during_send_drains_last_callback_before_publishing_queue_close() {
    let (mut server, connecting) = listener(8).unwrap();
    let mut session = server.accept(finite(), None).unwrap();
    let client = connecting.join().unwrap().unwrap();
    let (completion_tx, completion_rx) = mpsc::channel();
    let (message_tx, message_rx) = mpsc::channel();
    let (end_tx, end_rx) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let mut worker =
        ClientWorker::spawn(
            client,
            QueueLimits::default(),
            move |sender, event| match event {
                ClientEvent::Connected { .. } => {
                    completion_tx
                        .send(sender.submit_try_send(b"late").unwrap())
                        .unwrap();
                    gate.recv_timeout(Duration::from_secs(5)).unwrap();
                }
                ClientEvent::Message { data } => {
                    message_tx.send(data.to_vec()).unwrap();
                    gate.recv_timeout(Duration::from_secs(5)).unwrap();
                }
                ClientEvent::Disconnected { .. } => end_tx.send(()).unwrap(),
                _ => {}
            },
        )
        .unwrap();
    let completion = completion_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    session.try_send(b"BYE!").unwrap();
    drop(session);
    release.send(()).unwrap();
    assert!(matches!(completion.wait(), Err(Error::Disconnected(_))));
    assert_eq!(
        message_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        b"BYE!"
    );
    let closed_before_callback_return = worker.sender().is_stopped();
    let pending = worker.sender().submit_lossless(b"queued");
    release.send(()).unwrap();
    end_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    worker.stop().unwrap();
    assert!(
        !closed_before_callback_return,
        "очередь не опережает доставку BYE"
    );
    assert!(matches!(pending.unwrap().wait(), Err(Error::WorkerStopped)));
}
