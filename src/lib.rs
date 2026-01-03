#![forbid(unsafe_op_in_unsafe_fn)]

mod client;
mod constants;
mod error;
mod events;
mod layout;
mod naming;
mod ring;
mod server;
mod shared;
mod win;

pub mod auto;
pub mod ffi;
pub mod ntapi;

pub use auto::{AutoClient, AutoHandler, AutoOptions, AutoServer, AutoStatsSnapshot, ChannelKind};
pub use client::SharedClient;
pub use error::{Result, ShmError};
pub use ring::WriteOutcome;
pub use server::SharedServer;

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
            server.wait_for_client(Some(Duration::from_secs(2)))?;

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

        thread::sleep(Duration::from_millis(50));

        let client_res = (|| -> Result<()> {
            let client = SharedClient::connect(NAME, Duration::from_secs(2))?;
            let mut recv = Vec::new();
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(1) {
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

    #[derive(Clone)]
    struct CaptureHandler {
        buffer: Arc<(Mutex<Vec<Vec<u8>>>, Condvar)>,
    }

    impl CaptureHandler {
        fn new() -> (Self, Arc<(Mutex<Vec<Vec<u8>>>, Condvar)>) {
            let shared = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
            (
                CaptureHandler {
                    buffer: shared.clone(),
                },
                shared,
            )
        }

        fn wait_for(shared: &Arc<(Mutex<Vec<Vec<u8>>>, Condvar)>, expected: &[u8]) {
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
                    panic!("timeout waiting for message {:?}", expected);
                }
                let wait_for = (TIMEOUT - elapsed).min(Duration::from_millis(20));
                let (g, result) = cv.wait_timeout(guard, wait_for).unwrap();
                guard = g;
                if result.timed_out() && start.elapsed() >= TIMEOUT {
                    panic!("timeout waiting for message {:?}", expected);
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
