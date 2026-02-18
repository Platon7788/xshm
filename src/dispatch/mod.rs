//! Dispatch server — central lobby with dynamic per-client channels.
//!
//! # Architecture
//!
//! ```text
//! DispatchServer("NxT")        ← single lobby, accepts all clients
//!     ↓
//! Client connects to lobby → sends RegistrationRequest {pid, revision, name}
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
    /// Messages to process per batch on each client channel.
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
    /// Worker loop poll interval on dedicated channel.
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
    server: AutoServer,
    info: ClientRegistration,
    channel_name: String,
    /// Set to true when disconnect has been handled (prevents double-notify).
    disconnected: AtomicBool,
}

/// Shared client map accessible from both server and proxy handlers.
type ClientMap = Arc<RwLock<HashMap<u32, DispatchedClient>>>;

/// Central dispatch server — one lobby, dynamic per-client channels.
pub struct DispatchServer {
    base_name: String,
    clients: ClientMap,
    running: Arc<AtomicBool>,
    next_client_id: Arc<AtomicU32>,
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
            next_client_id: Arc::new(AtomicU32::new(1)),
            worker_handle: Mutex::new(None),
            handler,
            options,
        });

        let server_clone = server.clone();
        let name_owned = name.to_owned();
        let handle = thread::Builder::new()
            .name(format!("xshm-dispatch-{name}"))
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
            // Mark as disconnected so AutoProxyHandler won't double-notify
            client.disconnected.store(true, Ordering::Release);
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

    /// Get registration info for a client.
    pub fn client_info(&self, client_id: u32) -> Option<ClientRegistration> {
        self.clients
            .read()
            .unwrap()
            .get(&client_id)
            .map(|c| c.info.clone())
    }

    /// Get channel name for a client.
    pub fn client_channel(&self, client_id: u32) -> Option<String> {
        self.clients
            .read()
            .unwrap()
            .get(&client_id)
            .map(|c| c.channel_name.clone())
    }

    /// Stop the dispatch server and all client channels.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
    }

    /// Base name of the dispatch server.
    pub fn base_name(&self) -> &str {
        &self.base_name
    }

    /// Generate a unique channel name with cryptographic-quality random suffix.
    fn generate_channel_name(&self) -> String {
        // Use multiple entropy sources for uniqueness
        let time_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;

        let counter = self.next_client_id.load(Ordering::Relaxed) as u64;

        // PCG-style mixing: time XOR counter, then xorshift + multiply
        let mut state = time_nanos ^ (counter.wrapping_mul(0x517cc1b727220a95));
        state ^= state >> 17;
        state = state.wrapping_mul(0xbf58476d1ce4e5b9);
        state ^= state >> 31;
        state = state.wrapping_mul(0x94d049bb133111eb);
        state ^= state >> 32;

        format!("{}_{:016x}", self.base_name, state)
    }

    /// Main worker loop — accepts clients through the lobby one at a time.
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
                match lobby_server.wait_for_client(Some(self.options.poll_timeout)) {
                    Ok(()) => {
                        // Client connected — process registration
                        self.handle_lobby_client(&mut lobby_server, &mut buffer);

                        // Reset lobby for next client
                        lobby_server.mark_disconnected();
                    }
                    Err(ShmError::Timeout) => continue,
                    Err(ShmError::AlreadyConnected) => {
                        lobby_server.mark_disconnected();
                    }
                    Err(err) => {
                        self.handler.on_error(None, err);
                        break; // Recreate lobby on serious error
                    }
                }
            }
        }

        // Shutdown: stop all client channels
        let mut clients = self.clients.write().unwrap();
        for (id, client) in clients.drain() {
            client.disconnected.store(true, Ordering::Release);
            client.server.stop();
            self.handler.on_client_disconnect(id);
        }
    }

    /// Handle a single client in the lobby: read registration, create channel, respond.
    fn handle_lobby_client(&self, lobby: &mut SharedServer, buffer: &mut Vec<u8>) {
        let events = match lobby.events() {
            Some(e) => e,
            None => {
                self.handler.on_error(None, ShmError::NotReady);
                return;
            }
        };

        // Wait for registration data via c2s.data event (event-driven, no polling)
        let start = std::time::Instant::now();
        let request = loop {
            let remaining = self.options.lobby_timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                self.handler.on_error(None, ShmError::Timeout);
                return;
            }

            if !self.running.load(Ordering::Acquire) {
                return;
            }

            // Try to read first — data may already be in the buffer
            match lobby.receive_from_client(buffer) {
                Ok(len) => match protocol::decode_request(&buffer[..len]) {
                    Ok(req) => break req,
                    Err(err) => {
                        self.handler.on_error(None, err);
                        return;
                    }
                },
                Err(ShmError::QueueEmpty) => {
                    // Block on event — wakes when client writes data
                    let wait_time = remaining.min(self.options.poll_timeout);
                    let _ = events.c2s.data.wait(Some(wait_time));
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
            revision: request.revision,
            name: request.name.clone(),
        };

        // Create AutoServer for this client's dedicated channel
        let connect_signal = Arc::new((Mutex::new(false), Condvar::new()));

        let proxy_handler = Arc::new(AutoProxyHandler {
            client_id,
            handler: self.handler.clone(),
            clients: Arc::clone(&self.clients),
            connect_signal: connect_signal.clone(),
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

        // Signal lobby data available so client can read
        if let Some(events) = lobby.events() {
            let _ = events.s2c.data.set();
        }

        // Wait for client to connect to dedicated channel (signaled by AutoProxyHandler)
        let (lock, cvar) = &*connect_signal;
        let connected = lock.lock().unwrap();
        let timeout = self.options.channel_connect_timeout;
        let (connected, result) = cvar.wait_timeout(connected, timeout).unwrap();
        let client_connected = *connected;
        drop(connected);

        if !client_connected || result.timed_out() {
            auto_server.stop();
            return;
        }

        // Register client in the shared map
        self.clients.write().unwrap().insert(
            client_id,
            DispatchedClient {
                server: auto_server,
                info: info.clone(),
                channel_name,
                disconnected: AtomicBool::new(false),
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
    clients: ClientMap,
    connect_signal: Arc<(Mutex<bool>, Condvar)>,
}

impl AutoHandler for AutoProxyHandler {
    fn on_connect(&self) {
        // Signal the lobby worker that the client has connected to its channel
        let (lock, cvar) = &*self.connect_signal;
        let mut connected = lock.lock().unwrap();
        *connected = true;
        cvar.notify_one();
    }

    fn on_disconnect(&self) {
        // Check if already handled (e.g. by disconnect_client())
        let removed = {
            let mut clients = self.clients.write().unwrap();
            if let Some(client) = clients.get(&self.client_id) {
                if client
                    .disconnected
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    clients.remove(&self.client_id);
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };

        if removed {
            self.handler.on_client_disconnect(self.client_id);
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

/// Client that connects to a DispatchServer, registers, and communicates
/// on a dynamically assigned channel.
///
/// Lifecycle: connect once → register → get channel → communicate → stop.
/// Does NOT reconnect automatically — if disconnected, create a new client.
pub struct DispatchClient {
    auto_client: Mutex<Option<AutoClient>>,
    running: Arc<AtomicBool>,
    client_id: u32,
    channel_name: String,
}

impl DispatchClient {
    /// Connect to a dispatch server, register, and start communicating.
    ///
    /// This is a blocking call — it performs the lobby handshake synchronously,
    /// then spawns the dedicated channel via AutoClient.
    pub fn connect(
        name: &str,
        registration: ClientRegistration,
        handler: Arc<dyn DispatchClientHandler>,
        options: DispatchClientOptions,
    ) -> Result<Self> {
        // Phase 1: Connect to lobby and register (blocking)
        let mut buffer = Vec::with_capacity(MAX_MESSAGE_SIZE);
        let (assigned_id, assigned_channel) =
            lobby_register(name, &registration, &options, &mut buffer)?;

        // Phase 2: Connect to dedicated channel via AutoClient
        let running = Arc::new(AtomicBool::new(true));

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

        let auto_client = AutoClient::connect(&assigned_channel, client_handler, auto_options)?;

        handler.on_connect(assigned_id, &assigned_channel);

        Ok(Self {
            auto_client: Mutex::new(Some(auto_client)),
            running,
            client_id: assigned_id,
            channel_name: assigned_channel,
        })
    }

    /// Send a message to the server on the dedicated channel.
    pub fn send(&self, data: &[u8]) -> Result<()> {
        if !self.running.load(Ordering::Acquire) {
            return Err(ShmError::NotReady);
        }
        let guard = self.auto_client.lock().unwrap();
        match guard.as_ref() {
            Some(client) => client.send(data),
            None => Err(ShmError::NotConnected),
        }
    }

    /// Get the assigned client ID.
    pub fn client_id(&self) -> u32 {
        self.client_id
    }

    /// Get the assigned channel name.
    pub fn channel_name(&self) -> &str {
        &self.channel_name
    }

    /// Check if connected to the dedicated channel.
    pub fn is_connected(&self) -> bool {
        self.running.load(Ordering::Acquire) && self.auto_client.lock().unwrap().is_some()
    }

    /// Stop the client and disconnect.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
        let mut guard = self.auto_client.lock().unwrap();
        if let Some(client) = guard.take() {
            client.stop();
        }
    }
}

impl Drop for DispatchClient {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        let mut guard = self.auto_client.lock().unwrap();
        if let Some(client) = guard.take() {
            client.stop();
        }
    }
}

// ─── Lobby registration (blocking) ──────────────────────────────────────────

/// Perform lobby registration: connect, send request, read response.
fn lobby_register(
    base_name: &str,
    registration: &ClientRegistration,
    options: &DispatchClientOptions,
    buffer: &mut Vec<u8>,
) -> Result<(u32, String)> {
    let client = SharedClient::connect(base_name, options.lobby_timeout)?;

    // Send registration request
    let request = protocol::encode_request(&RegistrationRequest {
        pid: registration.pid,
        revision: registration.revision,
        name: registration.name.clone(),
    });
    client.send_to_server(&request)?;

    // Signal data available for server via event
    let _ = client.events().c2s.data.set();

    // Wait for response via s2c.data event (event-driven, no polling)
    let events = client.events();
    let start = std::time::Instant::now();
    loop {
        let remaining = options.response_timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return Err(ShmError::Timeout);
        }

        // Try to read first — data may already be in the buffer
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
                // Block on event — wakes when server writes response
                let _ = events
                    .s2c
                    .data
                    .wait(Some(remaining.min(options.poll_timeout)));
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

        // Wait for server to register the client
        let start = std::time::Instant::now();
        while server_handler.connects.load(Ordering::Relaxed) == 0
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

    #[test]
    fn dispatch_disconnect_no_double_notify() {
        let name = format!("TEST_DISPATCH_DC_{}", std::process::id());

        let server_handler = Arc::new(TestServerHandler::new());
        let server =
            DispatchServer::start(&name, server_handler.clone(), DispatchOptions::default())
                .expect("server start");

        thread::sleep(Duration::from_millis(100));

        let client_handler = Arc::new(TestClientHandler::new());
        let registration = ClientRegistration {
            pid: 99999,
            revision: 1,
            name: "dc_test.exe".into(),
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
        while server.client_count() == 0 && start.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(server.client_count(), 1);

        // Server-side disconnect
        let clients = server.connected_clients();
        server
            .disconnect_client(clients[0])
            .expect("disconnect_client");

        thread::sleep(Duration::from_millis(300));

        // Should only see exactly 1 disconnect (not double)
        assert_eq!(server_handler.disconnects.load(Ordering::Relaxed), 1);

        client.stop();
        server.stop();
    }

    #[test]
    fn dispatch_multiple_clients() {
        let name = format!("TEST_DISPATCH_MC_{}", std::process::id());

        let server_handler = Arc::new(TestServerHandler::new());
        let server =
            DispatchServer::start(&name, server_handler.clone(), DispatchOptions::default())
                .expect("server start");

        thread::sleep(Duration::from_millis(100));

        // Connect 3 clients sequentially
        let mut clients = Vec::new();
        for i in 0..3u32 {
            let handler = Arc::new(TestClientHandler::new());
            let reg = ClientRegistration {
                pid: 1000 + i,
                revision: 1,
                name: format!("client_{i}.exe"),
            };
            let client = DispatchClient::connect(
                &name,
                reg,
                handler.clone(),
                DispatchClientOptions::default(),
            )
            .expect("client connect");
            clients.push((client, handler));

            // Wait for server to register this client
            let expected = i + 1;
            let start = std::time::Instant::now();
            while server.client_count() < expected && start.elapsed() < Duration::from_secs(5) {
                thread::sleep(Duration::from_millis(50));
            }
        }

        assert_eq!(server.client_count(), 3);
        assert_eq!(server_handler.connects.load(Ordering::Relaxed), 3);

        // All clients can send messages
        for (client, _) in &clients {
            client.send(b"ping").expect("send");
        }
        thread::sleep(Duration::from_millis(300));
        assert!(server_handler.messages.load(Ordering::Relaxed) >= 3);

        // Broadcast
        let sent = server.broadcast(b"pong").expect("broadcast");
        assert_eq!(sent, 3);
        thread::sleep(Duration::from_millis(300));
        for (_, handler) in &clients {
            assert!(handler.messages.load(Ordering::Relaxed) >= 1);
        }

        // Cleanup
        for (client, _) in &clients {
            client.stop();
        }
        thread::sleep(Duration::from_millis(200));
        server.stop();
    }
}
