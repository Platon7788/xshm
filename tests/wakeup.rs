//! Регрессия «потерянный сигнал `DATA`» (0.10, Task 21): писатель решал,
//! сигналить ли `DATA`, по прежнему `message_count == 0`, а читатель считал
//! кольцо пустым по позициям и уменьшал счётчик ПОСЛЕ сдвига `read_pos`.
//! Сообщение оставалось в кольце, а читатель спал на `DATA` без сигнала.
//!
//! Тест -- два потока, 10⁶ сообщений по 16 байт, читатель ждёт ТОЛЬКО
//! событие `DATA` (`poll_*(None)`, без таймаутов-страховок), писатель --
//! только место (`wait_for_space(None)`). Общий дедлайн 60 с держит главный
//! поток: зависание -- провал теста, а не вечный прогон.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use xshm::{SharedClient, SharedServer, ShmError};

const TOTAL: u64 = 1_000_000;
const DEADLINE: Duration = Duration::from_secs(60);

#[expect(
    clippy::unwrap_used,
    reason = "тестовый помощник: паника -- провал теста"
)]
fn unique_name(tag: &str) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("XSHM_WAKE_{tag}_{}_{}", std::process::id(), ts % 1_000_000)
}

/// 16 байт: номер сообщения дважды (порядок и целостность).
fn payload(seq: u64) -> [u8; 16] {
    let mut msg = [0u8; 16];
    msg[..8].copy_from_slice(&seq.to_le_bytes());
    msg[8..].copy_from_slice(&(!seq).to_le_bytes());
    msg
}

fn check(seq: u64, msg: &[u8]) {
    assert_eq!(
        msg,
        payload(seq),
        "сообщение {seq} искажено или не по порядку"
    );
}

/// Направление обмена: кто пишет, кто читает.
#[derive(Clone, Copy, Debug)]
enum Direction {
    ServerToClient,
    ClientToServer,
}

/// Писатель: `try_send` без перезаписи, при полном кольце -- ожидание
/// места без таймаута.
fn write_all(
    try_send: impl Fn(&[u8]) -> xshm::Result<xshm::WriteOutcome>,
    wait_space: impl Fn() -> xshm::Result<bool>,
) -> xshm::Result<()> {
    for seq in 0..TOTAL {
        let msg = payload(seq);
        loop {
            match try_send(&msg) {
                Ok(_) => break,
                Err(ShmError::QueueFull) => {
                    wait_space()?;
                }
                Err(err) => return Err(err),
            }
        }
    }
    Ok(())
}

/// Читатель: читать до `QueueEmpty`, потом спать ТОЛЬКО на `DATA`.
fn read_all(
    receive: impl Fn(&mut Vec<u8>) -> xshm::Result<usize>,
    poll: impl Fn() -> xshm::Result<bool>,
    progress: &AtomicU64,
) -> xshm::Result<()> {
    let mut buf = Vec::new();
    let mut seq = 0u64;
    while seq < TOTAL {
        match receive(&mut buf) {
            Ok(len) => {
                check(seq, &buf[..len]);
                seq += 1;
                // Relaxed: счётчик прогресса только для диагностики зависания.
                progress.store(seq, Ordering::Relaxed);
            }
            Err(ShmError::QueueEmpty) => {
                poll()?;
            }
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

#[expect(clippy::unwrap_used, reason = "тест: паника -- провал теста")]
fn run(direction: Direction) {
    let name = unique_name(match direction {
        Direction::ServerToClient => "S2C",
        Direction::ClientToServer => "C2S",
    });
    let progress = Arc::new(AtomicU64::new(0));
    let (done_tx, done_rx) = mpsc::channel::<(&'static str, xshm::Result<()>)>();

    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let server_thread = thread::spawn({
        let name = name.clone();
        let progress = Arc::clone(&progress);
        let done_tx = done_tx.clone();
        move || {
            let mut server = SharedServer::start(&name).unwrap();
            ready_tx.send(()).unwrap();
            server
                .wait_for_client(Some(Duration::from_secs(10)))
                .unwrap();
            let result = match direction {
                Direction::ServerToClient => write_all(
                    |m| server.try_send_to_client(m),
                    || server.wait_for_space(16, None),
                ),
                Direction::ClientToServer => read_all(
                    |b| server.receive_from_client(b),
                    || server.poll_client(None),
                    &progress,
                ),
            };
            let _ = done_tx.send(("server", result));
            // Сервер живёт, пока главный поток не дождётся клиента: иначе
            // Drop сервера отключит ещё читающего клиента.
            server
        }
    });

    ready_rx.recv().unwrap();
    let client_thread = thread::spawn({
        let progress = Arc::clone(&progress);
        move || {
            let client = SharedClient::connect(&name, Duration::from_secs(10)).unwrap();
            let result = match direction {
                Direction::ServerToClient => read_all(
                    |b| client.receive_from_server(b),
                    || client.poll_server(None),
                    &progress,
                ),
                Direction::ClientToServer => write_all(
                    |m| client.try_send_to_server(m),
                    || client.wait_for_space(16, None),
                ),
            };
            let _ = done_tx.send(("client", result));
            client
        }
    });

    let deadline = Instant::now() + DEADLINE;
    for _ in 0..2 {
        let left = deadline.saturating_duration_since(Instant::now());
        match done_rx.recv_timeout(left) {
            Ok((side, result)) => {
                assert!(result.is_ok(), "{direction:?}: {side} -> {result:?}");
            }
            Err(_) => panic!(
                "{direction:?}: зависание -- прочитано {} из {TOTAL} за {DEADLINE:?} \
                 (потерянное пробуждение)",
                // Relaxed: диагностика.
                progress.load(Ordering::Relaxed)
            ),
        }
    }
    let client = client_thread.join().unwrap();
    drop(client);
    drop(server_thread.join().unwrap());
}

#[test]
fn data_wakeup_is_never_lost_server_to_client() {
    run(Direction::ServerToClient);
}

#[test]
fn data_wakeup_is_never_lost_client_to_server() {
    run(Direction::ClientToServer);
}
