//! Dispatch server — central lobby with dynamic per-client channels.
//!
//! # Architecture
//!
//! ```text
//! DispatchServer("NxT")        ← single lobby, accepts all clients
//!     ↓
//! Client connects to lobby → sends RegistrationRequest {pid, bits, name}
//!     ↓
//! Server creates AutoServer("NxT_a7f3b2c1") → sends RegistrationResponse
//!     ↓
//! Client disconnects from lobby → connects to "NxT_a7f3b2c1" via AutoClient
//!     ↓
//! 1:1 communication on dedicated channel
//! ```

pub mod ffi;
pub mod protocol;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::auto::{AutoClient, AutoHandler, AutoOptions, AutoServer, ChannelKind};
use crate::client::SharedClient;
use crate::constants::MAX_MESSAGE_SIZE;
use crate::error::{Result, ShmError};
use crate::server::SharedServer;
use crate::wait_delay;

pub use protocol::{RegistrationRequest, RegistrationResponse};

// ─── Public types ────────────────────────────────────────────────────────────

/// Client registration data received during lobby handshake.
#[derive(Debug, Clone)]
pub struct ClientRegistration {
    pub pid: u32,
    pub bits: u8,
    pub revision: u16,
    pub name: String,
}

/// Callback interface for DispatchServer events.
pub trait DispatchHandler: Send + Sync + 'static {
    /// Called when a client has registered and connected to its dedicated channel.
    fn on_client_connect(&self, client_id: u32, info: &ClientRegistration);

    /// Called when a client disconnects from its dedicated channel.
    fn on_client_disconnect(&self, client_id: u32);

    /// Called when a message is received from a client on its dedicated channel.
    fn on_message(&self, client_id: u32, data: &[u8]);

    /// Called on error (client_id = None for general errors).
    fn on_error(&self, client_id: Option<u32>, err: ShmError) {
        let _ = (client_id, err);
    }
}

/// Callback interface for DispatchClient events.
pub trait DispatchClientHandler: Send + Sync + 'static {
    /// Called when successfully connected to dedicated channel.
    fn on_connect(&self, client_id: u32, channel_name: &str);

    /// Called when disconnected from dedicated channel.
    fn on_disconnect(&self);

    /// Called when a message is received from the server.
    fn on_message(&self, data: &[u8]);

    /// Called on error.
    fn on_error(&self, err: ShmError) {
        let _ = err;
    }
}

/// Options for DispatchServer.
#[derive(Clone)]
pub struct DispatchOptions {
    /// Timeout for reading registration data from lobby after handshake.
    pub lobby_timeout: Duration,
    /// Timeout for client to connect to its dedicated channel after registration.
    pub channel_connect_timeout: Duration,
    /// Worker loop poll interval.
    pub poll_timeout: Duration,
    /// Messages to process per batch.
    pub recv_batch: usize,
}

impl Default for DispatchOptions {
    fn default() -> Self {
        Self {
            lobby_timeout: Duration::from_secs(5),
            channel_connect_timeout: Duration::from_secs(30),
            poll_timeout: Duration::from_millis(50),
            recv_batch: 32,
        }
    }
}

/// Options for DispatchClient.
#[derive(Clone)]
pub struct DispatchClientOptions {
    /// Timeout for lobby connection.
    pub lobby_timeout: Duration,
    /// Timeout for reading registration response from lobby.
    pub response_timeout: Duration,
    /// Timeout for connecting to dedicated channel.
    pub channel_timeout: Duration,
    /// Worker loop poll interval.
    pub poll_timeout: Duration,
    /// Messages to process per batch.
    pub recv_batch: usize,
    /// Max queued messages before dropping oldest.
    pub max_send_queue: usize,
}

impl Default for DispatchClientOptions {
    fn default() -> Self {
        Self {
            lobby_timeout: Duration::from_secs(5),
            response_timeout: Duration::from_secs(5),
            channel_timeout: Duration::from_secs(10),
            poll_timeout: Duration::from_millis(50),
            recv_batch: 32,
            max_send_queue: 256,
        }
    }
}

// ─── DispatchServer ──────────────────────────────────────────────────────────

/// Active client on a dedicated channel.
struct DispatchedClient {
    #[allow(dead_code)]
    server: AutoServer,
    #[allow(dead_code)]
    info: ClientRegistration,
    #[allow(dead_code)]
    channel_name: String,
}

/// Shared client map accessible from both server and proxy handlers.
type ClientMap = Arc<RwLock<HashMap<u32, DispatchedClient>>>;

/// Central dispatch server — one lobby, dynamic per-client channels.
pub struct DispatchServer {
    base_name: String,
    clients: ClientMap,
    running: Arc<AtomicBool>,
    next_client_id: AtomicU32,
    worker_handle: Mutex<Option<JoinHandle<()>>>,
    handler: Arc<dyn DispatchHandler>,
    options: DispatchOptions,
}

impl DispatchServer {
    /// Start the dispatch server with a lobby on the given base name.
    pub fn start(
        name: &str,
        handler: Arc<dyn DispatchHandler>,
        options: DispatchOptions,
    ) -> Result<Arc<Self>> {
        let running = Arc::new(AtomicBool::new(true));

        let server = Arc::new(Self {
            base_name: name.to_owned(),
            clients: Arc::new(RwLock::new(HashMap::new())),
            running,
            next_client_id: AtomicU32::new(1),
            worker_handle: Mutex::new(None),
            handler,
            options,
        });

        let server_clone = server.clone();
        let name_owned = name.to_owned();
        let handle = thread::Builder::new()
            .name(format!("xshm-dispatch-{}", name))
            .spawn(move || server_clone.worker_loop(&name_owned))
            .map_err(|e| ShmError::WindowsError {
                code: e.raw_os_error().unwrap_or(-1) as u32,
                context: "spawn dispatch worker",
            })?;

        *server.worker_handle.lock().unwrap() = Some(handle);

        Ok(server)
    }

    /// Send a message to a specific client.
    pub fn send_to(&self, client_id: u32, data: &[u8]) -> Result<()> {
        let clients = self.clients.read().unwrap();
        let client = clients.get(&client_id).ok_or(ShmError::NotConnected)?;
        client.server.send(data)
    }

    /// Broadcast a message to all connected clients.
    pub fn broadcast(&self, data: &[u8]) -> Result<u32> {
        let clients = self.clients.read().unwrap();
        let mut sent = 0u32;
        for client in clients.values() {
            if client.server.send(data).is_ok() {
                sent += 1;
            }
        }
        Ok(sent)
    }

    /// Disconnect a specific client and destroy its channel.
    pub fn disconnect_client(&self, client_id: u32) -> Result<()> {
        let removed = self.clients.write().unwrap().remove(&client_id);
        if let Some(client) = removed {
            client.server.stop();
            self.handler.on_client_disconnect(client_id);
            Ok(())
        } else {
            Err(ShmError::NotConnected)
        }
    }

    /// Get list of connected client IDs.
    pub fn connected_clients(&self) -> Vec<u32> {
        self.clients.read().unwrap().keys().copied().collect()
    }

    /// Get number of connected clients.
    pub fn client_count(&self) -> u32 {
        self.clients.read().unwrap().len() as u32
    }

    /// Check if a specific client is connected.
    pub fn is_client_connected(&self, client_id: u32) -> bool {
        self.clients.read().unwrap().contains_key(&client_id)
    }

    /// Stop the dispatch server and all client channels.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
    }

    /// Base name of the dispatch server.
    pub fn base_name(&self) -> &str {
        &self.base_name
    }

    /// Generate a unique channel name with random suffix.
    fn generate_channel_name(&self) -> String {
        let id = self.next_client_id.load(Ordering::Relaxed);
        let random = {
            // Simple PRNG using thread ID + time for uniqueness
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            let thread_id = thread::current().id();
            let hash = t
                .wrapping_mul(6364136223846793005)
                .wrapping_add(format!("{:?}", thread_id).len() as u64);
            hash ^ (id as u64)
        };
        format!("{}_{:08x}", self.base_name, random as u32)
    }

    /// Main worker loop — handles lobby connections and routes messages.
    fn worker_loop(&self, base_name: &str) {
        let mut buffer = Vec::with_capacity(MAX_MESSAGE_SIZE);

        while self.running.load(Ordering::Acquire) {
            // Create lobby (or recreate on failure)
            let mut lobby_server = match SharedServer::start(base_name) {
                Ok(s) => s,
                Err(err) => {
                    self.handler.on_error(None, err);
                    if !wait_delay(&self.running, self.options.poll_timeout) {
                        break;
                    }
                    continue;
                }
            };

            // Inner loop: accept clients sequentially through the lobby
            while self.running.load(Ordering::Acquire) {
                // Wait for a client to connect to lobby
                match lobby_server.wait_for_client(Some(self.options.poll_timeout)) {
                    Ok(()) => {
                        // Client connected — process registration
                        self.handle_lobby_client(&mut lobby_server, &mut buffer);

                        // Reset lobby for next client
                        lobby_server.mark_disconnected();
                    }
                    Err(ShmError::Timeout) => continue,
                    Err(ShmError::AlreadyConnected) => {
                        // Shouldn't happen, but reset and continue
                        lobby_server.mark_disconnected();
                    }
                    Err(err) => {
                        self.handler.on_error(None, err);
                        break; // Recreate lobby
                    }
                }
            }
        }

        // Shutdown: stop all client channels
        let mut clients = self.clients.write().unwrap();
        for (_, client) in clients.drain() {
            client.server.stop();
        }
    }

    /// Handle a single client in the lobby: read registration, create channel, respond.
    fn handle_lobby_client(&self, lobby: &mut SharedServer, buffer: &mut Vec<u8>) {
        // Read registration request (poll with timeout)
        let start = std::time::Instant::now();
        let request = loop {
            if start.elapsed() >= self.options.lobby_timeout {
                self.handler.on_error(None, ShmError::Timeout);
                return;
            }

            match lobby.receive_from_client(buffer) {
                Ok(len) => match protocol::decode_request(&buffer[..len]) {
                    Ok(req) => break req,
                    Err(err) => {
                        self.handler.on_error(None, err);
                        return;
                    }
                },
                Err(ShmError::QueueEmpty) => {
                    // Poll for data
                    let _ = lobby.poll_client(Some(Duration::from_millis(10)));
                    continue;
                }
                Err(err) => {
                    self.handler.on_error(None, err);
                    return;
                }
            }
        };

        let client_id = self.next_client_id.fetch_add(1, Ordering::Relaxed);
        let channel_name = self.generate_channel_name();

        let info = ClientRegistration {
            pid: request.pid,
            bits: request.bits,
            revision: request.revision,
            name: request.name.clone(),
        };

        // Create AutoServer for this client's dedicated channel
        let connect_signal = Arc::new((Mutex::new(false), Condvar::new()));
        let disconnect_signal = Arc::new(AtomicBool::new(false));

        let proxy_handler = Arc::new(AutoProxyHandler {
            client_id,
            handler: self.handler.clone(),
            connect_signal: connect_signal.clone(),
            disconnect_clients: Arc::clone(&self.clients),
            disconnect_signal: disconnect_signal.clone(),
        });

        let auto_options = AutoOptions {
            connect_timeout: self.options.channel_connect_timeout,
            wait_timeout: self.options.poll_timeout,
            recv_batch: self.options.recv_batch,
            ..AutoOptions::default()
        };

        let auto_server = match AutoServer::start(&channel_name, proxy_handler, auto_options) {
            Ok(s) => s,
            Err(err) => {
                self.handler.on_error(None, err);
                // Send rejection to client
                let reject = protocol::encode_response(&RegistrationResponse {
                    status: protocol::STATUS_REJECTED,
                    client_id: 0,
                    channel_name: String::new(),
                });
                let _ = lobby.send_to_client(&reject);
                return;
            }
        };

        // Send channel assignment to client via lobby
        let response = protocol::encode_response(&RegistrationResponse {
            status: protocol::STATUS_OK,
            client_id,
            channel_name: channel_name.clone(),
        });

        if let Err(err) = lobby.send_to_client(&response) {
            self.handler.on_error(None, err);
            auto_server.stop();
            return;
        }

        // Signal lobby data available
        let _ = lobby.events().map(|e| e.s2c.data.set());

        // Wait for client to connect to dedicated channel
        let (lock, cvar) = &*connect_signal;
        let mut connected = lock.lock().unwrap();
        let timeout = self.options.channel_connect_timeout;
        let result = cvar.wait_timeout(connected, timeout).unwrap();
        connected = result.0;

        if !*connected {
            // Client didn't connect in time
            auto_server.stop();
            return;
        }
        drop(connected);

        // Register client
        self.clients.write().unwrap().insert(
            client_id,
            DispatchedClient {
                server: auto_server,
                info: info.clone(),
                channel_name,
            },
        );

        self.handler.on_client_connect(client_id, &info);
    }
}

impl Drop for DispatchServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(handle) = self.worker_handle.lock().unwrap().take() {
            let _ = handle.join();
        }
    }
}

// ─── AutoProxyHandler — bridges AutoServer events to DispatchHandler ─────────

struct AutoProxyHandler {
    client_id: u32,
    handler: Arc<dyn DispatchHandler>,
    connect_signal: Arc<(Mutex<bool>, Condvar)>,
    disconnect_clients: ClientMap,
    disconnect_signal: Arc<AtomicBool>,
}

impl AutoHandler for AutoProxyHandler {
    fn on_connect(&self) {
        let (lock, cvar) = &*self.connect_signal;
        let mut connected = lock.lock().unwrap();
        *connected = true;
        cvar.notify_one();
    }

    fn on_disconnect(&self) {
        if self
            .disconnect_signal
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            // Remove from clients map and notify handler
            let removed = self
                .disconnect_clients
                .write()
                .unwrap()
                .remove(&self.client_id);
            if removed.is_some() {
                self.handler.on_client_disconnect(self.client_id);
            }
        }
    }

    fn on_message(&self, _direction: ChannelKind, payload: &[u8]) {
        self.handler.on_message(self.client_id, payload);
    }

    fn on_error(&self, err: ShmError) {
        self.handler.on_error(Some(self.client_id), err);
    }
}

// ─── DispatchClient ──────────────────────────────────────────────────────────

use std::sync::mpsc::{self, Receiver, Sender};

enum ClientCommand {
    Send(Vec<u8>),
    Shutdown,
}

/// Client that connects to a DispatchServer, registers, and communicates
/// on a dynamically assigned channel.
pub struct DispatchClient {
    cmd_tx: Sender<ClientCommand>,
    join: Mutex<Option<JoinHandle<()>>>,
    running: Arc<AtomicBool>,
    client_id: Arc<AtomicU32>,
    channel_name: Arc<Mutex<String>>,
}

impl DispatchClient {
    /// Connect to a dispatch server, register, and start communicating.
    pub fn connect(
        name: &str,
        registration: ClientRegistration,
        handler: Arc<dyn DispatchClientHandler>,
        options: DispatchClientOptions,
    ) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let client_id = Arc::new(AtomicU32::new(0));
        let channel_name = Arc::new(Mutex::new(String::new()));

        let running_clone = running.clone();
        let client_id_clone = client_id.clone();
        let channel_name_clone = channel_name.clone();
        let base_name = name.to_owned();

        let handle = thread::Builder::new()
            .name(format!("xshm-dispatch-client-{}", name))
            .spawn(move || {
                dispatch_client_worker(
                    &base_name,
                    registration,
                    handler,
                    options,
                    rx,
                    running_clone,
                    client_id_clone,
                    channel_name_clone,
                );
            })
            .map_err(|e| ShmError::WindowsError {
                code: e.raw_os_error().unwrap_or(-1) as u32,
                context: "spawn dispatch client worker",
            })?;

        Ok(Self {
            cmd_tx: tx,
            join: Mutex::new(Some(handle)),
            running,
            client_id,
            channel_name,
        })
    }

    /// Send a message to the server on the dedicated channel.
    pub fn send(&self, data: &[u8]) -> Result<()> {
        if !self.running.load(Ordering::Acquire) {
            return Err(ShmError::NotReady);
        }
        self.cmd_tx
            .send(ClientCommand::Send(data.to_vec()))
            .map_err(|_| ShmError::NotReady)
    }

    /// Get the assigned client ID (0 if not yet registered).
    pub fn client_id(&self) -> u32 {
        self.client_id.load(Ordering::Acquire)
    }

    /// Get the assigned channel name (empty if not yet registered).
    pub fn channel_name(&self) -> String {
        self.channel_name.lock().unwrap().clone()
    }

    /// Check if connected to the dedicated channel.
    pub fn is_connected(&self) -> bool {
        self.client_id.load(Ordering::Acquire) != 0
    }

    /// Stop the client.
    pub fn stop(&self) {
        let _ = self.cmd_tx.send(ClientCommand::Shutdown);
    }
}

impl Drop for DispatchClient {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        let _ = self.cmd_tx.send(ClientCommand::Shutdown);
        if let Some(handle) = self.join.lock().unwrap().take() {
            let _ = handle.join();
        }
    }
}

// ─── Client worker ───────────────────────────────────────────────────────────

fn dispatch_client_worker(
    base_name: &str,
    registration: ClientRegistration,
    handler: Arc<dyn DispatchClientHandler>,
    options: DispatchClientOptions,
    cmd_rx: Receiver<ClientCommand>,
    running: Arc<AtomicBool>,
    client_id_out: Arc<AtomicU32>,
    channel_name_out: Arc<Mutex<String>>,
) {
    let mut buffer = Vec::with_capacity(MAX_MESSAGE_SIZE);

    while running.load(Ordering::Acquire) {
        // Phase 1: Connect to lobby and register
        let (assigned_id, assigned_channel) =
            match lobby_register(base_name, &registration, &options, &mut buffer) {
                Ok(result) => result,
                Err(err) => {
                    handler.on_error(err);
                    if !wait_delay(&running, options.poll_timeout) {
                        break;
                    }
                    continue;
                }
            };

        client_id_out.store(assigned_id, Ordering::Release);
        *channel_name_out.lock().unwrap() = assigned_channel.clone();

        // Phase 2: Connect to dedicated channel via AutoClient
        let client_handler = Arc::new(DispatchClientProxy {
            handler: handler.clone(),
        });

        let auto_options = AutoOptions {
            connect_timeout: options.channel_timeout,
            wait_timeout: options.poll_timeout,
            max_send_queue: options.max_send_queue,
            recv_batch: options.recv_batch,
            ..AutoOptions::default()
        };

        let auto_client = match AutoClient::connect(&assigned_channel, client_handler, auto_options)
        {
            Ok(c) => c,
            Err(err) => {
                handler.on_error(err);
                client_id_out.store(0, Ordering::Release);
                if !wait_delay(&running, options.poll_timeout) {
                    break;
                }
                continue;
            }
        };

        handler.on_connect(assigned_id, &assigned_channel);

        // Phase 3: Forward commands to auto client
        loop {
            if !running.load(Ordering::Acquire) {
                break;
            }

            match cmd_rx.recv_timeout(options.poll_timeout) {
                Ok(ClientCommand::Send(data)) => {
                    if let Err(err) = auto_client.send(&data) {
                        handler.on_error(err);
                    }
                }
                Ok(ClientCommand::Shutdown) => {
                    running.store(false, Ordering::Release);
                    break;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    running.store(false, Ordering::Release);
                    break;
                }
            }
        }

        auto_client.stop();
        client_id_out.store(0, Ordering::Release);

        if !wait_delay(&running, options.poll_timeout) {
            break;
        }
    }
}

/// Perform lobby registration: connect, send request, read response.
fn lobby_register(
    base_name: &str,
    registration: &ClientRegistration,
    options: &DispatchClientOptions,
    buffer: &mut Vec<u8>,
) -> Result<(u32, String)> {
    // Connect to lobby
    let client = SharedClient::connect(base_name, options.lobby_timeout)?;

    // Send registration request
    let request = protocol::encode_request(&RegistrationRequest {
        pid: registration.pid,
        bits: registration.bits,
        revision: registration.revision,
        name: registration.name.clone(),
    });
    client.send_to_server(&request)?;

    // Wait for response
    let start = std::time::Instant::now();
    loop {
        if start.elapsed() >= options.response_timeout {
            return Err(ShmError::Timeout);
        }

        match client.receive_from_server(buffer) {
            Ok(len) => {
                let response = protocol::decode_response(&buffer[..len])?;
                if response.status != protocol::STATUS_OK {
                    return Err(ShmError::HandshakeFailed);
                }
                // Drop client — disconnects from lobby
                drop(client);
                return Ok((response.client_id, response.channel_name));
            }
            Err(ShmError::QueueEmpty) => {
                let _ = client.poll_server(Some(Duration::from_millis(10)));
                continue;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Proxy handler that forwards AutoClient events to DispatchClientHandler.
struct DispatchClientProxy {
    handler: Arc<dyn DispatchClientHandler>,
}

impl AutoHandler for DispatchClientProxy {
    fn on_disconnect(&self) {
        self.handler.on_disconnect();
    }

    fn on_message(&self, _direction: ChannelKind, payload: &[u8]) {
        self.handler.on_message(payload);
    }

    fn on_error(&self, err: ShmError) {
        self.handler.on_error(err);
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    struct TestServerHandler {
        connects: AtomicU32,
        disconnects: AtomicU32,
        messages: AtomicU32,
        last_pid: AtomicU32,
    }

    impl TestServerHandler {
        fn new() -> Self {
            Self {
                connects: AtomicU32::new(0),
                disconnects: AtomicU32::new(0),
                messages: AtomicU32::new(0),
                last_pid: AtomicU32::new(0),
            }
        }
    }

    impl DispatchHandler for TestServerHandler {
        fn on_client_connect(&self, _client_id: u32, info: &ClientRegistration) {
            self.connects.fetch_add(1, Ordering::Relaxed);
            self.last_pid.store(info.pid, Ordering::Relaxed);
        }
        fn on_client_disconnect(&self, _client_id: u32) {
            self.disconnects.fetch_add(1, Ordering::Relaxed);
        }
        fn on_message(&self, _client_id: u32, _data: &[u8]) {
            self.messages.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct TestClientHandler {
        connected: AtomicBool,
        messages: AtomicU32,
    }

    impl TestClientHandler {
        fn new() -> Self {
            Self {
                connected: AtomicBool::new(false),
                messages: AtomicU32::new(0),
            }
        }
    }

    impl DispatchClientHandler for TestClientHandler {
        fn on_connect(&self, _client_id: u32, _channel_name: &str) {
            self.connected.store(true, Ordering::Relaxed);
        }
        fn on_disconnect(&self) {
            self.connected.store(false, Ordering::Relaxed);
        }
        fn on_message(&self, _data: &[u8]) {
            self.messages.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn dispatch_server_start_stop() {
        let handler = Arc::new(TestServerHandler::new());
        let name = format!("TEST_DISPATCH_{}", std::process::id());
        let server = DispatchServer::start(&name, handler, DispatchOptions::default());
        assert!(server.is_ok());
        let server = server.unwrap();
        assert_eq!(server.client_count(), 0);
        server.stop();
    }

    #[test]
    fn dispatch_roundtrip() {
        let name = format!("TEST_DISPATCH_RT_{}", std::process::id());

        let server_handler = Arc::new(TestServerHandler::new());
        let server =
            DispatchServer::start(&name, server_handler.clone(), DispatchOptions::default())
                .expect("server start");

        thread::sleep(Duration::from_millis(100));

        let client_handler = Arc::new(TestClientHandler::new());
        let registration = ClientRegistration {
            pid: 12345,
            bits: 32,
            revision: 1,
            name: "test.exe".into(),
        };

        let client = DispatchClient::connect(
            &name,
            registration,
            client_handler.clone(),
            DispatchClientOptions::default(),
        )
        .expect("client connect");

        // Wait for connection
        let start = std::time::Instant::now();
        while !client_handler.connected.load(Ordering::Relaxed)
            && start.elapsed() < Duration::from_secs(5)
        {
            thread::sleep(Duration::from_millis(50));
        }

        assert!(client_handler.connected.load(Ordering::Relaxed));
        assert_eq!(server_handler.connects.load(Ordering::Relaxed), 1);
        assert_eq!(server_handler.last_pid.load(Ordering::Relaxed), 12345);
        assert!(client.client_id() > 0);
        assert_eq!(server.client_count(), 1);

        // Send message from client to server
        client.send(b"hello").expect("client send");
        thread::sleep(Duration::from_millis(200));
        assert!(server_handler.messages.load(Ordering::Relaxed) >= 1);

        // Send message from server to client
        let clients = server.connected_clients();
        assert_eq!(clients.len(), 1);
        server.send_to(clients[0], b"world").expect("server send");
        thread::sleep(Duration::from_millis(200));
        assert!(client_handler.messages.load(Ordering::Relaxed) >= 1);

        // Cleanup
        client.stop();
        thread::sleep(Duration::from_millis(200));
        server.stop();
    }
}
