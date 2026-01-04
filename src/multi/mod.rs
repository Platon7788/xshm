//! Мультиклиентный сервер для xShm.
//!
//! Позволяет одному серверу обслуживать до N клиентов одновременно.
//! Каждый клиент получает свой выделенный SPSC канал.
//!
//! # Архитектура
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                      MultiServer                            │
//! │                   "BaseName" (base_name)                    │
//! ├─────────────────────────────────────────────────────────────┤
//! │  Slot 0: SharedServer "BaseName_0" ←→ Client A             │
//! │  Slot 1: SharedServer "BaseName_1" ←→ Client B             │
//! │  Slot 2: (свободен, ожидает подключения)                   │
//! │  ...                                                        │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Пример использования (Rust)
//!
//! ```ignore
//! use xshm::multi::{MultiServer, MultiHandler, MultiOptions};
//! use std::sync::Arc;
//!
//! struct MyHandler;
//!
//! impl MultiHandler for MyHandler {
//!     fn on_client_connect(&self, client_id: u32) {
//!         println!("Client {} connected", client_id);
//!     }
//!     fn on_client_disconnect(&self, client_id: u32) {
//!         println!("Client {} disconnected", client_id);
//!     }
//!     fn on_message(&self, client_id: u32, data: &[u8]) {
//!         println!("Message from {}: {:?}", client_id, data);
//!     }
//! }
//!
//! let server = MultiServer::start("MyService", Arc::new(MyHandler), MultiOptions::default())?;
//! server.send_to(0, b"Hello client 0")?;
//! server.broadcast(b"Hello everyone")?;
//! ```

mod ffi;

pub use ffi::*;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::error::{Result, ShmError};
use crate::server::SharedServer;
use crate::win;

/// Максимальное количество клиентов по умолчанию
pub const DEFAULT_MAX_CLIENTS: u32 = 10;

/// Callback-интерфейс для обработки событий мультиклиентного сервера
pub trait MultiHandler: Send + Sync + 'static {
    /// Вызывается при подключении нового клиента
    fn on_client_connect(&self, client_id: u32);
    
    /// Вызывается при отключении клиента
    fn on_client_disconnect(&self, client_id: u32);
    
    /// Вызывается при получении сообщения от клиента
    fn on_message(&self, client_id: u32, data: &[u8]);
    
    /// Вызывается при ошибке (client_id = None для общих ошибок)
    fn on_error(&self, client_id: Option<u32>, err: ShmError) {
        let _ = (client_id, err); // default: ignore
    }
}

/// Опции для MultiServer
#[derive(Clone)]
pub struct MultiOptions {
    /// Максимальное количество одновременных клиентов
    pub max_clients: u32,
    /// Таймаут ожидания событий в worker loop
    pub poll_timeout: Duration,
    /// Количество сообщений для обработки за один цикл
    pub recv_batch: usize,
}

impl Default for MultiOptions {
    fn default() -> Self {
        Self {
            max_clients: DEFAULT_MAX_CLIENTS,
            poll_timeout: Duration::from_millis(50),
            recv_batch: 32,
        }
    }
}

/// Состояние одного клиентского слота
struct ClientSlot {
    /// Уникальный ID клиента (совпадает с индексом слота)
    id: u32,
    /// Внутренний SharedServer для этого слота
    server: SharedServer,
    /// Подключён ли клиент
    connected: bool,
}

/// Мультиклиентный сервер
pub struct MultiServer {
    /// Базовое имя канала
    base_name: String,
    /// Слоты клиентов (каждый защищён своим Mutex)
    slots: RwLock<Vec<Mutex<ClientSlot>>>,
    /// Максимальное количество клиентов
    max_clients: u32,
    /// Счётчик для генерации уникальных ID (если слот переиспользуется)
    _next_id: AtomicU32,
    /// Флаг работы
    running: Arc<AtomicBool>,
    /// Worker thread handle
    worker_handle: Mutex<Option<JoinHandle<()>>>,
    /// Handler для callbacks
    handler: Arc<dyn MultiHandler>,
    /// Опции
    options: MultiOptions,
}

impl MultiServer {
    /// Запуск мультиклиентного сервера
    ///
    /// # Arguments
    /// * `base_name` - Базовое имя канала. Клиенты подключаются к "{base_name}_{slot_id}"
    /// * `handler` - Обработчик событий
    /// * `options` - Опции сервера
    pub fn start(
        base_name: &str,
        handler: Arc<dyn MultiHandler>,
        options: MultiOptions,
    ) -> Result<Arc<Self>> {
        let running = Arc::new(AtomicBool::new(true));
        let slots: RwLock<Vec<Mutex<ClientSlot>>> = RwLock::new(Vec::new());
        
        // Создаём слоты для всех возможных клиентов
        {
            let mut slots_guard = slots.write().unwrap();
            for slot_id in 0..options.max_clients {
                let channel_name = format!("{}_{}", base_name, slot_id);
                let server = SharedServer::start(&channel_name)?;
                
                slots_guard.push(Mutex::new(ClientSlot {
                    id: slot_id,
                    server,
                    connected: false,
                }));
            }
        }
        
        let server = Arc::new(Self {
            base_name: base_name.to_owned(),
            slots,
            max_clients: options.max_clients,
            _next_id: AtomicU32::new(0),
            running,
            worker_handle: Mutex::new(None),
            handler,
            options,
        });
        
        // Запускаем worker thread
        let server_clone = server.clone();
        let handle = thread::Builder::new()
            .name(format!("xshm-multi-{}", base_name))
            .spawn(move || {
                server_clone.worker_loop();
            })
            .map_err(|e| ShmError::WindowsError {
                code: e.raw_os_error().unwrap_or(-1) as u32,
                context: "spawn multi worker",
            })?;
        
        *server.worker_handle.lock().unwrap() = Some(handle);
        
        Ok(server)
    }
    
    /// Отправка сообщения конкретному клиенту
    pub fn send_to(&self, client_id: u32, data: &[u8]) -> Result<()> {
        let slots = self.slots.read().unwrap();
        let slot_mutex = slots.get(client_id as usize).ok_or(ShmError::NotConnected)?;
        let slot = slot_mutex.lock().unwrap();
        
        if !slot.connected {
            return Err(ShmError::NotConnected);
        }
        
        slot.server.send_to_client(data)?;
        Ok(())
    }
    
    /// Отправка сообщения всем подключённым клиентам
    pub fn broadcast(&self, data: &[u8]) -> Result<u32> {
        let slots = self.slots.read().unwrap();
        let mut sent_count = 0u32;
        
        for slot_mutex in slots.iter() {
            let slot = slot_mutex.lock().unwrap();
            if slot.connected {
                if slot.server.send_to_client(data).is_ok() {
                    sent_count += 1;
                }
            }
        }
        
        Ok(sent_count)
    }
    
    /// Принудительное отключение клиента
    pub fn disconnect_client(&self, client_id: u32) -> Result<()> {
        let slots = self.slots.read().unwrap();
        let slot_mutex = slots.get(client_id as usize).ok_or(ShmError::NotConnected)?;
        let mut slot = slot_mutex.lock().unwrap();
        
        if slot.connected {
            slot.connected = false;
            drop(slot);
            self.handler.on_client_disconnect(client_id);
        }
        
        Ok(())
    }
    
    /// Получение списка подключённых клиентов
    pub fn connected_clients(&self) -> Vec<u32> {
        let slots = self.slots.read().unwrap();
        slots
            .iter()
            .filter_map(|slot_mutex| {
                let slot = slot_mutex.lock().unwrap();
                if slot.connected { Some(slot.id) } else { None }
            })
            .collect()
    }
    
    /// Количество подключённых клиентов
    pub fn client_count(&self) -> u32 {
        let slots = self.slots.read().unwrap();
        slots
            .iter()
            .filter(|slot_mutex| {
                let slot = slot_mutex.lock().unwrap();
                slot.connected
            })
            .count() as u32
    }
    
    /// Проверка подключения конкретного клиента
    pub fn is_client_connected(&self, client_id: u32) -> bool {
        let slots = self.slots.read().unwrap();
        slots
            .get(client_id as usize)
            .map(|slot_mutex| {
                let slot = slot_mutex.lock().unwrap();
                slot.connected
            })
            .unwrap_or(false)
    }
    
    /// Остановка сервера
    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
    }
    
    /// Базовое имя канала
    pub fn base_name(&self) -> &str {
        &self.base_name
    }
    
    /// Получение имени канала для конкретного слота
    pub fn channel_name(&self, slot_id: u32) -> Option<String> {
        if slot_id < self.max_clients {
            Some(format!("{}_{}", self.base_name, slot_id))
        } else {
            None
        }
    }
    
    /// Worker loop — обрабатывает подключения и сообщения
    fn worker_loop(&self) {
        let mut buffer = Vec::with_capacity(crate::constants::MAX_MESSAGE_SIZE);
        
        while self.running.load(Ordering::Acquire) {
            // Собираем handles для ожидания
            let mut wait_handles: Vec<isize> = Vec::new();
            let mut handle_to_slot: Vec<(u32, HandleType)> = Vec::new();
            
            {
                let slots = self.slots.read().unwrap();
                for slot_mutex in slots.iter() {
                    let slot = slot_mutex.lock().unwrap();
                    let events = slot.server.events();
                    
                    if slot.connected {
                        // Для подключённых: ждём данные и disconnect
                        wait_handles.push(events.c2s.data.raw_handle());
                        handle_to_slot.push((slot.id, HandleType::Data));
                        
                        wait_handles.push(events.disconnect.raw_handle());
                        handle_to_slot.push((slot.id, HandleType::Disconnect));
                    } else {
                        // Для неподключённых: ждём connect_req
                        wait_handles.push(events.connect_req.raw_handle());
                        handle_to_slot.push((slot.id, HandleType::ConnectReq));
                    }
                }
            }
            
            if wait_handles.is_empty() {
                thread::sleep(self.options.poll_timeout);
                continue;
            }
            
            // Ожидаем любое событие
            match win::wait_any(&wait_handles, Some(self.options.poll_timeout)) {
                Ok(Some(index)) => {
                    if index < handle_to_slot.len() {
                        let (slot_id, handle_type) = handle_to_slot[index];
                        self.handle_event(slot_id, handle_type, &mut buffer);
                    }
                }
                Ok(None) => {
                    // Timeout — проверяем все слоты на наличие данных
                    self.poll_all_slots(&mut buffer);
                }
                Err(err) => {
                    self.handler.on_error(None, err);
                }
            }
        }
    }
    
    /// Обработка события для конкретного слота
    fn handle_event(&self, slot_id: u32, handle_type: HandleType, buffer: &mut Vec<u8>) {
        match handle_type {
            HandleType::ConnectReq => {
                self.handle_connect(slot_id);
            }
            HandleType::Data => {
                self.receive_from_slot(slot_id, buffer);
            }
            HandleType::Disconnect => {
                self.handle_disconnect(slot_id);
            }
        }
    }
    
    /// Обработка подключения нового клиента
    fn handle_connect(&self, slot_id: u32) {
        let slots = self.slots.read().unwrap();
        if let Some(slot_mutex) = slots.get(slot_id as usize) {
            let mut slot = slot_mutex.lock().unwrap();
            
            // connect_req уже получен в wait_any, выполняем handshake напрямую
            // Не вызываем wait_for_client — он будет ждать событие повторно
            match Self::do_handshake(&mut slot.server) {
                Ok(()) => {
                    slot.connected = true;
                    let id = slot.id;
                    drop(slot);
                    drop(slots);
                    self.handler.on_client_connect(id);
                }
                Err(_) => {
                    // Handshake не удался
                }
            }
        }
    }
    
    /// Выполнение handshake без ожидания события (событие уже получено)
    fn do_handshake(server: &mut SharedServer) -> Result<()> {
        use crate::constants::{HANDSHAKE_CLIENT_HELLO, HANDSHAKE_SERVER_READY};
        use std::sync::atomic::Ordering;
        
        if server.is_connected() {
            return Err(ShmError::AlreadyConnected);
        }
        
        // Проверяем состояние клиента (connect_req уже получен)
        let view = server.view();
        let control = view.control_block();
        let client_state = control.client_state.load(Ordering::Acquire);
        
        if client_state != HANDSHAKE_CLIENT_HELLO {
            return Err(ShmError::HandshakeFailed);
        }
        
        // Сбрасываем буферы и обновляем generation
        let current_gen = control.generation.load(Ordering::Acquire);
        let new_generation = current_gen.wrapping_add(1);
        
        unsafe {
            (&*view.ring_header_a()).reset(new_generation);
            (&*view.ring_header_b()).reset(new_generation);
        }
        
        control.generation.store(new_generation, Ordering::Release);
        
        unsafe {
            (&*view.ring_header_a())
                .handshake_state
                .store(HANDSHAKE_SERVER_READY, Ordering::Release);
            (&*view.ring_header_b())
                .handshake_state
                .store(HANDSHAKE_SERVER_READY, Ordering::Release);
        }
        
        control.server_state.store(HANDSHAKE_SERVER_READY, Ordering::Release);
        control.client_state.store(HANDSHAKE_SERVER_READY, Ordering::Release);
        
        // Отправляем ACK клиенту
        server.events().connect_ack.set()?;
        server.set_connected(true);
        
        Ok(())
    }
    
    /// Обработка отключения клиента
    fn handle_disconnect(&self, slot_id: u32) {
        let was_connected = {
            let slots = self.slots.read().unwrap();
            if let Some(slot_mutex) = slots.get(slot_id as usize) {
                let mut slot = slot_mutex.lock().unwrap();
                let was = slot.connected;
                slot.connected = false;
                slot.server.mark_disconnected();
                was
            } else {
                false
            }
        };
        
        if was_connected {
            self.handler.on_client_disconnect(slot_id);
        }
        
        // Примечание: слот остаётся с тем же сервером, но помечен как disconnected
        // При следующем connect_req будет выполнен новый handshake
        // Пересоздание сервера не требуется - SharedServer поддерживает reconnect
    }
    
    /// Получение сообщений от слота
    fn receive_from_slot(&self, slot_id: u32, buffer: &mut Vec<u8>) {
        let slots = self.slots.read().unwrap();
        if let Some(slot_mutex) = slots.get(slot_id as usize) {
            let slot = slot_mutex.lock().unwrap();
            if !slot.connected {
                return;
            }
            
            for _ in 0..self.options.recv_batch {
                match slot.server.receive_from_client(buffer) {
                    Ok(len) => {
                        // Нужно освободить lock перед вызовом handler
                        let data = buffer[..len].to_vec();
                        drop(slot);
                        drop(slots);
                        self.handler.on_message(slot_id, &data);
                        return; // После drop нельзя продолжать цикл
                    }
                    Err(ShmError::QueueEmpty) => break,
                    Err(err) => {
                        drop(slot);
                        drop(slots);
                        self.handler.on_error(Some(slot_id), err);
                        return;
                    }
                }
            }
        }
    }
    
    /// Проверка всех слотов на наличие данных
    fn poll_all_slots(&self, buffer: &mut Vec<u8>) {
        let slot_ids: Vec<u32> = {
            let slots = self.slots.read().unwrap();
            slots.iter()
                .filter_map(|slot_mutex| {
                    let slot = slot_mutex.lock().unwrap();
                    if slot.connected { Some(slot.id) } else { None }
                })
                .collect()
        };
        
        for slot_id in slot_ids {
            self.receive_from_slot(slot_id, buffer);
        }
    }
}

impl Drop for MultiServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        
        if let Some(handle) = self.worker_handle.lock().unwrap().take() {
            let _ = handle.join();
        }
    }
}

/// Тип события для handle
#[derive(Clone, Copy)]
enum HandleType {
    ConnectReq,
    Data,
    Disconnect,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    
    struct TestHandler {
        connects: AtomicU32,
        disconnects: AtomicU32,
        messages: AtomicU32,
    }
    
    impl TestHandler {
        fn new() -> Self {
            Self {
                connects: AtomicU32::new(0),
                disconnects: AtomicU32::new(0),
                messages: AtomicU32::new(0),
            }
        }
    }
    
    impl MultiHandler for TestHandler {
        fn on_client_connect(&self, _client_id: u32) {
            self.connects.fetch_add(1, Ordering::Relaxed);
        }
        
        fn on_client_disconnect(&self, _client_id: u32) {
            self.disconnects.fetch_add(1, Ordering::Relaxed);
        }
        
        fn on_message(&self, _client_id: u32, _data: &[u8]) {
            self.messages.fetch_add(1, Ordering::Relaxed);
        }
    }
    
    #[test]
    fn test_multi_server_start() {
        let handler = Arc::new(TestHandler::new());
        let server = MultiServer::start(
            "TEST_MULTI_START",
            handler,
            MultiOptions::default(),
        );
        
        assert!(server.is_ok());
        let server = server.unwrap();
        assert_eq!(server.client_count(), 0);
        assert_eq!(server.connected_clients().len(), 0);
    }
}
