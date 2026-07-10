use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use std::sync::mpsc::{self, Receiver, Sender};

use crate::client::SharedClient;
use crate::constants::MAX_MESSAGE_SIZE;
use crate::error::{Result, ShmError};
use crate::server::SharedServer;
use crate::wait_delay;
use crate::win::{self};

fn map_spawn_error(err: std::io::Error, context: &'static str) -> ShmError {
    let code = err.raw_os_error().map(|c| c as u32).unwrap_or(0xFFFFFFFF);
    ShmError::WindowsError { code, context }
}

/// Джойнит worker-поток, если это безопасно; при self-join -- отпускает
/// `JoinHandle` без блокировки.
///
/// Если `Drop for AutoServer`/`AutoClient` вызывается СИНХРОННО из
/// собственного worker-потока (пользовательский `AutoHandler` дропает
/// сервер/клиент прямо внутри своего же callback'а -- `on_disconnect`,
/// `on_message` и т.п. вызываются ИМЕННО на worker-потоке), безусловный
/// `handle.join()` был бы self-join deadlock: поток ждал бы сам себя
/// навсегда (аудит 2026-07-10 -- тот же паттерн, что вызвал деадлок в
/// `dispatch/`, но здесь он общий для любого потребителя `auto`, публичного
/// модуля, а не только для одного места использования внутри библиотеки).
///
/// При обнаружении self-join `JoinHandle` просто дропается без join: поток
/// уже видит `running=false` (устанавливается до этого вызова) и завершится
/// сам -- безопасный detach силами ОС, не утечка (тред всё равно скоро
/// вернёт управление и выйдет из своего цикла).
fn join_unless_self(handle: JoinHandle<()>) {
    if handle.thread().id() != thread::current().id() {
        let _ = handle.join();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    ServerToClient,
    ClientToServer,
}

/// Callback-интерфейс `AutoServer`/`AutoClient`.
///
/// Все методы вызываются СИНХРОННО из собственного worker-потока
/// `AutoServer`/`AutoClient`. Если реализация синхронно дропает тот же
/// `AutoServer`/`AutoClient`, который её вызвал (например, извлекает его из
/// общей коллекции и роняет прямо в callback'е), `Drop` НЕ будет ждать
/// (`join()`) этот же worker-поток -- вместо самоблокировки поток просто
/// открепляется (detach) и завершится самостоятельно чуть позже (аудит
/// 2026-07-10). Это исключает deadlock, но означает, что к моменту
/// возврата из `Drop` поток может быть ещё не завершён -- если нужна
/// гарантия полного завершения, дропайте объект из ДРУГОГО потока.
pub trait AutoHandler: Send + Sync + 'static {
    fn on_connect(&self) {}
    fn on_disconnect(&self) {}
    fn on_message(&self, _direction: ChannelKind, _payload: &[u8]) {}
    fn on_overflow(&self, _direction: ChannelKind, _count: u32) {}
    fn on_space_available(&self, _direction: ChannelKind) {}
    fn on_error(&self, _err: ShmError) {}
}

#[derive(Clone)]
pub struct AutoOptions {
    /// Интервал опроса в worker loop. Переименовано из `wait_timeout` (0.6.0)
    /// для единообразия с `MultiOptions`/`DispatchOptions`, где то же самое
    /// поле называется `poll_timeout` -- разнобой имён заставлял вручную
    /// перекладывать значения при построении `AutoOptions` внутри `dispatch/`.
    pub poll_timeout: Duration,
    pub reconnect_delay: Duration,
    pub connect_timeout: Duration,
    pub max_send_queue: usize,
    pub recv_batch: usize,
}

impl Default for AutoOptions {
    fn default() -> Self {
        Self {
            poll_timeout: Duration::from_millis(50),
            reconnect_delay: Duration::from_millis(250),
            connect_timeout: Duration::from_secs(2),
            max_send_queue: 256,
            recv_batch: 32,
        }
    }
}

#[derive(Default, Clone, Debug)]
pub struct AutoStatsSnapshot {
    pub sent_messages: u64,
    pub send_overflows: u64,
    pub received_messages: u64,
    pub receive_overflows: u64,
}

#[derive(Default)]
struct AutoStats {
    sent_messages: AtomicU64,
    send_overflows: AtomicU64,
    received_messages: AtomicU64,
    receive_overflows: AtomicU64,
}

impl AutoStats {
    fn snapshot(&self) -> AutoStatsSnapshot {
        AutoStatsSnapshot {
            sent_messages: self.sent_messages.load(Ordering::Relaxed),
            send_overflows: self.send_overflows.load(Ordering::Relaxed),
            received_messages: self.received_messages.load(Ordering::Relaxed),
            receive_overflows: self.receive_overflows.load(Ordering::Relaxed),
        }
    }
}

enum WorkerCommand {
    Send(Vec<u8>),
    Shutdown,
}

struct SendQueue {
    queue: Mutex<VecDeque<Vec<u8>>>,
}

impl SendQueue {
    fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
        }
    }

    fn push(&self, data: Vec<u8>) {
        let mut guard = self.queue.lock().unwrap();
        guard.push_back(data);
    }

    fn pop(&self) -> Option<Vec<u8>> {
        let mut guard = self.queue.lock().unwrap();
        guard.pop_front()
    }

    fn push_front(&self, data: Vec<u8>) {
        let mut guard = self.queue.lock().unwrap();
        guard.push_front(data);
    }

    fn len(&self) -> usize {
        self.queue.lock().unwrap().len()
    }
}

pub struct AutoServer {
    cmd_tx: Sender<WorkerCommand>,
    join: Mutex<Option<JoinHandle<()>>>,
    stats: Arc<AutoStats>,
    running: Arc<AtomicBool>,
}

impl AutoServer {
    pub fn start(name: &str, handler: Arc<dyn AutoHandler>, options: AutoOptions) -> Result<Self> {
        let mut server = SharedServer::start(name)?;
        let (tx, rx) = mpsc::channel();
        let stats = Arc::new(AutoStats::default());
        let running = Arc::new(AtomicBool::new(true));
        let join_running = running.clone();
        let join_stats = stats.clone();
        let join_handler = handler.clone();
        let name_str = name.to_owned();
        // Thread name in debug only (opaque short tag `xsa-{name}` so
        // local traces still line up with the segment), anonymous in
        // release so Process Explorer / Process Hacker doesn't surface
        // "xshm-auto-server-…" as a flashing signpost on the host process.
        #[cfg_attr(not(debug_assertions), allow(unused_mut))]
        let mut builder = thread::Builder::new();
        #[cfg(debug_assertions)]
        {
            builder = builder.name(format!("xsa-{}", name));
        }
        let join = builder
            .spawn(move || {
                server_worker(
                    &name_str,
                    &mut server,
                    join_handler,
                    options,
                    rx,
                    join_stats,
                    join_running,
                );
            })
            .map_err(|err| map_spawn_error(err, "spawn server worker"))?;
        Ok(Self {
            cmd_tx: tx,
            join: Mutex::new(Some(join)),
            stats,
            running,
        })
    }

    pub fn send(&self, data: &[u8]) -> Result<()> {
        if !self.running.load(Ordering::Acquire) {
            return Err(ShmError::NotReady);
        }
        let msg = data.to_vec();
        self.cmd_tx
            .send(WorkerCommand::Send(msg))
            .map_err(|_| ShmError::NotReady)
    }

    pub fn stop(&self) {
        let _ = self.cmd_tx.send(WorkerCommand::Shutdown);
    }

    pub fn stats(&self) -> AutoStatsSnapshot {
        self.stats.snapshot()
    }
}

impl Drop for AutoServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        let _ = self.cmd_tx.send(WorkerCommand::Shutdown);
        if let Some(handle) = self.join.lock().unwrap().take() {
            join_unless_self(handle);
        }
    }
}

fn server_worker(
    _name: &str,
    server: &mut SharedServer,
    handler: Arc<dyn AutoHandler>,
    options: AutoOptions,
    cmd_rx: Receiver<WorkerCommand>,
    stats: Arc<AutoStats>,
    running: Arc<AtomicBool>,
) {
    let send_queue = SendQueue::new();
    let mut buffer = Vec::with_capacity(MAX_MESSAGE_SIZE);
    // Anonymous режим не поддерживается в auto-mode
    let server_events = server
        .events()
        .expect("Anonymous mode not supported in auto-mode");
    let handles = [
        server_events.disconnect.raw_handle(),
        server_events.c2s.data.raw_handle(),
        server_events.s2c.space.raw_handle(),
    ];

    let mut connected = false;

    while running.load(Ordering::Acquire) {
        if !connected {
            match server.wait_for_client(Some(options.poll_timeout)) {
                Ok(_) => {
                    connected = true;
                    handler.on_connect();
                }
                Err(ShmError::Timeout) => {
                    drain_commands(&send_queue, &cmd_rx, &options, &running);
                    continue;
                }
                Err(err) => {
                    handler.on_error(err.clone());
                    drain_commands(&send_queue, &cmd_rx, &options, &running);
                    continue;
                }
            }
        }

        drain_commands(&send_queue, &cmd_rx, &options, &running);

        if !connected {
            continue;
        }

        process_send_queue(
            server,
            &send_queue,
            &handler,
            &stats,
            ChannelKind::ServerToClient,
        );

        let outcome = process_receive_queue(
            server,
            &handler,
            &stats,
            &mut buffer,
            options.recv_batch,
            ChannelKind::ClientToServer,
        );
        if outcome.fatal {
            handler.on_disconnect();
            server.mark_disconnected();
            connected = false;
            continue;
        }
        if outcome.more_pending {
            // Ещё есть данные — не блокируемся, сразу следующий проход.
            continue;
        }

        match win::wait_any(&handles, Some(options.poll_timeout)) {
            Ok(Some(0)) => {
                handler.on_disconnect();
                server.mark_disconnected();
                connected = false;
            }
            Ok(Some(1)) => {
                // data available, loop will read
            }
            Ok(Some(2)) => {
                handler.on_space_available(ChannelKind::ServerToClient);
            }
            Ok(Some(_)) => {}
            Ok(None) => {}
            Err(err) => {
                handler.on_error(err.clone());
                handler.on_disconnect();
                server.mark_disconnected();
                connected = false;
            }
        }
    }
}

pub struct AutoClient {
    cmd_tx: Sender<WorkerCommand>,
    join: Mutex<Option<JoinHandle<()>>>,
    stats: Arc<AutoStats>,
    running: Arc<AtomicBool>,
}

impl AutoClient {
    pub fn connect(
        name: &str,
        handler: Arc<dyn AutoHandler>,
        options: AutoOptions,
    ) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let stats = Arc::new(AutoStats::default());
        let running = Arc::new(AtomicBool::new(true));
        let join_stats = stats.clone();
        let join_running = running.clone();
        let handler_clone = handler.clone();
        let name_str = name.to_owned();

        let join = thread::Builder::new()
            .name(format!("xshm-auto-client-{}", name))
            .spawn(move || {
                client_worker(
                    &name_str,
                    handler_clone,
                    options,
                    rx,
                    join_stats,
                    join_running,
                );
            })
            .map_err(|err| map_spawn_error(err, "spawn client worker"))?;

        Ok(Self {
            cmd_tx: tx,
            join: Mutex::new(Some(join)),
            stats,
            running,
        })
    }

    pub fn send(&self, data: &[u8]) -> Result<()> {
        if !self.running.load(Ordering::Acquire) {
            return Err(ShmError::NotReady);
        }
        let msg = data.to_vec();
        self.cmd_tx
            .send(WorkerCommand::Send(msg))
            .map_err(|_| ShmError::NotReady)
    }

    pub fn stop(&self) {
        let _ = self.cmd_tx.send(WorkerCommand::Shutdown);
    }

    pub fn stats(&self) -> AutoStatsSnapshot {
        self.stats.snapshot()
    }
}

impl Drop for AutoClient {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        let _ = self.cmd_tx.send(WorkerCommand::Shutdown);
        if let Some(handle) = self.join.lock().unwrap().take() {
            join_unless_self(handle);
        }
    }
}

fn client_worker(
    name: &str,
    handler: Arc<dyn AutoHandler>,
    options: AutoOptions,
    cmd_rx: Receiver<WorkerCommand>,
    stats: Arc<AutoStats>,
    running: Arc<AtomicBool>,
) {
    let send_queue = SendQueue::new();
    let mut buffer = Vec::with_capacity(MAX_MESSAGE_SIZE);

    while running.load(Ordering::Acquire) {
        let mut client = match SharedClient::connect(name, options.connect_timeout) {
            Ok(client) => client,
            Err(err) => {
                handler.on_error(err.clone());
                if !wait_delay(&running, options.reconnect_delay) {
                    break;
                }
                continue;
            }
        };

        handler.on_connect();
        // SharedClient всегда использует named events (не anonymous)
        let client_events = client.events();
        let handles = [
            client_events.disconnect.raw_handle(),
            client_events.s2c.data.raw_handle(),
            client_events.c2s.space.raw_handle(),
        ];

        loop {
            if !running.load(Ordering::Acquire) {
                break;
            }

            drain_commands(&send_queue, &cmd_rx, &options, &running);
            process_send_queue(
                &client,
                &send_queue,
                &handler,
                &stats,
                ChannelKind::ClientToServer,
            );
            let outcome = process_receive_queue(
                &client,
                &handler,
                &stats,
                &mut buffer,
                options.recv_batch,
                ChannelKind::ServerToClient,
            );
            if outcome.fatal {
                handler.on_disconnect();
                client.mark_disconnected();
                break;
            }
            if outcome.more_pending {
                continue;
            }

            match win::wait_any(&handles, Some(options.poll_timeout)) {
                Ok(Some(0)) => {
                    handler.on_disconnect();
                    client.mark_disconnected();
                    break;
                }
                Ok(Some(1)) => {}
                Ok(Some(2)) => handler.on_space_available(ChannelKind::ClientToServer),
                Ok(Some(_)) => {}
                Ok(None) => {}
                Err(err) => {
                    handler.on_error(err.clone());
                    handler.on_disconnect();
                    client.mark_disconnected();
                    break;
                }
            }
        }

        if !wait_delay(&running, options.reconnect_delay) {
            break;
        }
    }
}

fn drain_commands(
    queue: &SendQueue,
    rx: &Receiver<WorkerCommand>,
    options: &AutoOptions,
    running: &Arc<AtomicBool>,
) {
    while let Ok(cmd) = rx.try_recv() {
        match cmd {
            WorkerCommand::Send(msg) => {
                if queue.len() >= options.max_send_queue {
                    // drop oldest (overwrite semantics)
                    let _ = queue.pop();
                }
                queue.push(msg);
            }
            WorkerCommand::Shutdown => {
                running.store(false, Ordering::Release);
            }
        }
    }
}

fn process_send_queue<E>(
    endpoint: &E,
    queue: &SendQueue,
    handler: &Arc<dyn AutoHandler>,
    stats: &Arc<AutoStats>,
    direction: ChannelKind,
) where
    E: SendEndpoint,
{
    while let Some(msg) = queue.pop() {
        match endpoint.write(&msg) {
            Ok(outcome) => {
                stats.sent_messages.fetch_add(1, Ordering::Relaxed);
                if outcome.overwritten > 0 {
                    stats
                        .send_overflows
                        .fetch_add(outcome.overwritten as u64, Ordering::Relaxed);
                    handler.on_overflow(direction, outcome.overwritten);
                }
            }
            Err(ShmError::QueueFull) => {
                queue.push_front(msg);
                break;
            }
            Err(err) => {
                handler.on_error(err.clone());
                queue.push_front(msg);
                break;
            }
        }
    }
}

/// Результат обработки приёмной очереди за один проход.
struct ReceiveOutcome {
    /// Фатальная ошибка — соединение надо сбросить.
    fatal: bool,
    /// Батч упёрся в лимит, в кольце вероятно ещё есть данные — не спать.
    more_pending: bool,
}

/// Обрабатывает до `batch` сообщений за вызов.
fn process_receive_queue<R>(
    endpoint: &R,
    handler: &Arc<dyn AutoHandler>,
    stats: &Arc<AutoStats>,
    buffer: &mut Vec<u8>,
    batch: usize,
    direction: ChannelKind,
) -> ReceiveOutcome
where
    R: ReceiveEndpoint,
{
    let mut drained = false;
    for _ in 0..batch.max(1) {
        match endpoint.read(buffer) {
            Ok(len) => {
                stats.received_messages.fetch_add(1, Ordering::Relaxed);
                handler.on_message(direction, &buffer[..len]);
            }
            Err(ShmError::QueueEmpty) => {
                drained = true;
                break;
            }
            Err(ShmError::NotConnected) | Err(ShmError::NotReady) | Err(ShmError::Timeout) => {
                drained = true;
                break;
            }
            Err(ref err @ ShmError::Corrupted) => {
                handler.on_error(err.clone());
                return ReceiveOutcome {
                    fatal: true,
                    more_pending: false,
                };
            }
            Err(err) => {
                handler.on_error(err.clone());
                drained = true;
                break;
            }
        }
    }
    ReceiveOutcome {
        fatal: false,
        more_pending: !drained,
    }
}

trait SendEndpoint {
    fn write(&self, data: &[u8]) -> Result<crate::ring::WriteOutcome>;
}

trait ReceiveEndpoint {
    fn read(&self, buffer: &mut Vec<u8>) -> Result<usize>;
}

impl SendEndpoint for SharedServer {
    fn write(&self, data: &[u8]) -> Result<crate::ring::WriteOutcome> {
        self.send_to_client(data)
    }
}

impl ReceiveEndpoint for SharedServer {
    fn read(&self, buffer: &mut Vec<u8>) -> Result<usize> {
        self.receive_from_client(buffer)
    }
}

impl SendEndpoint for SharedClient {
    fn write(&self, data: &[u8]) -> Result<crate::ring::WriteOutcome> {
        self.send_to_server(data)
    }
}

impl ReceiveEndpoint for SharedClient {
    fn read(&self, buffer: &mut Vec<u8>) -> Result<usize> {
        self.receive_from_server(buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Handler, который в `on_disconnect` дропает контейнер, содержащий сам
    /// `AutoServer` -- воспроизводит паттерн, который вызвал self-join
    /// deadlock в `dispatch/` (аудит 2026-07-10, до фикса): callback
    /// вызывается СИНХРОННО из собственного worker-потока `AutoServer`, и
    /// если внутри него этот же `AutoServer` дропается, `Drop::drop` пытается
    /// заджойнить `worker_handle`, которым и является текущий поток.
    struct SelfDroppingHandler {
        container: Arc<Mutex<Option<AutoServer>>>,
        disconnect_returned: Arc<AtomicBool>,
    }

    impl AutoHandler for SelfDroppingHandler {
        fn on_disconnect(&self) {
            let taken = self.container.lock().unwrap().take();
            drop(taken); // именно тут раньше был self-join deadlock
            self.disconnect_returned.store(true, Ordering::Release);
        }
    }

    struct NoopHandler;
    impl AutoHandler for NoopHandler {}

    /// Регрессия (аудит 2026-07-10): `Drop for AutoServer`/`AutoClient`
    /// раньше безусловно джойнил `worker_handle` -- если Drop вызывается ИЗ
    /// СОБСТВЕННОГО worker-потока (пользовательский `AutoHandler` синхронно
    /// роняет `AutoServer` внутри своего же callback'а), это self-join
    /// deadlock. `auto` -- публичный модуль, доступный любому внешнему
    /// потребителю библиотеки, поэтому фикс должен быть в самом `Drop`, а не
    /// полагаться на то, что каждый вызывающий код (как `dispatch/`) сам
    /// не наступит на эти грабли.
    #[test]
    fn drop_from_own_worker_callback_does_not_self_join_deadlock() {
        let name = format!("TEST_AUTO_SELFDROP_{}", std::process::id());
        let container: Arc<Mutex<Option<AutoServer>>> = Arc::new(Mutex::new(None));
        let disconnect_returned = Arc::new(AtomicBool::new(false));

        let handler = Arc::new(SelfDroppingHandler {
            container: container.clone(),
            disconnect_returned: disconnect_returned.clone(),
        });

        let server = AutoServer::start(&name, handler, AutoOptions::default()).expect("start");
        *container.lock().unwrap() = Some(server);

        let client = AutoClient::connect(&name, Arc::new(NoopHandler), AutoOptions::default())
            .expect("client connect");

        // Даём клиенту время реально подключиться, прежде чем отключать.
        thread::sleep(Duration::from_millis(200));
        client.stop();

        // Если self-join deadlock всё ещё существует, on_disconnect зависнет
        // НАВСЕГДА внутри Drop for AutoServer, и флаг не станет true.
        let start = std::time::Instant::now();
        while !disconnect_returned.load(Ordering::Acquire)
            && start.elapsed() < Duration::from_secs(10)
        {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            disconnect_returned.load(Ordering::Acquire),
            "on_disconnect не вернулся за 10с -- self-join deadlock"
        );
    }
}
