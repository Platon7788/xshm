//! Мультиклиентный сервер для xShm.
//!
//! Позволяет одному серверу обслуживать до N клиентов одновременно.
//! Клиент подключается к базовому имени, сервер автоматически назначает слот.
//!
//! # Архитектура
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                      MultiServer                            │
//! │                   "BaseName" (base_name)                    │
//! ├─────────────────────────────────────────────────────────────┤
//! │  Lobby: "BaseName" — принимает connect_req от клиентов      │
//! │         → находит свободный слот                            │
//! │         → записывает slot_id в shared memory                │
//! │         → отправляет connect_ack                            │
//! ├─────────────────────────────────────────────────────────────┤
//! │  Slot 0: SharedServer "BaseName_0" ←→ Client A             │
//! │  Slot 1: SharedServer "BaseName_1" ←→ Client B             │
//! │  Slot 2: (свободен)                                        │
//! └─────────────────────────────────────────────────────────────┘
//! ```

mod ffi;

pub use ffi::*;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::client::SharedClient;
use crate::constants::{
    HANDSHAKE_CLIENT_HELLO, HANDSHAKE_IDLE, HANDSHAKE_SERVER_READY,
    MAX_MESSAGE_SIZE, RESERVED_SLOT_ID_INDEX, SLOT_ID_NO_SLOT,
};
use crate::error::{Result, ShmError};
use crate::events::SharedEvents;
use crate::naming::mapping_name;
use crate::server::SharedServer;
use crate::shared::SharedView;
use crate::win::{self, Mapping};

/// Максимальное количество клиентов по умолчанию
pub const DEFAULT_MAX_CLIENTS: u32 = 20;

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
        let _ = (client_id, err);
    }
}

/// Callback-интерфейс для MultiClient
pub trait MultiClientHandler: Send + Sync + 'static {
    /// Вызывается при успешном подключении (slot_id — назначенный слот)
    fn on_connect(&self, slot_id: u32);
    
    /// Вызывается при отключении
    fn on_disconnect(&self);
    
    /// Вызывается при получении сообщения от сервера
    fn on_message(&self, data: &[u8]);
    
    /// Вызывается при ошибке
    fn on_error(&self, err: ShmError) {
        let _ = err;
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

/// Опции для MultiClient
#[derive(Clone)]
pub struct MultiClientOptions {
    /// Таймаут подключения к lobby
    pub lobby_timeout: Duration,
    /// Таймаут подключения к слоту
    pub slot_timeout: Duration,
    /// Таймаут ожидания событий
    pub poll_timeout: Duration,
    /// Количество сообщений за один цикл
    pub recv_batch: usize,
}

impl Default for MultiClientOptions {
    fn default() -> Self {
        Self {
            lobby_timeout: Duration::from_secs(5),
            slot_timeout: Duration::from_secs(5),
            poll_timeout: Duration::from_millis(50),
            recv_batch: 32,
        }
    }
}

/// Состояние одного клиентского слота
struct ClientSlot {
    id: u32,
    server: SharedServer,
    connected: bool,
}

/// Lobby — принимает подключения и назначает слоты
struct Lobby {
    #[allow(dead_code)]
    mapping: Mapping,
    view: SharedView,
    events: SharedEvents,
}

impl Lobby {
    fn create(base_name: &str) -> Result<Self> {
        let map_name = mapping_name(base_name);
        let mapping = Mapping::create(&map_name)?;
        let view = unsafe { SharedView::new(mapping.as_ptr()) };
        
        let control = view.control_block_mut();
        control.reset();
        
        let events = SharedEvents::create(base_name)?;
        
        Ok(Self { mapping, view, events })
    }
    
    fn events(&self) -> &SharedEvents {
        &self.events
    }
    
    fn view(&self) -> &SharedView {
        &self.view
    }
}

/// Мультиклиентный сервер
pub struct MultiServer {
    base_name: String,
    lobby: Lobby,
    slots: RwLock<Vec<Mutex<ClientSlot>>>,
    max_clients: u32,
    running: Arc<AtomicBool>,
    worker_handle: Mutex<Option<JoinHandle<()>>>,
    handler: Arc<dyn MultiHandler>,
    options: MultiOptions,
}

impl MultiServer {
    /// Запуск мультиклиентного сервера
    pub fn start(
        base_name: &str,
        handler: Arc<dyn MultiHandler>,
        options: MultiOptions,
    ) -> Result<Arc<Self>> {
        let running = Arc::new(AtomicBool::new(true));
        
        // Создаём lobby для приёма подключений
        let lobby = Lobby::create(base_name)?;
        
        // Создаём слоты
        let slots: RwLock<Vec<Mutex<ClientSlot>>> = RwLock::new(Vec::new());
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
            lobby,
            slots,
            max_clients: options.max_clients,
            running,
            worker_handle: Mutex::new(None),
            handler,
            options,
        });
        
        // Запускаем worker thread
        let server_clone = server.clone();
        let handle = thread::Builder::new()
            .name(format!("xshm-multi-{}", base_name))
            .spawn(move || server_clone.worker_loop())
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
        slots.iter()
            .filter_map(|slot_mutex| {
                let slot = slot_mutex.lock().unwrap();
                if slot.connected { Some(slot.id) } else { None }
            })
            .collect()
    }
    
    /// Количество подключённых клиентов
    pub fn client_count(&self) -> u32 {
        let slots = self.slots.read().unwrap();
        slots.iter()
            .filter(|slot_mutex| slot_mutex.lock().unwrap().connected)
            .count() as u32
    }
    
    /// Проверка подключения конкретного клиента
    pub fn is_client_connected(&self, client_id: u32) -> bool {
        let slots = self.slots.read().unwrap();
        slots.get(client_id as usize)
            .map(|slot_mutex| slot_mutex.lock().unwrap().connected)
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

    /// Найти свободный слот
    fn find_free_slot(&self) -> Option<u32> {
        let slots = self.slots.read().unwrap();
        for slot_mutex in slots.iter() {
            let slot = slot_mutex.lock().unwrap();
            if !slot.connected {
                return Some(slot.id);
            }
        }
        None
    }
    
    /// Worker loop — обрабатывает lobby и слоты
    fn worker_loop(&self) {
        let mut buffer = Vec::with_capacity(MAX_MESSAGE_SIZE);
        
        while self.running.load(Ordering::Acquire) {
            // Собираем handles для ожидания
            let mut wait_handles: Vec<isize> = Vec::new();
            let mut handle_to_event: Vec<EventSource> = Vec::new();
            
            // Lobby connect_req — всегда слушаем
            wait_handles.push(self.lobby.events().connect_req.raw_handle());
            handle_to_event.push(EventSource::LobbyConnect);
            
            // Слоты
            {
                let slots = self.slots.read().unwrap();
                for slot_mutex in slots.iter() {
                    let slot = slot_mutex.lock().unwrap();
                    let events = slot.server.events();
                    
                    if slot.connected {
                        // Данные от клиента
                        wait_handles.push(events.c2s.data.raw_handle());
                        handle_to_event.push(EventSource::SlotData(slot.id));
                        
                        // Disconnect
                        wait_handles.push(events.disconnect.raw_handle());
                        handle_to_event.push(EventSource::SlotDisconnect(slot.id));
                    } else {
                        // Ожидаем connect_req на слоте (после lobby handshake)
                        wait_handles.push(events.connect_req.raw_handle());
                        handle_to_event.push(EventSource::SlotConnect(slot.id));
                    }
                }
            }
            
            // Ожидаем любое событие
            match win::wait_any(&wait_handles, Some(self.options.poll_timeout)) {
                Ok(Some(index)) => {
                    if index < handle_to_event.len() {
                        self.handle_event(&handle_to_event[index], &mut buffer);
                    }
                }
                Ok(None) => {
                    // Timeout — проверяем все слоты на данные
                    self.poll_all_slots(&mut buffer);
                }
                Err(err) => {
                    self.handler.on_error(None, err);
                }
            }
        }
    }

    /// Обработка события
    fn handle_event(&self, source: &EventSource, buffer: &mut Vec<u8>) {
        match source {
            EventSource::LobbyConnect => self.handle_lobby_connect(),
            EventSource::SlotConnect(slot_id) => self.handle_slot_connect(*slot_id),
            EventSource::SlotData(slot_id) => self.receive_from_slot(*slot_id, buffer),
            EventSource::SlotDisconnect(slot_id) => self.handle_slot_disconnect(*slot_id),
        }
    }
    
    /// Обработка подключения через lobby
    fn handle_lobby_connect(&self) {
        let control = self.lobby.view().control_block();
        let client_state = control.client_state.load(Ordering::Acquire);
        
        if client_state != HANDSHAKE_CLIENT_HELLO {
            return;
        }
        
        // Находим свободный слот
        let slot_id = self.find_free_slot().unwrap_or(SLOT_ID_NO_SLOT);
        
        // Записываем slot_id в reserved[0]
        control.reserved[RESERVED_SLOT_ID_INDEX].store(slot_id, Ordering::Release);
        
        // Обновляем состояние
        control.server_state.store(HANDSHAKE_SERVER_READY, Ordering::Release);
        
        // Отправляем ACK
        let _ = self.lobby.events().connect_ack.set();
        
        // Сбрасываем состояние lobby для следующего клиента
        control.client_state.store(HANDSHAKE_IDLE, Ordering::Release);
        control.server_state.store(HANDSHAKE_IDLE, Ordering::Release);
    }
    
    /// Обработка подключения к слоту (после lobby)
    fn handle_slot_connect(&self, slot_id: u32) {
        let slots = self.slots.read().unwrap();
        if let Some(slot_mutex) = slots.get(slot_id as usize) {
            let mut slot = slot_mutex.lock().unwrap();
            
            if slot.connected {
                return; // Уже подключён
            }
            
            // Выполняем handshake
            match Self::do_slot_handshake(&mut slot.server) {
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

    /// Выполнение handshake на слоте
    fn do_slot_handshake(server: &mut SharedServer) -> Result<()> {
        if server.is_connected() {
            return Err(ShmError::AlreadyConnected);
        }
        
        let view = server.view();
        let control = view.control_block();
        let client_state = control.client_state.load(Ordering::Acquire);
        
        if client_state != HANDSHAKE_CLIENT_HELLO {
            return Err(ShmError::HandshakeFailed);
        }
        
        // Сбрасываем буферы
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
        
        server.events().connect_ack.set()?;
        server.set_connected(true);
        
        Ok(())
    }
    
    /// Обработка отключения клиента
    fn handle_slot_disconnect(&self, slot_id: u32) {
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
                        let data = buffer[..len].to_vec();
                        drop(slot);
                        drop(slots);
                        self.handler.on_message(slot_id, &data);
                        return;
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
    
    /// Проверка всех слотов на данные
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

/// Источник события для worker loop
#[derive(Clone, Copy)]
enum EventSource {
    LobbyConnect,
    SlotConnect(u32),
    SlotData(u32),
    SlotDisconnect(u32),
}

// ============================================================================
// MultiClient — клиент с автоматическим назначением слота
// ============================================================================

use std::sync::mpsc::{self, Receiver, Sender};

enum ClientCommand {
    Send(Vec<u8>),
    Shutdown,
}

/// Мультиклиент — подключается к базовому имени, получает слот автоматически
pub struct MultiClient {
    cmd_tx: Sender<ClientCommand>,
    join: Mutex<Option<JoinHandle<()>>>,
    running: Arc<AtomicBool>,
    slot_id: Arc<AtomicU32>,
}

impl MultiClient {
    /// Подключение к мультисерверу
    /// 
    /// Клиент автоматически:
    /// 1. Подключается к lobby (base_name)
    /// 2. Получает назначенный slot_id
    /// 3. Переподключается к слоту (base_name_N)
    pub fn connect(
        base_name: &str,
        handler: Arc<dyn MultiClientHandler>,
        options: MultiClientOptions,
    ) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let slot_id = Arc::new(AtomicU32::new(SLOT_ID_NO_SLOT));
        
        let running_clone = running.clone();
        let slot_id_clone = slot_id.clone();
        let name = base_name.to_owned();
        
        let handle = thread::Builder::new()
            .name(format!("xshm-multi-client-{}", base_name))
            .spawn(move || {
                client_worker(&name, handler, options, rx, running_clone, slot_id_clone);
            })
            .map_err(|e| ShmError::WindowsError {
                code: e.raw_os_error().unwrap_or(-1) as u32,
                context: "spawn multi client worker",
            })?;
        
        Ok(Self {
            cmd_tx: tx,
            join: Mutex::new(Some(handle)),
            running,
            slot_id,
        })
    }
    
    /// Отправка сообщения серверу
    pub fn send(&self, data: &[u8]) -> Result<()> {
        if !self.running.load(Ordering::Acquire) {
            return Err(ShmError::NotReady);
        }
        self.cmd_tx
            .send(ClientCommand::Send(data.to_vec()))
            .map_err(|_| ShmError::NotReady)
    }
    
    /// Получить назначенный slot_id (SLOT_ID_NO_SLOT если не подключён)
    pub fn slot_id(&self) -> u32 {
        self.slot_id.load(Ordering::Acquire)
    }
    
    /// Проверка подключения
    pub fn is_connected(&self) -> bool {
        self.slot_id.load(Ordering::Acquire) != SLOT_ID_NO_SLOT
    }
    
    /// Остановка клиента
    pub fn stop(&self) {
        let _ = self.cmd_tx.send(ClientCommand::Shutdown);
    }
}

impl Drop for MultiClient {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        let _ = self.cmd_tx.send(ClientCommand::Shutdown);
        if let Some(handle) = self.join.lock().unwrap().take() {
            let _ = handle.join();
        }
    }
}

/// Worker для MultiClient
fn client_worker(
    base_name: &str,
    handler: Arc<dyn MultiClientHandler>,
    options: MultiClientOptions,
    cmd_rx: Receiver<ClientCommand>,
    running: Arc<AtomicBool>,
    slot_id_out: Arc<AtomicU32>,
) {
    let mut buffer = Vec::with_capacity(MAX_MESSAGE_SIZE);
    
    while running.load(Ordering::Acquire) {
        // Шаг 1: Подключаемся к lobby и получаем slot_id
        let slot_id = match lobby_handshake(base_name, &options) {
            Ok(id) => id,
            Err(err) => {
                handler.on_error(err);
                if !wait_delay(&running, options.poll_timeout) {
                    break;
                }
                continue;
            }
        };
        
        if slot_id == SLOT_ID_NO_SLOT {
            handler.on_error(ShmError::NoFreeSlot);
            if !wait_delay(&running, options.poll_timeout) {
                break;
            }
            continue;
        }
        
        // Шаг 2: Подключаемся к назначенному слоту
        let slot_name = format!("{}_{}", base_name, slot_id);
        let client = match SharedClient::connect(&slot_name, options.slot_timeout) {
            Ok(c) => c,
            Err(err) => {
                handler.on_error(err);
                if !wait_delay(&running, options.poll_timeout) {
                    break;
                }
                continue;
            }
        };
        
        slot_id_out.store(slot_id, Ordering::Release);
        handler.on_connect(slot_id);
        
        // Шаг 3: Работаем с данными
        let handles = [
            client.events().disconnect.raw_handle(),
            client.events().s2c.data.raw_handle(),
        ];
        
        let mut send_queue: Vec<Vec<u8>> = Vec::new();
        
        loop {
            if !running.load(Ordering::Acquire) {
                break;
            }
            
            // Обрабатываем команды
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    ClientCommand::Send(data) => send_queue.push(data),
                    ClientCommand::Shutdown => {
                        running.store(false, Ordering::Release);
                    }
                }
            }
            
            // Отправляем данные
            while let Some(data) = send_queue.first() {
                match client.send_to_server(data) {
                    Ok(_) => { send_queue.remove(0); }
                    Err(ShmError::QueueFull) => break,
                    Err(err) => {
                        handler.on_error(err);
                        break;
                    }
                }
            }
            
            // Получаем данные
            loop {
                match client.receive_from_server(&mut buffer) {
                    Ok(len) => handler.on_message(&buffer[..len]),
                    Err(ShmError::QueueEmpty) => break,
                    Err(err) => {
                        handler.on_error(err);
                        break;
                    }
                }
            }
            
            // Ожидаем события
            match win::wait_any(&handles, Some(options.poll_timeout)) {
                Ok(Some(0)) => {
                    // Disconnect
                    slot_id_out.store(SLOT_ID_NO_SLOT, Ordering::Release);
                    handler.on_disconnect();
                    break;
                }
                Ok(Some(1)) => {
                    // Data available — продолжаем цикл
                }
                Ok(_) => {}
                Err(err) => {
                    handler.on_error(err);
                    slot_id_out.store(SLOT_ID_NO_SLOT, Ordering::Release);
                    handler.on_disconnect();
                    break;
                }
            }
        }
        
        if !wait_delay(&running, options.poll_timeout) {
            break;
        }
    }
}

/// Handshake с lobby для получения slot_id
fn lobby_handshake(base_name: &str, options: &MultiClientOptions) -> Result<u32> {
    // Открываем lobby mapping
    let map_name = mapping_name(base_name);
    let mapping = Mapping::open(&map_name)?;
    let view = unsafe { SharedView::new(mapping.as_ptr()) };
    
    // Открываем события
    let events = SharedEvents::open(base_name)?;
    
    // Отправляем connect_req
    let control = view.control_block();
    control.client_state.store(HANDSHAKE_CLIENT_HELLO, Ordering::Release);
    events.connect_req.set()?;
    
    // Ждём connect_ack
    if !events.connect_ack.wait(Some(options.lobby_timeout))? {
        control.client_state.store(HANDSHAKE_IDLE, Ordering::Release);
        return Err(ShmError::Timeout);
    }
    
    // Читаем slot_id из reserved[0]
    let slot_id = control.reserved[RESERVED_SLOT_ID_INDEX].load(Ordering::Acquire);
    
    // Сбрасываем состояние
    control.client_state.store(HANDSHAKE_IDLE, Ordering::Release);
    
    Ok(slot_id)
}

fn wait_delay(running: &AtomicBool, delay: Duration) -> bool {
    if delay.is_zero() {
        return running.load(Ordering::Acquire);
    }
    let deadline = std::time::Instant::now() + delay;
    while std::time::Instant::now() < deadline {
        if !running.load(Ordering::Acquire) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    running.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;
    
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
        let server = MultiServer::start("TEST_MULTI_START", handler, MultiOptions::default());
        assert!(server.is_ok());
        let server = server.unwrap();
        assert_eq!(server.client_count(), 0);
    }
}
