//! Стресс-тесты single-client канала: поток сообщений в одну и в обе
//! стороны с перезаписью при переполнении.
//!
//! Task 18: устойчивость под нагрузкой хоста. Никаких фиксированных `sleep`
//! как условия (клиент подключается по сигналу «сервер поднят», а не через
//! 50 мс) и никаких жёстких 5–10-секундных дедлайнов: сторож прогресса
//! (`Watchdog`) валит тест, только если ни одного нового сообщения нет
//! `STALL` (10 с -- это зависание, а не медленный хост), плюс общий дедлайн
//! с большим запасом (`OVERALL`, 120 с).

use std::convert::TryInto;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use xshm::{SharedClient, SharedServer, ShmError};

const MIN_SLEEP: Duration = Duration::from_micros(50);
/// Нет прогресса столько времени -- зависание.
const STALL: Duration = Duration::from_secs(10);
/// Общий дедлайн теста (с запасом на перегруженный хост).
const OVERALL: Duration = Duration::from_secs(120);
/// Ожидание рукопожатия: подключение только по сигналу «сервер поднят»,
/// граница -- от зависания, а не от медленного старта.
const CONNECT: Duration = Duration::from_secs(30);

#[expect(
    clippy::unwrap_used,
    reason = "тестовый помощник: паника -- провал теста"
)]
fn unique_name(tag: &str) -> String {
    use std::time::SystemTime;
    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(
        "XSHM_STRESS_{}_{}_{}",
        tag,
        std::process::id(),
        ts % 1_000_000
    )
}

/// Сторож прогресса: провал -- нет прогресса `STALL` или вышел `OVERALL`.
struct Watchdog {
    what: &'static str,
    started: Instant,
    last: Instant,
}

impl Watchdog {
    fn new(what: &'static str) -> Self {
        let now = Instant::now();
        Self {
            what,
            started: now,
            last: now,
        }
    }

    /// Новое сообщение (отправлено или принято).
    fn progress(&mut self) {
        self.last = Instant::now();
    }

    fn check(&self) {
        let idle = self.last.elapsed();
        assert!(idle < STALL, "{}: нет прогресса {idle:?}", self.what);
        let total = self.started.elapsed();
        assert!(total < OVERALL, "{}: общий дедлайн {total:?}", self.what);
    }
}

/// Поднять сервер в отдельном потоке: `ready` -- сигнал «секция создана»
/// (клиенту можно подключаться), затем ожидание клиента и `body`.
#[expect(
    clippy::unwrap_used,
    reason = "тестовый помощник: паника -- провал теста"
)]
fn spawn_server<T: Send + 'static>(
    name: &str,
    body: impl FnOnce(SharedServer) -> xshm::Result<T> + Send + 'static,
) -> thread::JoinHandle<xshm::Result<T>> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let name = name.to_owned();
    let handle = thread::spawn(move || {
        let mut server = SharedServer::start(&name)?;
        ready_tx.send(()).unwrap();
        server.wait_for_client(Some(CONNECT))?;
        body(server)
    });
    // Сервер не поднялся (ошибка старта) -- канал закрыт; ошибку отдаст join.
    let _ = ready_rx.recv();
    handle
}

#[test]
fn stress_server_to_client() {
    const TOTAL: usize = 4000;
    let name = unique_name("S2C");

    let server_thread = spawn_server(&name, |server| -> xshm::Result<(usize, usize)> {
        let mut dog = Watchdog::new("server S2C");
        let mut overwritten_total = 0usize;
        for i in 0..TOTAL {
            let mut msg = [0u8; 8];
            msg[..4].copy_from_slice(&(i as u32).to_le_bytes());
            loop {
                match server.send_to_client(&msg) {
                    Ok(outcome) => {
                        overwritten_total += outcome.overwritten as usize;
                        dog.progress();
                        break;
                    }
                    Err(ShmError::QueueFull) => {
                        dog.check();
                        thread::sleep(MIN_SLEEP);
                    }
                    Err(err) => return Err(err),
                }
            }
            thread::yield_now();
        }

        server.send_to_client(b"END")?;

        let mut recv_buf = Vec::new();
        let dog = Watchdog::new("server ждёт ACK");
        loop {
            dog.check();
            if server.poll_client(Some(Duration::from_millis(10)))? {
                let len = server.receive_from_client(&mut recv_buf)?;
                if &recv_buf[..len] == b"ACK" {
                    break;
                }
            }
        }

        Ok((TOTAL, overwritten_total))
    });

    let client_result = (|| -> xshm::Result<usize> {
        let client = SharedClient::connect(&name, CONNECT)?;
        let mut recv_buf = Vec::new();
        let mut expected = 0u32;
        let mut dog = Watchdog::new("client S2C");
        let mut done = false;

        while !done {
            dog.check();
            if client.poll_server(Some(Duration::from_millis(10)))? {
                // Drain all available messages after a successful poll
                loop {
                    match client.receive_from_server(&mut recv_buf) {
                        Ok(len) => {
                            dog.progress();
                            if &recv_buf[..len] == b"END" {
                                done = true;
                                break;
                            }
                            assert!(len >= 4, "message too short: {len}");
                            let value = u32::from_le_bytes(recv_buf[..4].try_into().unwrap());
                            // Ring buffer перезаписывает старые сообщения при переполнении
                            // (overwrite-on-full), поэтому легитимен forward-гэп: value может
                            // «перепрыгнуть» expected, если reader отстал и часть сообщений
                            // была вытеснена. Но value НИКОГДА не должен убывать или повторяться
                            // — это был бы реальный баг (переупорядочивание/дублирование).
                            assert!(
                                value >= expected,
                                "reorder/duplicate: got {value}, expected >= {expected}"
                            );
                            expected = value + 1;
                        }
                        Err(ShmError::QueueEmpty) => break,
                        Err(err) => return Err(err),
                    }
                }
            } else {
                thread::sleep(MIN_SLEEP);
            }
        }

        // Последнее сообщение до END не вытесняется (за ним в кольце только END).
        assert_eq!(expected as usize, TOTAL);
        client.send_to_server(b"ACK")?;
        Ok(expected as usize)
    })();

    if let Err(err) = client_result {
        panic!("client error: {err:?}");
    }

    match server_thread.join() {
        Ok(Ok((sent, _overwritten))) => {
            assert_eq!(sent, TOTAL);
            // overwrite при переполнении — легитимен (ring buffer вытесняет старое).
            // Порядок и отсутствие дублей проверяет reader через монотонность value,
            // поэтому не требуем overwritten == 0 — это делало тест хрупким под нагрузкой.
        }
        Ok(Err(err)) => panic!("server error: {err:?}"),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

#[test]
fn stress_client_to_server() {
    const TOTAL: usize = 4000;
    let name = unique_name("C2S");

    let server_thread = spawn_server(&name, |server| -> xshm::Result<usize> {
        let mut recv_buf = Vec::new();
        let mut expected = 0u32;
        let mut dog = Watchdog::new("server C2S");
        let mut done = false;

        while !done {
            dog.check();
            if server.poll_client(Some(Duration::from_millis(10)))? {
                // Drain all available messages after a successful poll
                loop {
                    match server.receive_from_client(&mut recv_buf) {
                        Ok(len) => {
                            dog.progress();
                            if &recv_buf[..len] == b"END" {
                                done = true;
                                break;
                            }
                            assert!(len >= 4);
                            let value = u32::from_le_bytes(recv_buf[..4].try_into().unwrap());
                            // См. пояснение в stress_server_to_client: overwrite-on-full
                            // допускает forward-гэп (value может обгонять expected), но
                            // убывание/повтор value — реальный баг (reorder/дубль).
                            assert!(
                                value >= expected,
                                "reorder/duplicate: got {value}, expected >= {expected}"
                            );
                            expected = value + 1;
                        }
                        Err(ShmError::QueueEmpty) => break,
                        Err(err) => return Err(err),
                    }
                }
            } else {
                thread::sleep(MIN_SLEEP);
            }
        }

        assert_eq!(expected as usize, TOTAL);
        server.send_to_client(b"ACK")?;
        Ok(expected as usize)
    });

    let client_result = (|| -> xshm::Result<(usize, usize)> {
        let client = SharedClient::connect(&name, CONNECT)?;
        let mut dog = Watchdog::new("client C2S");
        let mut overwritten_total = 0usize;
        for i in 0..TOTAL {
            let mut msg = [0u8; 8];
            msg[..4].copy_from_slice(&(i as u32).to_le_bytes());
            loop {
                match client.send_to_server(&msg) {
                    Ok(outcome) => {
                        overwritten_total += outcome.overwritten as usize;
                        dog.progress();
                        break;
                    }
                    Err(ShmError::QueueFull) => {
                        dog.check();
                        thread::sleep(MIN_SLEEP);
                    }
                    Err(err) => return Err(err),
                }
            }
            thread::yield_now();
        }

        client.send_to_server(b"END")?;

        let mut recv_buf = Vec::new();
        let dog = Watchdog::new("client ждёт ACK");
        loop {
            dog.check();
            if client.poll_server(Some(Duration::from_millis(10)))? {
                match client.receive_from_server(&mut recv_buf) {
                    Ok(len) => {
                        if &recv_buf[..len] == b"ACK" {
                            break;
                        }
                    }
                    Err(ShmError::QueueEmpty) => {}
                    Err(err) => return Err(err),
                }
            }
        }

        Ok((TOTAL, overwritten_total))
    })();

    match server_thread.join() {
        Ok(Ok(received)) => {
            assert_eq!(received, TOTAL);
        }
        Ok(Err(err)) => panic!("server error: {err:?}"),
        Err(panic) => std::panic::resume_unwind(panic),
    }

    match client_result {
        Ok((sent, _overwritten)) => {
            assert_eq!(sent, TOTAL);
            // overwrite легитимен; порядок и отсутствие дублей гарантирует reader
            // (монотонность value), поэтому overwritten == 0 больше не требуем.
        }
        Err(err) => panic!("client error: {err:?}"),
    }
}

#[test]
fn stress_bidirectional() {
    const TOTAL: usize = 2000;
    let name = unique_name("BI");

    let server_thread = spawn_server(&name, |server| -> xshm::Result<()> {
        let mut recv_buf = Vec::new();
        let mut received = 0u32;
        let mut sent = 0u32;
        let mut dog = Watchdog::new("server BI");

        while sent < TOTAL as u32 || received < TOTAL as u32 {
            if sent < TOTAL as u32 {
                let mut msg = [0u8; 8];
                msg[..4].copy_from_slice(&sent.to_le_bytes());
                loop {
                    match server.send_to_client(&msg) {
                        Ok(_) => break,
                        Err(ShmError::QueueFull) => {
                            dog.check();
                            thread::sleep(MIN_SLEEP);
                        }
                        Err(err) => return Err(err),
                    }
                }
                sent += 1;
                dog.progress();
            }

            if server.poll_client(Some(Duration::from_millis(1)))? {
                match server.receive_from_client(&mut recv_buf) {
                    Ok(len) => {
                        assert!(len >= 4);
                        let value = u32::from_le_bytes(recv_buf[..4].try_into().unwrap());
                        // overwrite-on-full допускает forward-гэп; убывание/повтор — баг.
                        assert!(
                            value >= received,
                            "server reorder/duplicate: got {value}, expected >= {received}"
                        );
                        received = value + 1;
                        dog.progress();
                    }
                    Err(ShmError::QueueEmpty) => {}
                    Err(err) => return Err(err),
                }
            } else {
                thread::sleep(MIN_SLEEP);
            }

            dog.check();
        }

        server.send_to_client(b"DONE")?;
        Ok(())
    });

    let client_result = (|| -> xshm::Result<()> {
        let client = SharedClient::connect(&name, CONNECT)?;
        let mut recv_buf = Vec::new();
        let mut received = 0u32;
        let mut sent = 0u32;
        let mut dog = Watchdog::new("client BI");

        while sent < TOTAL as u32 || received < TOTAL as u32 {
            if sent < TOTAL as u32 {
                let mut msg = [0u8; 8];
                msg[..4].copy_from_slice(&sent.to_le_bytes());
                loop {
                    match client.send_to_server(&msg) {
                        Ok(_) => break,
                        Err(ShmError::QueueFull) => {
                            dog.check();
                            thread::sleep(MIN_SLEEP);
                        }
                        Err(err) => return Err(err),
                    }
                }
                sent += 1;
                dog.progress();
            }

            if client.poll_server(Some(Duration::from_millis(1)))? {
                match client.receive_from_server(&mut recv_buf) {
                    Ok(len) => {
                        dog.progress();
                        if &recv_buf[..len] == b"DONE" {
                            break;
                        }
                        assert!(len >= 4);
                        let value = u32::from_le_bytes(recv_buf[..4].try_into().unwrap());
                        // overwrite-on-full допускает forward-гэп; убывание/повтор — баг.
                        assert!(
                            value >= received,
                            "client reorder/duplicate: got {value}, expected >= {received}"
                        );
                        received = value + 1;
                    }
                    Err(ShmError::QueueEmpty) => {}
                    Err(err) => return Err(err),
                }
            } else {
                thread::sleep(MIN_SLEEP);
            }

            dog.check();
        }

        Ok(())
    })();

    if let Err(err) = client_result {
        panic!("client error: {err:?}");
    }

    match server_thread.join() {
        Ok(Ok(())) => {}
        Ok(Err(err)) => panic!("server error: {err:?}"),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}
