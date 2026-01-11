//! FFI интерфейс для мультиклиентного сервера.
//!
//! C-совместимый API для использования MultiServer из C/C++.

use std::ffi::CStr;
use std::os::raw::{c_char, c_void};
use std::ptr::null_mut;
use std::sync::Arc;
use std::time::Duration;

use crate::error::ShmError;
use crate::ffi::shm_error_t;
use crate::multi::{MultiHandler, MultiOptions, MultiServer, DEFAULT_MAX_CLIENTS};

/// Опции для мультиклиентного сервера
#[repr(C)]
#[derive(Clone, Copy)]
pub struct shm_multi_options_t {
    /// Максимальное количество клиентов (по умолчанию 20)
    pub max_clients: u32,
    /// Таймаут ожидания событий в мс (по умолчанию 50)
    pub poll_timeout_ms: u32,
    /// Количество сообщений за один цикл (по умолчанию 32)
    pub recv_batch: u32,
}

impl Default for shm_multi_options_t {
    fn default() -> Self {
        Self {
            max_clients: DEFAULT_MAX_CLIENTS,
            poll_timeout_ms: 50,
            recv_batch: 32,
        }
    }
}

/// Callbacks для мультиклиентного сервера
#[repr(C)]
#[derive(Clone, Copy)]
pub struct shm_multi_callbacks_t {
    /// Вызывается при подключении клиента
    pub on_client_connect: Option<extern "C" fn(client_id: u32, user_data: *mut c_void)>,
    /// Вызывается при отключении клиента
    pub on_client_disconnect: Option<extern "C" fn(client_id: u32, user_data: *mut c_void)>,
    /// Вызывается при получении сообщения
    pub on_message: Option<extern "C" fn(
        client_id: u32,
        data: *const c_void,
        size: u32,
        user_data: *mut c_void,
    )>,
    /// Вызывается при ошибке (client_id = u32::MAX для общих ошибок)
    pub on_error: Option<extern "C" fn(
        client_id: u32,
        error: shm_error_t,
        user_data: *mut c_void,
    )>,
    /// Пользовательские данные, передаются во все callbacks
    pub user_data: *mut c_void,
}

impl Default for shm_multi_callbacks_t {
    fn default() -> Self {
        Self {
            on_client_connect: None,
            on_client_disconnect: None,
            on_message: None,
            on_error: None,
            user_data: null_mut(),
        }
    }
}

/// Handle мультиклиентного сервера
pub type MultiServerHandle = c_void;

/// Внутренний handler для FFI
struct FfiMultiHandler {
    callbacks: shm_multi_callbacks_t,
}

unsafe impl Send for FfiMultiHandler {}
unsafe impl Sync for FfiMultiHandler {}

impl MultiHandler for FfiMultiHandler {
    fn on_client_connect(&self, client_id: u32) {
        if let Some(cb) = self.callbacks.on_client_connect {
            cb(client_id, self.callbacks.user_data);
        }
    }

    fn on_client_disconnect(&self, client_id: u32) {
        if let Some(cb) = self.callbacks.on_client_disconnect {
            cb(client_id, self.callbacks.user_data);
        }
    }

    fn on_message(&self, client_id: u32, data: &[u8]) {
        if let Some(cb) = self.callbacks.on_message {
            cb(
                client_id,
                data.as_ptr() as *const c_void,
                data.len() as u32,
                self.callbacks.user_data,
            );
        }
    }

    fn on_error(&self, client_id: Option<u32>, err: ShmError) {
        if let Some(cb) = self.callbacks.on_error {
            cb(
                client_id.unwrap_or(u32::MAX),
                err.into(),
                self.callbacks.user_data,
            );
        }
    }
}

/// Внутреннее состояние сервера
struct MultiServerState {
    server: Arc<MultiServer>,
    _handler: Arc<FfiMultiHandler>,
}

fn to_rust_str(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let cstr = unsafe { CStr::from_ptr(ptr) };
    Some(cstr.to_string_lossy().into_owned())
}

// ============================================================================
// FFI Functions
// ============================================================================

/// Получить опции по умолчанию
#[unsafe(no_mangle)]
pub extern "C" fn shm_multi_options_default() -> shm_multi_options_t {
    shm_multi_options_t::default()
}

/// Получить callbacks по умолчанию (все NULL)
#[unsafe(no_mangle)]
pub extern "C" fn shm_multi_callbacks_default() -> shm_multi_callbacks_t {
    shm_multi_callbacks_t::default()
}

/// Запуск мультиклиентного сервера
///
/// # Parameters
/// - `base_name`: Базовое имя канала (клиенты подключаются к "{base_name}_{slot_id}")
/// - `callbacks`: Callbacks для событий
/// - `options`: Опции сервера (NULL для значений по умолчанию)
///
/// # Returns
/// Handle сервера или NULL при ошибке
#[unsafe(no_mangle)]
pub extern "C" fn shm_multi_server_start(
    base_name: *const c_char,
    callbacks: *const shm_multi_callbacks_t,
    options: *const shm_multi_options_t,
) -> *mut MultiServerHandle {
    let name = match to_rust_str(base_name) {
        Some(n) => n,
        None => return null_mut(),
    };

    let callbacks_val = if callbacks.is_null() {
        shm_multi_callbacks_t::default()
    } else {
        unsafe { *callbacks }
    };

    let opts = if options.is_null() {
        MultiOptions::default()
    } else {
        let o = unsafe { *options };
        MultiOptions {
            max_clients: o.max_clients,
            poll_timeout: Duration::from_millis(o.poll_timeout_ms as u64),
            recv_batch: o.recv_batch as usize,
        }
    };

    let handler = Arc::new(FfiMultiHandler {
        callbacks: callbacks_val,
    });

    match MultiServer::start(&name, handler.clone(), opts) {
        Ok(server) => {
            let state = Box::new(MultiServerState {
                server,
                _handler: handler,
            });
            Box::into_raw(state) as *mut MultiServerHandle
        }
        Err(err) => {
            if let Some(cb) = callbacks_val.on_error {
                cb(u32::MAX, err.into(), callbacks_val.user_data);
            }
            null_mut()
        }
    }
}

/// Отправка сообщения конкретному клиенту
///
/// # Parameters
/// - `handle`: Handle сервера
/// - `client_id`: ID клиента
/// - `data`: Данные для отправки
/// - `size`: Размер данных
///
/// # Returns
/// SHM_SUCCESS или код ошибки
#[unsafe(no_mangle)]
pub extern "C" fn shm_multi_server_send_to(
    handle: *mut MultiServerHandle,
    client_id: u32,
    data: *const c_void,
    size: u32,
) -> shm_error_t {
    if handle.is_null() || data.is_null() || size == 0 {
        return shm_error_t::SHM_ERROR_INVALID_PARAM;
    }

    let state = unsafe { &*(handle as *const MultiServerState) };
    let slice = unsafe { std::slice::from_raw_parts(data as *const u8, size as usize) };

    match state.server.send_to(client_id, slice) {
        Ok(()) => shm_error_t::SHM_SUCCESS,
        Err(err) => err.into(),
    }
}

/// Отправка сообщения всем подключённым клиентам
///
/// # Parameters
/// - `handle`: Handle сервера
/// - `data`: Данные для отправки
/// - `size`: Размер данных
/// - `sent_count`: (out) Количество клиентов, которым отправлено (может быть NULL)
///
/// # Returns
/// SHM_SUCCESS или код ошибки
#[unsafe(no_mangle)]
pub extern "C" fn shm_multi_server_broadcast(
    handle: *mut MultiServerHandle,
    data: *const c_void,
    size: u32,
    sent_count: *mut u32,
) -> shm_error_t {
    if handle.is_null() || data.is_null() || size == 0 {
        return shm_error_t::SHM_ERROR_INVALID_PARAM;
    }

    let state = unsafe { &*(handle as *const MultiServerState) };
    let slice = unsafe { std::slice::from_raw_parts(data as *const u8, size as usize) };

    match state.server.broadcast(slice) {
        Ok(count) => {
            if !sent_count.is_null() {
                unsafe { *sent_count = count };
            }
            shm_error_t::SHM_SUCCESS
        }
        Err(err) => err.into(),
    }
}

/// Отключение клиента
///
/// # Parameters
/// - `handle`: Handle сервера
/// - `client_id`: ID клиента для отключения
///
/// # Returns
/// SHM_SUCCESS или код ошибки
#[unsafe(no_mangle)]
pub extern "C" fn shm_multi_server_disconnect_client(
    handle: *mut MultiServerHandle,
    client_id: u32,
) -> shm_error_t {
    if handle.is_null() {
        return shm_error_t::SHM_ERROR_INVALID_PARAM;
    }

    let state = unsafe { &*(handle as *const MultiServerState) };

    match state.server.disconnect_client(client_id) {
        Ok(()) => shm_error_t::SHM_SUCCESS,
        Err(err) => err.into(),
    }
}

/// Получение количества подключённых клиентов
///
/// # Parameters
/// - `handle`: Handle сервера
///
/// # Returns
/// Количество подключённых клиентов или 0 при ошибке
#[unsafe(no_mangle)]
pub extern "C" fn shm_multi_server_client_count(handle: *const MultiServerHandle) -> u32 {
    if handle.is_null() {
        return 0;
    }

    let state = unsafe { &*(handle as *const MultiServerState) };
    state.server.client_count()
}

/// Проверка подключения конкретного клиента
///
/// # Parameters
/// - `handle`: Handle сервера
/// - `client_id`: ID клиента
///
/// # Returns
/// true если клиент подключён
#[unsafe(no_mangle)]
pub extern "C" fn shm_multi_server_is_client_connected(
    handle: *const MultiServerHandle,
    client_id: u32,
) -> bool {
    if handle.is_null() {
        return false;
    }

    let state = unsafe { &*(handle as *const MultiServerState) };
    state.server.is_client_connected(client_id)
}

/// Получение списка подключённых клиентов
///
/// # Parameters
/// - `handle`: Handle сервера
/// - `client_ids`: Буфер для записи ID клиентов
/// - `max_count`: Размер буфера
/// - `actual_count`: (out) Фактическое количество клиентов
///
/// # Returns
/// SHM_SUCCESS или код ошибки
#[unsafe(no_mangle)]
pub extern "C" fn shm_multi_server_get_clients(
    handle: *const MultiServerHandle,
    client_ids: *mut u32,
    max_count: u32,
    actual_count: *mut u32,
) -> shm_error_t {
    if handle.is_null() || actual_count.is_null() {
        return shm_error_t::SHM_ERROR_INVALID_PARAM;
    }

    let state = unsafe { &*(handle as *const MultiServerState) };
    let clients = state.server.connected_clients();

    unsafe { *actual_count = clients.len() as u32 };

    if !client_ids.is_null() && max_count > 0 {
        let copy_count = std::cmp::min(clients.len(), max_count as usize);
        unsafe {
            std::ptr::copy_nonoverlapping(clients.as_ptr(), client_ids, copy_count);
        }
    }

    shm_error_t::SHM_SUCCESS
}

/// Получение имени канала для конкретного слота
///
/// # Parameters
/// - `handle`: Handle сервера
/// - `slot_id`: ID слота
/// - `buffer`: Буфер для записи имени
/// - `buffer_size`: Размер буфера
///
/// # Returns
/// Длина имени (без null-терминатора) или 0 при ошибке
#[unsafe(no_mangle)]
pub extern "C" fn shm_multi_server_channel_name(
    handle: *const MultiServerHandle,
    slot_id: u32,
    buffer: *mut c_char,
    buffer_size: u32,
) -> u32 {
    if handle.is_null() {
        return 0;
    }

    let state = unsafe { &*(handle as *const MultiServerState) };

    match state.server.channel_name(slot_id) {
        Some(name) => {
            let name_bytes = name.as_bytes();
            let len = name_bytes.len();

            if !buffer.is_null() && buffer_size > 0 {
                let copy_len = std::cmp::min(len, (buffer_size - 1) as usize);
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        name_bytes.as_ptr(),
                        buffer as *mut u8,
                        copy_len,
                    );
                    *buffer.add(copy_len) = 0; // null-terminator
                }
            }

            len as u32
        }
        None => 0,
    }
}

/// Остановка мультиклиентного сервера
///
/// # Parameters
/// - `handle`: Handle сервера
#[unsafe(no_mangle)]
pub extern "C" fn shm_multi_server_stop(handle: *mut MultiServerHandle) {
    if handle.is_null() {
        return;
    }

    unsafe {
        let state = Box::from_raw(handle as *mut MultiServerState);
        state.server.stop();
        // state drops here, cleaning up
    }
}

// ============================================================================
// MultiClient FFI
// ============================================================================

use crate::multi::{MultiClient, MultiClientHandler, MultiClientOptions};
use crate::constants::SLOT_ID_NO_SLOT;

/// Опции для мультиклиента
#[repr(C)]
#[derive(Clone, Copy)]
pub struct shm_multi_client_options_t {
    /// Таймаут подключения к lobby в мс (по умолчанию 5000)
    pub lobby_timeout_ms: u32,
    /// Таймаут подключения к слоту в мс (по умолчанию 5000)
    pub slot_timeout_ms: u32,
    /// Таймаут ожидания событий в мс (по умолчанию 50)
    pub poll_timeout_ms: u32,
    /// Количество сообщений за один цикл (по умолчанию 32)
    pub recv_batch: u32,
}

impl Default for shm_multi_client_options_t {
    fn default() -> Self {
        Self {
            lobby_timeout_ms: 5000,
            slot_timeout_ms: 5000,
            poll_timeout_ms: 50,
            recv_batch: 32,
        }
    }
}

/// Callbacks для мультиклиента
#[repr(C)]
#[derive(Clone, Copy)]
pub struct shm_multi_client_callbacks_t {
    /// Вызывается при успешном подключении (slot_id — назначенный слот)
    pub on_connect: Option<extern "C" fn(slot_id: u32, user_data: *mut c_void)>,
    /// Вызывается при отключении
    pub on_disconnect: Option<extern "C" fn(user_data: *mut c_void)>,
    /// Вызывается при получении сообщения от сервера
    pub on_message: Option<extern "C" fn(data: *const c_void, size: u32, user_data: *mut c_void)>,
    /// Вызывается при ошибке
    pub on_error: Option<extern "C" fn(error: shm_error_t, user_data: *mut c_void)>,
    /// Пользовательские данные
    pub user_data: *mut c_void,
}

impl Default for shm_multi_client_callbacks_t {
    fn default() -> Self {
        Self {
            on_connect: None,
            on_disconnect: None,
            on_message: None,
            on_error: None,
            user_data: null_mut(),
        }
    }
}

/// Handle мультиклиента
pub type MultiClientHandle = c_void;

/// Внутренний handler для FFI клиента
struct FfiMultiClientHandler {
    callbacks: shm_multi_client_callbacks_t,
}

unsafe impl Send for FfiMultiClientHandler {}
unsafe impl Sync for FfiMultiClientHandler {}

impl MultiClientHandler for FfiMultiClientHandler {
    fn on_connect(&self, slot_id: u32) {
        if let Some(cb) = self.callbacks.on_connect {
            cb(slot_id, self.callbacks.user_data);
        }
    }

    fn on_disconnect(&self) {
        if let Some(cb) = self.callbacks.on_disconnect {
            cb(self.callbacks.user_data);
        }
    }

    fn on_message(&self, data: &[u8]) {
        if let Some(cb) = self.callbacks.on_message {
            cb(
                data.as_ptr() as *const c_void,
                data.len() as u32,
                self.callbacks.user_data,
            );
        }
    }

    fn on_error(&self, err: ShmError) {
        if let Some(cb) = self.callbacks.on_error {
            cb(err.into(), self.callbacks.user_data);
        }
    }
}

/// Внутреннее состояние клиента
struct MultiClientState {
    client: MultiClient,
    _handler: Arc<FfiMultiClientHandler>,
}

/// Получить опции клиента по умолчанию
#[unsafe(no_mangle)]
pub extern "C" fn shm_multi_client_options_default() -> shm_multi_client_options_t {
    shm_multi_client_options_t::default()
}

/// Получить callbacks клиента по умолчанию (все NULL)
#[unsafe(no_mangle)]
pub extern "C" fn shm_multi_client_callbacks_default() -> shm_multi_client_callbacks_t {
    shm_multi_client_callbacks_t::default()
}

/// Подключение мультиклиента к серверу
///
/// Клиент автоматически:
/// 1. Подключается к lobby (base_name)
/// 2. Получает назначенный slot_id от сервера
/// 3. Переподключается к слоту (base_name_N)
///
/// # Parameters
/// - `base_name`: Базовое имя канала (то же что у MultiServer)
/// - `callbacks`: Callbacks для событий
/// - `options`: Опции клиента (NULL для значений по умолчанию)
///
/// # Returns
/// Handle клиента или NULL при ошибке
#[unsafe(no_mangle)]
pub extern "C" fn shm_multi_client_connect(
    base_name: *const c_char,
    callbacks: *const shm_multi_client_callbacks_t,
    options: *const shm_multi_client_options_t,
) -> *mut MultiClientHandle {
    let name = match to_rust_str(base_name) {
        Some(n) => n,
        None => return null_mut(),
    };

    let callbacks_val = if callbacks.is_null() {
        shm_multi_client_callbacks_t::default()
    } else {
        unsafe { *callbacks }
    };

    let opts = if options.is_null() {
        MultiClientOptions::default()
    } else {
        let o = unsafe { *options };
        MultiClientOptions {
            lobby_timeout: Duration::from_millis(o.lobby_timeout_ms as u64),
            slot_timeout: Duration::from_millis(o.slot_timeout_ms as u64),
            poll_timeout: Duration::from_millis(o.poll_timeout_ms as u64),
            recv_batch: o.recv_batch as usize,
        }
    };

    let handler = Arc::new(FfiMultiClientHandler {
        callbacks: callbacks_val,
    });

    match MultiClient::connect(&name, handler.clone(), opts) {
        Ok(client) => {
            let state = Box::new(MultiClientState {
                client,
                _handler: handler,
            });
            Box::into_raw(state) as *mut MultiClientHandle
        }
        Err(err) => {
            if let Some(cb) = callbacks_val.on_error {
                cb(err.into(), callbacks_val.user_data);
            }
            null_mut()
        }
    }
}

/// Отправка сообщения серверу
///
/// # Parameters
/// - `handle`: Handle клиента
/// - `data`: Данные для отправки
/// - `size`: Размер данных
///
/// # Returns
/// SHM_SUCCESS или код ошибки
#[unsafe(no_mangle)]
pub extern "C" fn shm_multi_client_send(
    handle: *mut MultiClientHandle,
    data: *const c_void,
    size: u32,
) -> shm_error_t {
    if handle.is_null() || data.is_null() || size == 0 {
        return shm_error_t::SHM_ERROR_INVALID_PARAM;
    }

    let state = unsafe { &*(handle as *const MultiClientState) };
    let slice = unsafe { std::slice::from_raw_parts(data as *const u8, size as usize) };

    match state.client.send(slice) {
        Ok(()) => shm_error_t::SHM_SUCCESS,
        Err(err) => err.into(),
    }
}

/// Получение назначенного slot_id
///
/// # Parameters
/// - `handle`: Handle клиента
///
/// # Returns
/// slot_id или SLOT_ID_NO_SLOT (0xFFFFFFFF) если не подключён
#[unsafe(no_mangle)]
pub extern "C" fn shm_multi_client_slot_id(handle: *const MultiClientHandle) -> u32 {
    if handle.is_null() {
        return SLOT_ID_NO_SLOT;
    }

    let state = unsafe { &*(handle as *const MultiClientState) };
    state.client.slot_id()
}

/// Проверка подключения клиента
///
/// # Parameters
/// - `handle`: Handle клиента
///
/// # Returns
/// true если клиент подключён к слоту
#[unsafe(no_mangle)]
pub extern "C" fn shm_multi_client_is_connected(handle: *const MultiClientHandle) -> bool {
    if handle.is_null() {
        return false;
    }

    let state = unsafe { &*(handle as *const MultiClientState) };
    state.client.is_connected()
}

/// Отключение мультиклиента
///
/// # Parameters
/// - `handle`: Handle клиента
#[unsafe(no_mangle)]
pub extern "C" fn shm_multi_client_disconnect(handle: *mut MultiClientHandle) {
    if handle.is_null() {
        return;
    }

    unsafe {
        let state = Box::from_raw(handle as *mut MultiClientState);
        state.client.stop();
        // state drops here, cleaning up
    }
}
