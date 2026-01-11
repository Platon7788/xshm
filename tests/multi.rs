//! Тесты мультиклиентного сервера с автоматическим назначением слотов

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use xshm::multi::{
    MultiClient, MultiClientHandler, MultiClientOptions,
    MultiHandler, MultiOptions, MultiServer,
};
use xshm::ShmError;

fn unique_name(tag: &str) -> String {
    use std::time::SystemTime;
    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("XSHM_MULTI_{}_{}_{}", tag, std::process::id(), ts % 1_000_000)
}

/// Handler для тестов MultiServer
struct TestServerHandler {
    connects: AtomicU32,
    disconnects: AtomicU32,
    messages: AtomicU32,
    last_client_id: AtomicU32,
    last_message: Mutex<Vec<u8>>,
}

impl TestServerHandler {
    fn new() -> Self {
        Self {
            connects: AtomicU32::new(0),
            disconnects: AtomicU32::new(0),
            messages: AtomicU32::new(0),
            last_client_id: AtomicU32::new(u32::MAX),
            last_message: Mutex::new(Vec::new()),
        }
    }
    
    fn wait_for_connects(&self, count: u32, timeout: Duration) -> bool {
        let start = Instant::now();
        while self.connects.load(Ordering::Acquire) < count {
            if start.elapsed() >= timeout {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
        true
    }
    
    fn wait_for_messages(&self, count: u32, timeout: Duration) -> bool {
        let start = Instant::now();
        while self.messages.load(Ordering::Acquire) < count {
            if start.elapsed() >= timeout {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
        true
    }
}

impl MultiHandler for TestServerHandler {
    fn on_client_connect(&self, client_id: u32) {
        println!("[Server] Client {} connected", client_id);
        self.connects.fetch_add(1, Ordering::Release);
        self.last_client_id.store(client_id, Ordering::Release);
    }
    
    fn on_client_disconnect(&self, client_id: u32) {
        println!("[Server] Client {} disconnected", client_id);
        self.disconnects.fetch_add(1, Ordering::Release);
    }
    
    fn on_message(&self, client_id: u32, data: &[u8]) {
        println!("[Server] Message from client {}: {:?}", client_id, String::from_utf8_lossy(data));
        self.messages.fetch_add(1, Ordering::Release);
        let mut guard = self.last_message.lock().unwrap();
        guard.clear();
        guard.extend_from_slice(data);
    }
    
    fn on_error(&self, client_id: Option<u32>, err: ShmError) {
        println!("[Server] Error for client {:?}: {:?}", client_id, err);
    }
}

/// Handler для тестовых клиентов
struct TestClientHandler {
    slot_id: AtomicU32,
    messages: AtomicU32,
    connected: AtomicU32,
    last_message: Mutex<Vec<u8>>,
}

impl TestClientHandler {
    fn new() -> Self {
        Self {
            slot_id: AtomicU32::new(u32::MAX),
            messages: AtomicU32::new(0),
            connected: AtomicU32::new(0),
            last_message: Mutex::new(Vec::new()),
        }
    }
    
    fn wait_for_connect(&self, timeout: Duration) -> bool {
        let start = Instant::now();
        while self.connected.load(Ordering::Acquire) == 0 {
            if start.elapsed() >= timeout {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
        true
    }
    
    fn wait_for_messages(&self, count: u32, timeout: Duration) -> bool {
        let start = Instant::now();
        while self.messages.load(Ordering::Acquire) < count {
            if start.elapsed() >= timeout {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
        true
    }
}

impl MultiClientHandler for TestClientHandler {
    fn on_connect(&self, slot_id: u32) {
        println!("[Client] Connected to slot {}", slot_id);
        self.slot_id.store(slot_id, Ordering::Release);
        self.connected.fetch_add(1, Ordering::Release);
    }
    
    fn on_disconnect(&self) {
        println!("[Client] Disconnected");
    }
    
    fn on_message(&self, data: &[u8]) {
        println!("[Client] Received: {:?}", String::from_utf8_lossy(data));
        self.messages.fetch_add(1, Ordering::Release);
        let mut guard = self.last_message.lock().unwrap();
        guard.clear();
        guard.extend_from_slice(data);
    }
    
    fn on_error(&self, err: ShmError) {
        println!("[Client] Error: {:?}", err);
    }
}

#[test]
fn test_multi_single_client_auto_slot() {
    let base_name = unique_name("SINGLE_AUTO");
    println!("[TEST] Base name: {}", base_name);
    
    // Запускаем сервер
    let server_handler = Arc::new(TestServerHandler::new());
    let server = MultiServer::start(&base_name, server_handler.clone(), MultiOptions::default())
        .expect("MultiServer start");
    
    thread::sleep(Duration::from_millis(100));
    
    // Клиент подключается к базовому имени — сервер назначит слот автоматически
    let client_handler = Arc::new(TestClientHandler::new());
    let client = MultiClient::connect(&base_name, client_handler.clone(), MultiClientOptions::default())
        .expect("MultiClient connect");
    
    // Ждём подключения
    assert!(client_handler.wait_for_connect(Duration::from_secs(5)), "Client should connect");
    assert!(server_handler.wait_for_connects(1, Duration::from_secs(5)), "Server should see client");
    
    // Проверяем что клиент получил слот
    let slot_id = client_handler.slot_id.load(Ordering::Acquire);
    println!("[TEST] Client got slot_id: {}", slot_id);
    assert!(slot_id < 10, "Slot ID should be valid");
    assert!(client.is_connected());
    
    // Клиент отправляет сообщение
    client.send(b"Hello from client").expect("Client send");
    assert!(server_handler.wait_for_messages(1, Duration::from_secs(2)), "Server should receive");
    
    // Сервер отправляет ответ
    server.send_to(slot_id, b"Hello from server").expect("Server send");
    assert!(client_handler.wait_for_messages(1, Duration::from_secs(2)), "Client should receive");
    
    println!("[TEST] Single client auto-slot: PASSED");
}

#[test]
fn test_multi_multiple_clients_auto_slot() {
    let base_name = unique_name("MULTI_AUTO");
    println!("[TEST] Base name: {}", base_name);
    
    // Запускаем сервер с 3 слотами
    let server_handler = Arc::new(TestServerHandler::new());
    let server = MultiServer::start(&base_name, server_handler.clone(), MultiOptions {
        max_clients: 3,
        ..Default::default()
    }).expect("MultiServer start");
    
    thread::sleep(Duration::from_millis(100));
    
    // Подключаем 3 клиента — все к одному базовому имени
    let mut clients = Vec::new();
    let mut client_handlers = Vec::new();
    
    for i in 0..3 {
        let ch = Arc::new(TestClientHandler::new());
        println!("[TEST] Connecting client {}...", i);
        let client = MultiClient::connect(&base_name, ch.clone(), MultiClientOptions::default())
            .expect(&format!("Client {} connect", i));
        
        // Ждём подключения каждого клиента
        assert!(ch.wait_for_connect(Duration::from_secs(5)), "Client {} should connect", i);
        
        clients.push(client);
        client_handlers.push(ch);
    }
    
    // Ждём пока сервер увидит всех
    assert!(server_handler.wait_for_connects(3, Duration::from_secs(5)), "All clients should connect");
    assert_eq!(server.client_count(), 3);
    
    // Проверяем что все клиенты получили разные слоты
    let mut slots: Vec<u32> = client_handlers.iter()
        .map(|h| h.slot_id.load(Ordering::Acquire))
        .collect();
    slots.sort();
    println!("[TEST] Assigned slots: {:?}", slots);
    assert_eq!(slots, vec![0, 1, 2], "Each client should get unique slot");
    
    // Broadcast от сервера
    let sent = server.broadcast(b"Broadcast to all").expect("Broadcast");
    assert_eq!(sent, 3);
    
    // Все клиенты должны получить
    for (i, ch) in client_handlers.iter().enumerate() {
        assert!(ch.wait_for_messages(1, Duration::from_secs(2)), "Client {} should receive broadcast", i);
    }
    
    // Каждый клиент отправляет сообщение
    for (i, client) in clients.iter().enumerate() {
        let msg = format!("Hello from client {}", i);
        client.send(msg.as_bytes()).expect(&format!("Client {} send", i));
    }
    
    // Сервер должен получить все
    assert!(server_handler.wait_for_messages(3, Duration::from_secs(2)), "Server should receive all");
    
    println!("[TEST] Multiple clients auto-slot: PASSED");
}

#[test]
fn test_multi_client_reconnect() {
    let base_name = unique_name("RECONN_AUTO");
    println!("[TEST] Base name: {}", base_name);
    
    let server_handler = Arc::new(TestServerHandler::new());
    let _server = MultiServer::start(&base_name, server_handler.clone(), MultiOptions::default())
        .expect("MultiServer start");
    
    thread::sleep(Duration::from_millis(100));
    
    // Первый клиент
    {
        let ch = Arc::new(TestClientHandler::new());
        let client = MultiClient::connect(&base_name, ch.clone(), MultiClientOptions::default())
            .expect("First client connect");
        
        assert!(ch.wait_for_connect(Duration::from_secs(5)));
        let slot1 = ch.slot_id.load(Ordering::Acquire);
        println!("[TEST] First client got slot: {}", slot1);
        
        client.send(b"First client").expect("First send");
        assert!(server_handler.wait_for_messages(1, Duration::from_secs(2)));
        
        // Клиент отключается при drop
    }
    
    thread::sleep(Duration::from_millis(500));
    
    // Второй клиент — должен получить тот же слот (или другой свободный)
    {
        let ch = Arc::new(TestClientHandler::new());
        let client = MultiClient::connect(&base_name, ch.clone(), MultiClientOptions::default())
            .expect("Second client connect");
        
        assert!(ch.wait_for_connect(Duration::from_secs(5)));
        let slot2 = ch.slot_id.load(Ordering::Acquire);
        println!("[TEST] Second client got slot: {}", slot2);
        
        client.send(b"Second client").expect("Second send");
        assert!(server_handler.wait_for_messages(2, Duration::from_secs(2)));
    }
    
    println!("[TEST] Client reconnect: PASSED");
}
