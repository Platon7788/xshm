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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    ServerToClient,
    ClientToServer,
}

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
    pub wait_timeout: Duration,
    pub reconnect_delay: Duration,
    pub connect_timeout: Duration,
    pub max_send_queue: usize,
    pub recv_batch: usize,
}

impl Default for AutoOptions {
    fn default() -> Self {
        Self {
            wait_timeout: Duration::from_millis(50),
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
        let join = thread::Builder::new()
            .name(format!("xshm-auto-server-{}", name))
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
            let _ = handle.join();
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
            match server.wait_for_client(Some(options.wait_timeout)) {
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

        process_receive_queue(
            server,
            &handler,
            &stats,
            &mut buffer,
            options.recv_batch,
            ChannelKind::ClientToServer,
        );

        match win::wait_any(&handles, Some(options.wait_timeout)) {
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
            let _ = handle.join();
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
                &mut client,
                &send_queue,
                &handler,
                &stats,
                ChannelKind::ClientToServer,
            );
            process_receive_queue(
                &client,
                &handler,
                &stats,
                &mut buffer,
                options.recv_batch,
                ChannelKind::ServerToClient,
            );

            match win::wait_any(&handles, Some(options.wait_timeout)) {
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

fn process_receive_queue<R>(
    endpoint: &R,
    handler: &Arc<dyn AutoHandler>,
    stats: &Arc<AutoStats>,
    buffer: &mut Vec<u8>,
    batch: usize,
    direction: ChannelKind,
) where
    R: ReceiveEndpoint,
{
    for _ in 0..batch.max(1) {
        match endpoint.read(buffer) {
            Ok(len) => {
                stats.received_messages.fetch_add(1, Ordering::Relaxed);
                handler.on_message(direction, &buffer[..len]);
            }
            Err(ShmError::QueueEmpty) => break,
            Err(err @ ShmError::NotConnected)
            | Err(err @ ShmError::NotReady)
            | Err(err @ ShmError::Timeout) => {
                handler.on_error(err.clone());
                break;
            }
            Err(err) => {
                handler.on_error(err.clone());
                break;
            }
        }
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
