// Политика линтов -- в `[lints]` секции Cargo.toml (см. манифест).

mod client;
mod constants;
mod error;
pub mod events;
mod layout;
mod naming;
mod ring;
mod server;
mod shared;
mod thread_hook;
mod win;

pub mod auto;
mod beacon;
pub mod dispatch;
pub mod multi;

// Внутренний модуль - деталь реализации, не часть публичного API
pub(crate) mod ntapi;

#[cfg(test)]
mod peer_tests;

pub use auto::{AutoClient, AutoHandler, AutoOptions, AutoServer, AutoStatsSnapshot, ChannelKind};
pub use beacon::{Beacon, ProcessExit, Waitable, wait_any};
pub use client::SharedClient;
/// Лимиты кольца -- нужны потребителям, которые строят свой протокол поверх
/// `try_send*`/`free_space` (размер кадра, число слотов).
pub use constants::{
    MAX_MESSAGE_SIZE, MAX_MESSAGES, MESSAGE_HEADER_SIZE, MIN_MESSAGE_SIZE, RING_CAPACITY,
};
pub use dispatch::{
    ClientRegistration, DispatchClient, DispatchClientHandler, DispatchClientOptions,
    DispatchHandler, DispatchOptions, DispatchServer,
};
pub use error::{DisconnectReason, Result, ShmError};
pub use events::EventHandles;
pub use multi::{
    MultiClient, MultiClientHandler, MultiClientOptions, MultiHandler, MultiOptions, MultiServer,
};
pub use ring::{FreeSpace, WriteOutcome};
pub use server::SharedServer;
pub use thread_hook::{set_thread_start_hook, thread_start_hook};

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Пауза `delay` (отступ после ошибки), прерываемая событием `wake`: без
/// сна-циклов — поток спит в ядре до срабатывания `wake` или конца паузы.
/// Срабатывание `wake` без остановки (например, новая команда в очереди) не
/// укорачивает паузу. `false` — `running` стал `false`.
pub(crate) fn wait_delay_or(
    running: &AtomicBool,
    wake: &win::EventHandle,
    delay: Duration,
) -> bool {
    let deadline = Instant::now() + delay;
    loop {
        if !running.load(Ordering::Acquire) {
            return false;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        if wake.wait(Some(left)).is_err() {
            // Ожидание невозможно (дескриптор испорчен) -- не крутиться.
            std::thread::sleep(left);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn server_client_roundtrip() {
        const NAME: &str = "UNITTEST_XSHM";

        let server_thread = thread::spawn(|| -> Result<()> {
            let mut server = SharedServer::start(NAME)?;
            // ожидание клиента
            server.wait_for_client(Some(Duration::from_secs(5)))?;

            let mut recv_buffer = Vec::new();
            loop {
                if server.poll_client(Some(Duration::from_millis(50)))? {
                    let len = server.receive_from_client(&mut recv_buffer)?;
                    if &recv_buffer[..len] == b"bye" {
                        break;
                    }
                }
                server.send_to_client(b"ping")?;
            }
            Ok(())
        });

        let client_res = (|| -> Result<()> {
            // Сервер поднимается в соседнем потоке: под нагрузкой полного
            // прогона секции может ещё не быть -- повторяем подключение до
            // 5 с, а не угадываем фиксированной паузой.
            let deadline = Instant::now() + Duration::from_secs(5);
            let client = loop {
                match SharedClient::connect(NAME, Duration::from_secs(2)) {
                    Ok(client) => break client,
                    Err(_) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(err) => return Err(err),
                }
            };
            let mut recv = Vec::new();
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(5) {
                if client.poll_server(Some(Duration::from_millis(20)))? {
                    let len = client.receive_from_server(&mut recv)?;
                    if &recv[..len] == b"ping" {
                        client.send_to_server(b"bye")?;
                        break;
                    }
                }
            }
            Ok(())
        })();

        if let Err(err) = client_res {
            panic!("client error: {err:?}");
        }
        let server_result = server_thread.join().unwrap();
        assert!(server_result.is_ok());
    }

    /// Накопитель принятых сообщений + Condvar для пробуждения ожидающего.
    type MessageLog = Arc<(Mutex<Vec<Vec<u8>>>, Condvar)>;

    #[derive(Clone)]
    struct CaptureHandler {
        buffer: MessageLog,
    }

    impl CaptureHandler {
        fn new() -> (Self, MessageLog) {
            let shared = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
            (
                CaptureHandler {
                    buffer: shared.clone(),
                },
                shared,
            )
        }

        fn wait_for(shared: &MessageLog, expected: &[u8]) {
            let (lock, cv) = &**shared;
            let mut guard = lock.lock().unwrap();
            const TIMEOUT: Duration = Duration::from_secs(2);
            let start = Instant::now();
            loop {
                if guard.iter().any(|msg| msg.as_slice() == expected) {
                    break;
                }
                let elapsed = start.elapsed();
                if elapsed >= TIMEOUT {
                    panic!("timeout waiting for message {expected:?}");
                }
                let wait_for = (TIMEOUT - elapsed).min(Duration::from_millis(20));
                let (g, result) = cv.wait_timeout(guard, wait_for).unwrap();
                guard = g;
                if result.timed_out() && start.elapsed() >= TIMEOUT {
                    panic!("timeout waiting for message {expected:?}");
                }
            }
        }
    }

    impl AutoHandler for CaptureHandler {
        fn on_message(&self, _direction: ChannelKind, payload: &[u8]) {
            let (lock, cv) = &*self.buffer;
            let mut guard = lock.lock().unwrap();
            guard.push(payload.to_vec());
            cv.notify_all();
        }
    }

    #[test]
    fn auto_server_client_roundtrip() {
        let name = format!("AUTO_UNITTEST_{}", std::process::id());
        let (server_handler, server_buf) = CaptureHandler::new();
        let server = AutoServer::start(&name, Arc::new(server_handler), AutoOptions::default())
            .expect("server start");

        thread::sleep(Duration::from_millis(100));

        let (client_handler, client_buf) = CaptureHandler::new();
        let client = AutoClient::connect(&name, Arc::new(client_handler), AutoOptions::default())
            .expect("client connect");

        thread::sleep(Duration::from_millis(100));

        client.send(b"ping").expect("client send");
        server.send(b"pong").expect("server send");

        CaptureHandler::wait_for(&server_buf, b"ping");
        CaptureHandler::wait_for(&client_buf, b"pong");

        drop(client);
        drop(server);
    }
}
