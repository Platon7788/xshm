#![cfg(feature = "c-core")]
//! Очередь команд и stop/join проверяются с настоящим C-диспетчером.
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::Duration;
use xshm::c_core::{
    Client, DispatchClientOptions, DispatchEvent, DispatchOptions, DispatchSender, DispatchWorker,
    Error, QueueLimits, Wait,
};

fn name() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "xshm_worker_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
fn options() -> DispatchOptions {
    DispatchOptions {
        max_clients: 1,
        ..DispatchOptions::default()
    }
}
const fn finite() -> Wait {
    Wait::For(Duration::from_secs(5))
}
fn connect(lobby: &str) -> xshm::c_core::Result<Client> {
    Client::connect_dispatch(lobby, 7, "worker", DispatchClientOptions::default(), None)
        .map(|(client, _)| client)
}

#[test]
fn worker_confirms_delivery_and_callback_can_submit_without_self_deadlock() {
    let lobby = name();
    let (connected_tx, connected_rx) = mpsc::channel();
    let (callback_tx, callback_rx) = mpsc::channel();
    let sender = Arc::new(OnceLock::<DispatchSender>::new());
    let callback_sender = sender.clone();
    let mut worker = DispatchWorker::start(
        &lobby,
        options(),
        QueueLimits::default(),
        move |event| match event {
            DispatchEvent::Connected { peer, .. } => connected_tx.send(peer).unwrap(),
            DispatchEvent::Message { peer, data, .. } => {
                let completion = callback_sender
                    .get()
                    .unwrap()
                    .submit_try_send(&peer, data)
                    .unwrap();
                assert!(matches!(completion.wait(), Err(Error::WouldDeadlock)));
                callback_tx.send(()).unwrap();
            }
            _ => {}
        },
    )
    .unwrap();
    sender.set(worker.sender()).unwrap();
    let mut client = connect(&lobby).unwrap();
    let peer = connected_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    sender
        .get()
        .unwrap()
        .submit_try_send(&peer, b"push")
        .unwrap()
        .wait()
        .unwrap();
    client.wait_readable(finite(), None).unwrap();
    let mut buffer = [0; 4];
    assert_eq!(client.receive_into(&mut buffer).unwrap(), 4);
    assert_eq!(&buffer, b"push");
    client.try_send(b"echo").unwrap();
    callback_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    client.wait_readable(finite(), None).unwrap();
    assert_eq!(client.receive_into(&mut buffer).unwrap(), 4);
    assert_eq!(&buffer, b"echo");
    sender
        .get()
        .unwrap()
        .submit_disconnect(&peer)
        .unwrap()
        .wait()
        .unwrap();
    assert!(matches!(
        client.wait_readable(finite(), None),
        Err(Error::Disconnected(_))
    ));
    worker.stop().unwrap();
    worker.stop().unwrap();
    assert!(worker.is_stopped());
    assert!(matches!(
        sender.get().unwrap().submit_try_send(&peer, b"late"),
        Err(Error::WorkerStopped)
    ));
}

#[test]
fn worker_queue_is_bounded_and_stop_cancels_pending_sends_without_overwriting() {
    let lobby = name();
    let (connected_tx, connected_rx) = mpsc::channel();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let mut worker = DispatchWorker::start(
        &lobby,
        options(),
        QueueLimits {
            commands: 2,
            bytes: 4,
        },
        move |event| match event {
            DispatchEvent::Connected { peer, .. } => connected_tx.send(peer).unwrap(),
            DispatchEvent::Message { .. } => {
                entered_tx.send(()).unwrap();
                resume_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            _ => {}
        },
    )
    .unwrap();
    let mut client = connect(&lobby).unwrap();
    let peer = connected_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    client.try_send(b"block").unwrap();
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let sender = worker.sender();
    assert!(matches!(
        sender.submit_try_send(&peer, b"large"),
        Err(Error::Full)
    ));
    let first = sender.submit_try_send(&peer, b"aa").unwrap();
    let second = sender.submit_try_send(&peer, b"bb").unwrap();
    assert!(first.try_result().unwrap().is_none());
    assert!(matches!(
        sender.submit_try_send(&peer, b"c"),
        Err(Error::Full)
    ));
    assert!(matches!(sender.submit_disconnect(&peer), Err(Error::Full)));
    // Stop не требует свободной позиции в очереди и не блокируется callback.
    sender.request_stop().unwrap();
    resume_tx.send(()).unwrap();
    worker.stop().unwrap();
    assert!(matches!(first.wait(), Err(Error::WorkerStopped)));
    assert!(matches!(second.wait(), Err(Error::WorkerStopped)));
    // Ни одна отложенная отправка не попала в канал после запроса stop.
    assert!(matches!(
        client.wait_readable(finite(), None),
        Err(Error::Disconnected(_))
    ));
}

#[test]
fn worker_drop_waits_for_running_callback_and_closes_external_sender() {
    let lobby = name();
    let (connected_tx, connected_rx) = mpsc::channel();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let worker =
        DispatchWorker::start(
            &lobby,
            options(),
            QueueLimits::default(),
            move |event| match event {
                DispatchEvent::Connected { peer, .. } => connected_tx.send(peer).unwrap(),
                DispatchEvent::Message { .. } => {
                    entered_tx.send(()).unwrap();
                    resume_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                }
                _ => {}
            },
        )
        .unwrap();
    let mut client = connect(&lobby).unwrap();
    let peer = connected_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let sender = worker.sender();
    client.try_send(b"block").unwrap();
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let (drop_tx, drop_rx) = mpsc::channel();
    let dropping = std::thread::spawn(move || {
        drop(worker);
        drop_tx.send(()).unwrap();
    });
    assert!(matches!(
        drop_rx.recv_timeout(Duration::from_millis(50)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    resume_tx.send(()).unwrap();
    drop_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    dropping.join().unwrap();
    assert!(matches!(
        sender.submit_try_send(&peer, b"late"),
        Err(Error::WorkerStopped)
    ));
    assert!(matches!(
        client.wait_readable(finite(), None),
        Err(Error::Disconnected(_))
    ));
}

#[test]
#[cfg(panic = "unwind")]
fn worker_panic_closes_queue_and_is_reported_on_every_stop() {
    let lobby = name();
    let (connected_tx, connected_rx) = mpsc::channel();
    let mut worker =
        DispatchWorker::start(&lobby, options(), QueueLimits::default(), move |event| {
            if let DispatchEvent::Connected { peer, .. } = event {
                connected_tx.send(peer).unwrap();
                panic!("ошибка обработчика worker");
            }
        })
        .unwrap();
    let mut client = connect(&lobby).unwrap();
    let peer = connected_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let sender = worker.sender();
    assert!(matches!(
        client.wait_readable(finite(), None),
        Err(Error::Disconnected(_))
    ));
    for _ in 0..2 {
        assert!(
            matches!(worker.stop(), Err(Error::WorkerFailed(error)) if matches!(error.as_ref(), Error::WorkerPanicked))
        );
    }
    assert!(matches!(
        sender.submit_try_send(&peer, b"late"),
        Err(Error::WorkerStopped)
    ));
}

#[test]
fn worker_start_reports_bind_failure_synchronously_and_validates_limits() {
    let lobby = name();
    let mut first =
        DispatchWorker::start(&lobby, options(), QueueLimits::default(), |_| {}).unwrap();
    assert!(matches!(
        DispatchWorker::start(&lobby, options(), QueueLimits::default(), |_| {}),
        Err(Error::NameCollision)
    ));
    let invalid = QueueLimits {
        commands: 0,
        ..QueueLimits::default()
    };
    assert!(matches!(
        DispatchWorker::start(&name(), options(), invalid, |_| {}),
        Err(Error::InvalidConfig)
    ));
    first.stop().unwrap();
    let mut next =
        DispatchWorker::start(&lobby, options(), QueueLimits::default(), |_| {}).unwrap();
    next.stop().unwrap();
}

#[test]
fn worker_owner_can_be_dropped_in_its_callback_without_self_join() {
    let lobby = name();
    let owner = Arc::new(std::sync::Mutex::new(None::<DispatchWorker>));
    let callback_owner = owner.clone();
    let (connected_tx, connected_rx) = mpsc::channel();
    let (dropped_tx, dropped_rx) = mpsc::channel();
    let worker = DispatchWorker::start(&lobby, options(), QueueLimits::default(), move |event| {
        match event {
            DispatchEvent::Connected { peer, .. } => connected_tx.send(peer).unwrap(),
            DispatchEvent::Message { .. } => {
                let worker = callback_owner.lock().unwrap().take().unwrap();
                drop(worker); // запрос stop без попытки join самого себя
                dropped_tx.send(()).unwrap();
            }
            _ => {}
        }
    })
    .unwrap();
    let sender = worker.sender();
    *owner.lock().unwrap() = Some(worker);
    let mut client = connect(&lobby).unwrap();
    let peer = connected_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    client.try_send(b"drop owner").unwrap();
    dropped_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(
        client.wait_readable(finite(), None),
        Err(Error::Disconnected(_))
    ));
    assert!(owner.lock().unwrap().is_none());
    assert!(matches!(
        sender.submit_try_send(&peer, b"late"),
        Err(Error::WorkerStopped)
    ));
}

#[test]
#[cfg(panic = "unwind")]
fn worker_preserves_admission_failure_and_stops_without_extra_callbacks() {
    let lobby = name();
    let mut worker = DispatchWorker::start_with_admission(
        &lobby,
        options(),
        QueueLimits::default(),
        |_| panic!("ошибка допуска"),
        |_| panic!("событий быть не должно"),
    )
    .unwrap();
    assert!(matches!(connect(&lobby), Err(Error::Rejected)));
    assert!(
        matches!(worker.stop(), Err(Error::WorkerFailed(error)) if matches!(error.as_ref(), Error::AdmissionPanicked))
    );
}

#[test]
fn worker_reports_transport_full_without_losing_the_previous_frame() {
    let lobby = name();
    let (connected_tx, connected_rx) = mpsc::channel();
    let mut config = options();
    config.channel.layout.s2c = xshm::c_core::RingConfig {
        capacity: 4096,
        max_messages: 1,
        max_message_size: 1024,
    };
    let mut worker = DispatchWorker::start(&lobby, config, QueueLimits::default(), move |event| {
        if let DispatchEvent::Connected { peer, .. } = event {
            connected_tx.send(peer).unwrap();
        }
    })
    .unwrap();
    let mut client = connect(&lobby).unwrap();
    let peer = connected_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let sender = worker.sender();
    sender
        .submit_try_send(&peer, b"one!")
        .unwrap()
        .wait()
        .unwrap();
    assert!(matches!(
        sender.submit_try_send(&peer, b"two!").unwrap().wait(),
        Err(Error::Full)
    ));
    let mut bytes = [0; 4];
    assert_eq!(client.receive_into(&mut bytes).unwrap(), 4);
    assert_eq!(&bytes, b"one!");
    sender
        .submit_try_send(&peer, b"next")
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(client.receive_into(&mut bytes).unwrap(), 4);
    assert_eq!(&bytes, b"next");
    worker.stop().unwrap();
}
