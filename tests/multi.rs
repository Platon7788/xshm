//! Тесты мультиклиентного сервера

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use xshm::auto::{AutoClient, AutoHandler as SingleAutoHandler, AutoOptions, ChannelKind};
use xshm::multi::{MultiHandler, MultiOptions, MultiServer};

fn unique_name(tag: &str) -> String {
    use std::time::SystemTime;
    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("XSHM_MULTI_{}_{}_{}", tag, std::process::id(), ts % 1_000_000)
}

/// Handler для тестов MultiServer
struct TestMultiHandler {
    connects: AtomicU32,
    disconnects: AtomicU32,
    messages: AtomicU32,
    last_message: Mutex<Vec<u8>>,
    notify: Condvar,
}

impl TestMultiHandler {
    fn new() -> Self {
        Self {
            connects: AtomicU32::new(0),
            disconnects: AtomicU32::new(0),
            messages: AtomicU32::new(0),
            last_message: Mutex::new(Vec::new()),
            notify: Condvar::new(),
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

impl MultiHandler for TestMultiHandler {
    fn on_client_connect(&self, client_id: u32) {
        println!("[MultiServer] Client {} connected", client_id);
        self.connects.fetch_add(1, Ordering::Release);
    }
    
    fn on_client_disconnect(&self, client_id: u32) {
        println!("[MultiServer] Client {} disconnected", client_id);
        self.disconnects.fetch_add(1, Ordering::Release);
    }
    
    fn on_message(&self, client_id: u32, data: &[u8]) {
        println!("[MultiServer] Message from client {}: {:?}", client_id, data);
        self.messages.fetch_add(1, Ordering::Release);
        let mut guard = self.last_message.lock().unwrap();
        guard.clear();
        guard.extend_from_slice(data);
        self.notify.notify_all();
    }
    
    fn on_error(&self, client_id: Option<u32>, err: xshm::ShmError) {
        println!("[MultiServer] Error for client {:?}: {:?}", client_id, err);
    }
}

/// Handler для тестовых клиентов
struct TestClientHandler {
    messages: AtomicU32,
    last_message: Mutex<Vec<u8>>,
}

impl TestClientHandler {
    fn new() -> Self {
        Self {
            messages: AtomicU32::new(0),
            last_message: Mutex::new(Vec::new()),
        }
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

impl SingleAutoHandler for TestClientHandler {
    fn on_connect(&self) {
        println!("[Client] Connected");
    }
    
    fn on_disconnect(&self) {
        println!("[Client] Disconnected");
    }
    
    fn on_message(&self, _direction: ChannelKind, data: &[u8]) {
        println!("[Client] Received: {:?}", data);
        self.messages.fetch_add(1, Ordering::Release);
        let mut guard = self.last_message.lock().unwrap();
        guard.clear();
        guard.extend_from_slice(data);
    }
}

#[test]
fn test_multi_server_single_client() {
    let base_name = unique_name("SINGLE");
    println!("[TEST] Base name: {}", base_name);
    
    let handler = Arc::new(TestMultiHandler::new());
    let server = MultiServer::start(&base_name, handler.clone(), MultiOptions::default())
        .expect("MultiServer start");
    
    // Даём серверу время инициализироваться
    thread::sleep(Duration::from_millis(200));
    
    // Подключаем клиента к слоту 0
    let client_handler = Arc::new(TestClientHandler::new());
    let channel_name = server.channel_name(0).unwrap();
    println!("[TEST] Connecting client to: {}", channel_name);
    
    let client = AutoClient::connect(&channel_name, client_handler.clone(), AutoOptions::default())
        .expect("Client connect");
    
    println!("[TEST] Client created, waiting for connect callback...");
    
    // Ждём подключения
    let connected = handler.wait_for_connects(1, Duration::from_secs(5));
    println!("[TEST] wait_for_connects returned: {}, connects={}", connected, handler.connects.load(Ordering::Relaxed));
    
    assert!(connected, "Client should connect");
    assert_eq!(server.client_count(), 1);
    assert!(server.is_client_connected(0));
    
    // Клиент отправляет сообщение
    client.send(b"Hello from client").expect("Client send");
    
    // Ждём сообщения на сервере
    assert!(handler.wait_for_messages(1, Duration::from_secs(2)), "Server should receive message");
    
    // Сервер отправляет ответ
    server.send_to(0, b"Hello from server").expect("Server send");
    
    // Ждём ответа на клиенте
    assert!(client_handler.wait_for_messages(1, Duration::from_secs(2)), "Client should receive response");
    
    // Отключаем клиента
    drop(client);
    thread::sleep(Duration::from_millis(200));
    
    println!("Test passed: single client");
}

#[test]
fn test_multi_server_multiple_clients() {
    let base_name = unique_name("MULTI");
    
    let handler = Arc::new(TestMultiHandler::new());
    let server = MultiServer::start(&base_name, handler.clone(), MultiOptions {
        max_clients: 3,
        ..Default::default()
    }).expect("MultiServer start");
    
    thread::sleep(Duration::from_millis(100));
    
    // Подключаем 3 клиента
    let mut clients = Vec::new();
    let mut client_handlers = Vec::new();
    
    for i in 0..3 {
        let ch = Arc::new(TestClientHandler::new());
        let channel_name = server.channel_name(i).unwrap();
        let client = AutoClient::connect(&channel_name, ch.clone(), AutoOptions::default())
            .expect(&format!("Client {} connect", i));
        clients.push(client);
        client_handlers.push(ch);
        thread::sleep(Duration::from_millis(50));
    }
    
    // Ждём подключения всех
    assert!(handler.wait_for_connects(3, Duration::from_secs(5)), "All clients should connect");
    assert_eq!(server.client_count(), 3);
    
    // Broadcast от сервера
    let sent = server.broadcast(b"Broadcast message").expect("Broadcast");
    assert_eq!(sent, 3);
    
    // Ждём сообщения на всех клиентах
    for (i, ch) in client_handlers.iter().enumerate() {
        assert!(ch.wait_for_messages(1, Duration::from_secs(2)), 
            "Client {} should receive broadcast", i);
    }
    
    // Каждый клиент отправляет сообщение
    for (i, client) in clients.iter().enumerate() {
        let msg = format!("Hello from client {}", i);
        client.send(msg.as_bytes()).expect(&format!("Client {} send", i));
    }
    
    // Ждём все сообщения на сервере
    assert!(handler.wait_for_messages(3, Duration::from_secs(2)), "Server should receive all messages");
    
    // Отправка конкретному клиенту
    server.send_to(1, b"Private message to client 1").expect("Send to client 1");
    
    // Только клиент 1 должен получить
    assert!(client_handlers[1].wait_for_messages(2, Duration::from_secs(2)), 
        "Client 1 should receive private message");
    
    println!("Test passed: multiple clients");
}

#[test]
fn test_multi_server_client_reconnect() {
    let base_name = unique_name("RECONN");
    
    let handler = Arc::new(TestMultiHandler::new());
    let server = MultiServer::start(&base_name, handler.clone(), MultiOptions::default())
        .expect("MultiServer start");
    
    thread::sleep(Duration::from_millis(100));
    
    // Первое подключение
    let channel_name = server.channel_name(0).unwrap();
    {
        let ch = Arc::new(TestClientHandler::new());
        let client = AutoClient::connect(&channel_name, ch, AutoOptions::default())
            .expect("First client connect");
        
        assert!(handler.wait_for_connects(1, Duration::from_secs(2)));
        
        client.send(b"First client").expect("First send");
        assert!(handler.wait_for_messages(1, Duration::from_secs(2)));
        
        // Клиент отключается при drop
    }
    
    thread::sleep(Duration::from_millis(500));
    
    // Второе подключение к тому же слоту
    {
        let ch = Arc::new(TestClientHandler::new());
        let client = AutoClient::connect(&channel_name, ch, AutoOptions::default())
            .expect("Second client connect");
        
        assert!(handler.wait_for_connects(2, Duration::from_secs(2)));
        
        client.send(b"Second client").expect("Second send");
        assert!(handler.wait_for_messages(2, Duration::from_secs(2)));
    }
    
    println!("Test passed: client reconnect");
}
