//! FFI-интерфейс совместимый с существующей C-библиотекой.
//!
//! All public functions in this module accept raw pointers from C callers.
//! The `not_unsafe_ptr_arg_deref` lint is suppressed at the module level
//! because every FFI function validates its pointer arguments before use.

use std::ffi::CStr;
use std::os::raw::{c_char, c_void};
use std::ptr::null_mut;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::auto::{
    AutoClient, AutoHandler, AutoOptions, AutoServer, AutoStatsSnapshot, ChannelKind,
};
use crate::client::SharedClient;
use crate::constants::MAX_MESSAGE_SIZE;
use crate::error::{Result, ShmError};
use crate::server::SharedServer;

#[repr(C)]
pub struct shm_endpoint_config_t {
    pub name: *const c_char,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct shm_callbacks_t {
    pub on_connect: Option<extern "C" fn(user_data: *mut c_void)>,
    pub on_disconnect: Option<extern "C" fn(user_data: *mut c_void)>,
    pub on_data_available: Option<extern "C" fn(user_data: *mut c_void)>,
    pub on_space_available: Option<extern "C" fn(user_data: *mut c_void)>,
    pub on_error: Option<extern "C" fn(error: shm_error_t, user_data: *mut c_void)>,
    pub user_data: *mut c_void,
    pub on_message: Option<
        extern "C" fn(
            direction: shm_direction_t,
            data: *const c_void,
            size: u32,
            user_data: *mut c_void,
        ),
    >,
    pub on_overflow:
        Option<extern "C" fn(direction: shm_direction_t, dropped: u32, user_data: *mut c_void)>,
}

#[repr(C)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum shm_error_t {
    SHM_SUCCESS = 0,
    SHM_ERROR_INVALID_PARAM = -1,
    SHM_ERROR_MEMORY = -2,
    SHM_ERROR_TIMEOUT = -3,
    SHM_ERROR_EMPTY = -4,
    SHM_ERROR_EXISTS = -5,
    SHM_ERROR_NOT_FOUND = -6,
    SHM_ERROR_ACCESS = -7,
    SHM_ERROR_NOT_READY = -8,
    SHM_ERROR_PROTOCOL = -9,
    SHM_ERROR_FULL = -10,
    SHM_ERROR_NO_SLOT = -11,
}

impl From<ShmError> for shm_error_t {
    fn from(value: ShmError) -> Self {
        match value {
            ShmError::MessageTooSmall | ShmError::MessageTooLarge => {
                shm_error_t::SHM_ERROR_INVALID_PARAM
            }
            ShmError::QueueEmpty => shm_error_t::SHM_ERROR_EMPTY,
            ShmError::QueueFull => shm_error_t::SHM_ERROR_FULL,
            ShmError::Timeout => shm_error_t::SHM_ERROR_TIMEOUT,
            ShmError::NotReady => shm_error_t::SHM_ERROR_NOT_READY,
            ShmError::NotConnected => shm_error_t::SHM_ERROR_NOT_FOUND,
            ShmError::AlreadyConnected => shm_error_t::SHM_ERROR_EXISTS,
            ShmError::HandshakeFailed | ShmError::Corrupted => shm_error_t::SHM_ERROR_PROTOCOL,
            ShmError::WindowsError { .. } => shm_error_t::SHM_ERROR_ACCESS,
            ShmError::InvalidConfig(_) => shm_error_t::SHM_ERROR_INVALID_PARAM,
            ShmError::NoFreeSlot => shm_error_t::SHM_ERROR_NO_SLOT,
        }
    }
}

#[cfg(test)]
mod error_mapping_tests {
    use super::*;

    /// NotConnected и NotReady раньше делили один shm_error_t (SHM_ERROR_NOT_READY),
    /// теряя диагностическую разницу между «нет соединения» и «handshake ещё
    /// не готов». Теперь у каждого свой код.
    #[test]
    fn not_connected_and_not_ready_have_distinct_codes() {
        let not_connected: shm_error_t = ShmError::NotConnected.into();
        let not_ready: shm_error_t = ShmError::NotReady.into();
        assert_ne!(not_connected, not_ready);
        assert_eq!(not_connected, shm_error_t::SHM_ERROR_NOT_FOUND);
        assert_eq!(not_ready, shm_error_t::SHM_ERROR_NOT_READY);
    }
}

#[cfg(test)]
mod recv_cache_tests {
    use super::*;
    use std::ffi::CString;
    use std::thread;
    use std::time::Duration as StdDuration;

    fn unique_name(tag: &str) -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("XSHM_FFI_TEST_{}_{}_{}", tag, std::process::id(), n)
    }

    /// Раньше сообщение вычитывалось из ring buffer ДО проверки, помещается
    /// ли оно в буфер C-вызывающего — при слишком малом буфере сообщение
    /// терялось безвозвратно. Теперь оно кэшируется в RecvCache и отдаётся
    /// повторным вызовом с достаточным буфером.
    #[test]
    fn receive_with_too_small_buffer_does_not_lose_message() {
        let name = unique_name("RECVCACHE");
        const MSG: &[u8] = b"hello world";

        let server_thread = {
            let name = name.clone();
            thread::spawn(move || {
                let name_c = CString::new(name).unwrap();
                let config = shm_endpoint_config_t {
                    name: name_c.as_ptr(),
                };
                let server = shm_server_start(&config, std::ptr::null());
                assert!(!server.is_null());
                assert_eq!(
                    shm_server_wait_for_client(server, 5000),
                    shm_error_t::SHM_SUCCESS
                );
                assert_eq!(
                    shm_server_send(server, MSG.as_ptr() as *const c_void, MSG.len() as u32),
                    shm_error_t::SHM_SUCCESS
                );

                // ждём "done" от клиента, прежде чем останавливать сервер
                let mut recv_buf = [0u8; 16];
                let start = std::time::Instant::now();
                loop {
                    assert!(start.elapsed() < StdDuration::from_secs(5), "timeout waiting for done");
                    if shm_server_poll(server, 200) == shm_error_t::SHM_SUCCESS {
                        let mut size = recv_buf.len() as u32;
                        if shm_server_receive(
                            server,
                            recv_buf.as_mut_ptr() as *mut c_void,
                            &mut size,
                        ) == shm_error_t::SHM_SUCCESS
                        {
                            break;
                        }
                    }
                }
                shm_server_stop(server);
            })
        };

        thread::sleep(StdDuration::from_millis(50));

        let name_c = CString::new(name).unwrap();
        let config = shm_endpoint_config_t {
            name: name_c.as_ptr(),
        };
        let client = shm_client_connect(&config, std::ptr::null(), 5000);
        assert!(!client.is_null());
        assert_eq!(shm_client_poll(client, 5000), shm_error_t::SHM_SUCCESS);

        // буфер слишком мал, чтобы вместить сообщение целиком
        let mut small_buf = [0u8; 4];
        let mut small_size = small_buf.len() as u32;
        let result = shm_client_receive(
            client,
            small_buf.as_mut_ptr() as *mut c_void,
            &mut small_size,
        );
        assert_eq!(result, shm_error_t::SHM_ERROR_MEMORY);

        // повторный вызов с достаточным буфером должен вернуть ТО ЖЕ сообщение
        // (не следующее и не пустоту) — доказывает, что оно не было потеряно
        let mut big_buf = [0u8; 64];
        let mut big_size = big_buf.len() as u32;
        let result = shm_client_receive(client, big_buf.as_mut_ptr() as *mut c_void, &mut big_size);
        assert_eq!(result, shm_error_t::SHM_SUCCESS);
        assert_eq!(big_size as usize, MSG.len());
        assert_eq!(&big_buf[..MSG.len()], MSG);

        assert_eq!(
            shm_client_send(client, b"done".as_ptr() as *const c_void, 4),
            shm_error_t::SHM_SUCCESS
        );
        shm_client_disconnect(client);
        server_thread.join().unwrap();
    }

    /// Конкурентный send с одного потока и receive с другого на одном handle
    /// не должен приводить к панике/deadlock'у — оба пути теперь берут только
    /// `&ServerState`, без конфликта `&` vs `&mut` через unsafe-границу FFI.
    #[test]
    fn concurrent_send_and_receive_on_same_handle_do_not_conflict() {
        let name = unique_name("CONCURRENT");
        const ITERATIONS: usize = 200;

        let server_thread = {
            let name = name.clone();
            thread::spawn(move || {
                let name_c = CString::new(name).unwrap();
                let config = shm_endpoint_config_t {
                    name: name_c.as_ptr(),
                };
                let server = shm_server_start(&config, std::ptr::null());
                assert!(!server.is_null());
                assert_eq!(
                    shm_server_wait_for_client(server, 5000),
                    shm_error_t::SHM_SUCCESS
                );
                server as usize
            })
        };

        thread::sleep(StdDuration::from_millis(50));

        let name_c = CString::new(name).unwrap();
        let config = shm_endpoint_config_t {
            name: name_c.as_ptr(),
        };
        let client = shm_client_connect(&config, std::ptr::null(), 5000);
        assert!(!client.is_null());

        let server = server_thread.join().unwrap() as *mut ServerHandle;
        let server_addr = server as usize;

        // поток A: сервер шлёт сообщения клиенту
        let sender = thread::spawn(move || {
            let server = server_addr as *mut ServerHandle;
            for i in 0..ITERATIONS {
                let msg = (i as u32).to_le_bytes();
                loop {
                    match shm_server_send(server, msg.as_ptr() as *const c_void, 4) {
                        shm_error_t::SHM_SUCCESS => break,
                        shm_error_t::SHM_ERROR_FULL => thread::sleep(StdDuration::from_micros(50)),
                        other => panic!("unexpected send error: {other:?}"),
                    }
                }
            }
        });

        // поток B: сервер параллельно опрашивает клиентские сообщения (их нет,
        // важен сам факт конкурентного вызова receive и send на одном handle)
        let receiver = thread::spawn(move || {
            let server = server_addr as *mut ServerHandle;
            let mut buf = [0u8; 16];
            for _ in 0..ITERATIONS {
                let mut size = buf.len() as u32;
                let _ = shm_server_receive(server, buf.as_mut_ptr() as *mut c_void, &mut size);
                thread::sleep(StdDuration::from_micros(50));
            }
        });

        sender.join().unwrap();
        receiver.join().unwrap();

        let mut received = 0usize;
        let start = std::time::Instant::now();
        while received < ITERATIONS {
            assert!(start.elapsed() < StdDuration::from_secs(5), "timeout draining");
            if shm_client_poll(client, 200) == shm_error_t::SHM_SUCCESS {
                let mut buf = [0u8; 16];
                let mut size = buf.len() as u32;
                if shm_client_receive(client, buf.as_mut_ptr() as *mut c_void, &mut size)
                    == shm_error_t::SHM_SUCCESS
                {
                    received += 1;
                }
            }
        }

        shm_client_disconnect(client);
        shm_server_stop(server);
    }
}

#[repr(C)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum shm_direction_t {
    SHM_DIR_SERVER_TO_CLIENT = 0,
    SHM_DIR_CLIENT_TO_SERVER = 1,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct shm_auto_options_t {
    pub poll_timeout_ms: u32,
    pub reconnect_delay_ms: u32,
    pub connect_timeout_ms: u32,
    pub max_send_queue: u32,
    pub recv_batch: u32,
}

impl Default for shm_auto_options_t {
    fn default() -> Self {
        Self {
            poll_timeout_ms: 50,
            reconnect_delay_ms: 250,
            connect_timeout_ms: 2000,
            max_send_queue: 256,
            recv_batch: 32,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct shm_auto_stats_t {
    pub sent_messages: u64,
    pub send_overflows: u64,
    pub received_messages: u64,
    pub receive_overflows: u64,
}

fn to_rust_str(ptr: *const c_char) -> Result<String> {
    if ptr.is_null() {
        return Err(ShmError::NotReady);
    }
    let cstr = unsafe { CStr::from_ptr(ptr) };
    Ok(cstr.to_string_lossy().into_owned())
}

/// Буфер приёма с кэшем недоставленного сообщения.
///
/// Если сообщение вычитано из ring buffer, но буфер C-вызывающего оказался
/// мал (`len > capacity`), сообщение уже необратимо удалено из ring —
/// возвращать ошибку без сохранения означало бы потерю сообщения. Поэтому
/// оно остаётся в `buffer`, а `pending_len` помечает его как недоставленное:
/// следующий вызов receive сначала отдаёт его из кэша и только затем читает
/// новое сообщение из ring buffer.
struct RecvCache {
    buffer: Vec<u8>,
    pending_len: Option<usize>,
}

impl RecvCache {
    fn new() -> Self {
        Self {
            buffer: Vec::with_capacity(MAX_MESSAGE_SIZE),
            pending_len: None,
        }
    }
}

struct ServerState {
    inner: SharedServer,
    callbacks: Option<shm_callbacks_t>,
    recv_cache: Mutex<RecvCache>,
}

struct ClientState {
    inner: SharedClient,
    recv_cache: Mutex<RecvCache>,
}

pub type ServerHandle = c_void;
pub type ClientHandle = c_void;
pub type AutoServerHandle = c_void;
pub type AutoClientHandle = c_void;

struct AutoServerState {
    inner: AutoServer,
    _callbacks: Option<shm_callbacks_t>,
    _handler: Arc<FfiHandler>,
}

struct AutoClientState {
    inner: AutoClient,
    _callbacks: Option<shm_callbacks_t>,
    _handler: Arc<FfiHandler>,
}

#[derive(Clone)]
struct FfiHandler {
    callbacks: shm_callbacks_t,
}

unsafe impl Send for FfiHandler {}
unsafe impl Sync for FfiHandler {}

fn server_state_from(handle: *mut ServerHandle) -> *mut ServerState {
    handle as *mut ServerState
}

fn client_state_from(handle: *mut ClientHandle) -> *mut ClientState {
    handle as *mut ClientState
}

fn client_state_from_const(handle: *const ClientHandle) -> *const ClientState {
    handle as *const ClientState
}

fn auto_server_state_from(handle: *mut AutoServerHandle) -> *mut AutoServerState {
    handle as *mut AutoServerState
}

fn auto_client_state_from(handle: *mut AutoClientHandle) -> *mut AutoClientState {
    handle as *mut AutoClientState
}

fn auto_server_state_from_const(handle: *const AutoServerHandle) -> *const AutoServerState {
    handle as *const AutoServerState
}

fn auto_client_state_from_const(handle: *const AutoClientHandle) -> *const AutoClientState {
    handle as *const AutoClientState
}

impl Default for shm_callbacks_t {
    fn default() -> Self {
        Self {
            on_connect: None,
            on_disconnect: None,
            on_data_available: None,
            on_space_available: None,
            on_error: None,
            user_data: null_mut(),
            on_message: None,
            on_overflow: None,
        }
    }
}

impl AutoHandler for FfiHandler {
    fn on_connect(&self) {
        if let Some(cb) = self.callbacks.on_connect {
            cb(self.callbacks.user_data);
        }
    }

    fn on_disconnect(&self) {
        if let Some(cb) = self.callbacks.on_disconnect {
            cb(self.callbacks.user_data);
        }
    }

    fn on_message(&self, direction: ChannelKind, payload: &[u8]) {
        if let Some(cb) = self.callbacks.on_message {
            cb(
                direction.into(),
                payload.as_ptr() as *const c_void,
                payload.len() as u32,
                self.callbacks.user_data,
            );
        } else if let Some(cb) = self.callbacks.on_data_available {
            cb(self.callbacks.user_data);
        }
    }

    fn on_overflow(&self, direction: ChannelKind, dropped: u32) {
        if let Some(cb) = self.callbacks.on_overflow {
            cb(direction.into(), dropped, self.callbacks.user_data);
        }
    }

    fn on_space_available(&self, _direction: ChannelKind) {
        if let Some(cb) = self.callbacks.on_space_available {
            cb(self.callbacks.user_data);
        }
    }

    fn on_error(&self, err: ShmError) {
        if let Some(cb) = self.callbacks.on_error {
            cb(err.into(), self.callbacks.user_data);
        }
    }
}

impl From<ChannelKind> for shm_direction_t {
    fn from(value: ChannelKind) -> Self {
        match value {
            ChannelKind::ServerToClient => shm_direction_t::SHM_DIR_SERVER_TO_CLIENT,
            ChannelKind::ClientToServer => shm_direction_t::SHM_DIR_CLIENT_TO_SERVER,
        }
    }
}

fn ffi_auto_options(ptr: *const shm_auto_options_t) -> AutoOptions {
    if ptr.is_null() {
        return AutoOptions::default();
    }
    let opts = unsafe { *ptr };
    AutoOptions {
        poll_timeout: Duration::from_millis(opts.poll_timeout_ms as u64),
        reconnect_delay: Duration::from_millis(opts.reconnect_delay_ms as u64),
        connect_timeout: Duration::from_millis(opts.connect_timeout_ms as u64),
        max_send_queue: opts.max_send_queue as usize,
        recv_batch: opts.recv_batch as usize,
    }
}

fn write_stats(dst: *mut shm_auto_stats_t, stats: AutoStatsSnapshot) -> bool {
    if dst.is_null() {
        return false;
    }
    unsafe {
        *dst = shm_auto_stats_t {
            sent_messages: stats.sent_messages,
            send_overflows: stats.send_overflows,
            received_messages: stats.received_messages,
            receive_overflows: stats.receive_overflows,
        };
    }
    true
}

#[unsafe(no_mangle)]
pub extern "C" fn shm_auto_options_default() -> shm_auto_options_t {
    shm_auto_options_t::default()
}

#[unsafe(no_mangle)]
pub extern "C" fn shm_server_start_auto(
    config: *const shm_endpoint_config_t,
    callbacks: *const shm_callbacks_t,
    options: *const shm_auto_options_t,
) -> *mut AutoServerHandle {
    if config.is_null() {
        return null_mut();
    }
    let cfg = unsafe { &*config };
    let name = match to_rust_str(cfg.name) {
        Ok(name) => name,
        Err(_) => return null_mut(),
    };
    let callbacks_val = if callbacks.is_null() {
        shm_callbacks_t::default()
    } else {
        unsafe { *callbacks }
    };
    let handler = Arc::new(FfiHandler {
        callbacks: callbacks_val,
    });
    let opts = ffi_auto_options(options);
    match AutoServer::start(&name, handler.clone(), opts) {
        Ok(inner) => Box::into_raw(Box::new(AutoServerState {
            inner,
            _callbacks: Some(callbacks_val),
            _handler: handler,
        })) as *mut AutoServerHandle,
        Err(err) => {
            if let Some(cb) = callbacks_val.on_error {
                cb(err.into(), callbacks_val.user_data);
            }
            null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn shm_server_send_auto(
    handle: *mut AutoServerHandle,
    data: *const c_void,
    size: u32,
) -> shm_error_t {
    if handle.is_null() || data.is_null() || size == 0 || size as usize > MAX_MESSAGE_SIZE {
        return shm_error_t::SHM_ERROR_INVALID_PARAM;
    }
    let state = unsafe { &*auto_server_state_from(handle) };
    let slice = unsafe { std::slice::from_raw_parts(data as *const u8, size as usize) };
    match state.inner.send(slice) {
        Ok(_) => shm_error_t::SHM_SUCCESS,
        Err(err) => err.into(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn shm_server_stats_auto(
    handle: *const AutoServerHandle,
    out: *mut shm_auto_stats_t,
) -> bool {
    if handle.is_null() {
        return false;
    }
    let state = unsafe { &*auto_server_state_from_const(handle) };
    write_stats(out, state.inner.stats())
}

#[unsafe(no_mangle)]
pub extern "C" fn shm_server_stop_auto(handle: *mut AutoServerHandle) {
    if handle.is_null() {
        return;
    }
    unsafe {
        drop(Box::from_raw(auto_server_state_from(handle)));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn shm_client_connect_auto(
    config: *const shm_endpoint_config_t,
    callbacks: *const shm_callbacks_t,
    options: *const shm_auto_options_t,
) -> *mut AutoClientHandle {
    if config.is_null() {
        return null_mut();
    }
    let cfg = unsafe { &*config };
    let name = match to_rust_str(cfg.name) {
        Ok(name) => name,
        Err(_) => return null_mut(),
    };
    let callbacks_val = if callbacks.is_null() {
        shm_callbacks_t::default()
    } else {
        unsafe { *callbacks }
    };
    let handler = Arc::new(FfiHandler {
        callbacks: callbacks_val,
    });
    let opts = ffi_auto_options(options);
    match AutoClient::connect(&name, handler.clone(), opts) {
        Ok(inner) => Box::into_raw(Box::new(AutoClientState {
            inner,
            _callbacks: Some(callbacks_val),
            _handler: handler,
        })) as *mut AutoClientHandle,
        Err(err) => {
            if let Some(cb) = callbacks_val.on_error {
                cb(err.into(), callbacks_val.user_data);
            }
            null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn shm_client_send_auto(
    handle: *mut AutoClientHandle,
    data: *const c_void,
    size: u32,
) -> shm_error_t {
    if handle.is_null() || data.is_null() || size == 0 || size as usize > MAX_MESSAGE_SIZE {
        return shm_error_t::SHM_ERROR_INVALID_PARAM;
    }
    let state = unsafe { &*auto_client_state_from(handle) };
    let slice = unsafe { std::slice::from_raw_parts(data as *const u8, size as usize) };
    match state.inner.send(slice) {
        Ok(_) => shm_error_t::SHM_SUCCESS,
        Err(err) => err.into(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn shm_client_stats_auto(
    handle: *const AutoClientHandle,
    out: *mut shm_auto_stats_t,
) -> bool {
    if handle.is_null() {
        return false;
    }
    let state = unsafe { &*auto_client_state_from_const(handle) };
    write_stats(out, state.inner.stats())
}

#[unsafe(no_mangle)]
pub extern "C" fn shm_client_disconnect_auto(handle: *mut AutoClientHandle) {
    if handle.is_null() {
        return;
    }
    unsafe {
        drop(Box::from_raw(auto_client_state_from(handle)));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn shm_server_start(
    config: *const shm_endpoint_config_t,
    callbacks: *const shm_callbacks_t,
) -> *mut ServerHandle {
    if config.is_null() {
        return null_mut();
    }
    let cfg = unsafe { &*config };
    let name = match to_rust_str(cfg.name) {
        Ok(name) => name,
        Err(_) => return null_mut(),
    };
    let callbacks = if callbacks.is_null() {
        None
    } else {
        Some(unsafe { *callbacks })
    };

    match SharedServer::start(&name) {
        Ok(server) => Box::into_raw(Box::new(ServerState {
            inner: server,
            callbacks,
            recv_cache: Mutex::new(RecvCache::new()),
        })) as *mut ServerHandle,
        Err(err) => {
            let code: shm_error_t = err.into();
            if let Some(cb) = callbacks {
                if let Some(on_error) = cb.on_error {
                    on_error(code, cb.user_data);
                }
            }
            null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn shm_server_wait_for_client(
    handle: *mut ServerHandle,
    timeout_ms: u32,
) -> shm_error_t {
    if handle.is_null() {
        return shm_error_t::SHM_ERROR_INVALID_PARAM;
    }
    let state = unsafe { &mut *server_state_from(handle) };
    let timeout = if timeout_ms == u32::MAX {
        None
    } else {
        Some(Duration::from_millis(timeout_ms as u64))
    };
    match state.inner.wait_for_client(timeout) {
        Ok(_) => {
            if let Some(cb) = state.callbacks.as_ref() {
                if let Some(on_connect) = cb.on_connect {
                    on_connect(cb.user_data);
                }
            }
            shm_error_t::SHM_SUCCESS
        }
        Err(err) => {
            let code: shm_error_t = err.clone().into();
            if let Some(cb) = state.callbacks.as_ref() {
                if let Some(on_error) = cb.on_error {
                    on_error(code, cb.user_data);
                }
            }
            code
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn shm_server_stop(handle: *mut ServerHandle) {
    if handle.is_null() {
        return;
    }
    unsafe {
        let boxed = Box::from_raw(server_state_from(handle));
        if let Some(cb) = boxed.callbacks {
            if let Some(on_disconnect) = cb.on_disconnect {
                on_disconnect(cb.user_data);
            }
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn shm_server_send(
    handle: *mut ServerHandle,
    data: *const c_void,
    size: u32,
) -> shm_error_t {
    if handle.is_null() || data.is_null() || size == 0 || size as usize > MAX_MESSAGE_SIZE {
        return shm_error_t::SHM_ERROR_INVALID_PARAM;
    }
    let state = unsafe { &*server_state_from(handle) };
    let slice = unsafe { std::slice::from_raw_parts(data as *const u8, size as usize) };
    match state.inner.send_to_client(slice) {
        Ok(_) => shm_error_t::SHM_SUCCESS,
        Err(err) => err.into(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn shm_server_receive(
    handle: *mut ServerHandle,
    buffer: *mut c_void,
    size: *mut u32,
) -> shm_error_t {
    if handle.is_null() || buffer.is_null() || size.is_null() {
        return shm_error_t::SHM_ERROR_INVALID_PARAM;
    }
    let state = unsafe { &*server_state_from(handle) };
    let capacity = unsafe { *size } as usize;
    if capacity == 0 {
        return shm_error_t::SHM_ERROR_INVALID_PARAM;
    }

    let mut cache = state.recv_cache.lock().unwrap();
    let len = match cache.pending_len.take() {
        Some(pending_len) => pending_len,
        None => match state.inner.receive_from_client(&mut cache.buffer) {
            Ok(len) => len,
            Err(err) => return err.into(),
        },
    };
    if len > capacity {
        cache.pending_len = Some(len);
        return shm_error_t::SHM_ERROR_MEMORY;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(cache.buffer.as_ptr(), buffer as *mut u8, len);
        *size = len as u32;
    }
    shm_error_t::SHM_SUCCESS
}

#[unsafe(no_mangle)]
pub extern "C" fn shm_server_poll(handle: *mut ServerHandle, timeout_ms: u32) -> shm_error_t {
    if handle.is_null() {
        return shm_error_t::SHM_ERROR_INVALID_PARAM;
    }
    let state = unsafe { &*server_state_from(handle) };
    let timeout = if timeout_ms == u32::MAX {
        None
    } else {
        Some(Duration::from_millis(timeout_ms as u64))
    };
    match state.inner.poll_client(timeout) {
        Ok(true) => shm_error_t::SHM_SUCCESS,
        Ok(false) => shm_error_t::SHM_ERROR_TIMEOUT,
        Err(err) => err.into(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn shm_client_connect(
    config: *const shm_endpoint_config_t,
    callbacks: *const shm_callbacks_t,
    timeout_ms: u32,
) -> *mut ClientHandle {
    if config.is_null() {
        return null_mut();
    }
    let cfg = unsafe { &*config };
    let name = match to_rust_str(cfg.name) {
        Ok(name) => name,
        Err(_) => return null_mut(),
    };
    let timeout = Duration::from_millis(timeout_ms as u64);
    match SharedClient::connect(&name, timeout) {
        Ok(client) => {
            if !callbacks.is_null() {
                let cb = unsafe { *callbacks };
                if let Some(on_connect) = cb.on_connect {
                    on_connect(cb.user_data);
                }
            }
            Box::into_raw(Box::new(ClientState {
                inner: client,
                recv_cache: Mutex::new(RecvCache::new()),
            })) as *mut ClientHandle
        }
        Err(err) => {
            if !callbacks.is_null() {
                let cb = unsafe { *callbacks };
                let code: shm_error_t = err.clone().into();
                if let Some(on_error) = cb.on_error {
                    on_error(code, cb.user_data);
                }
            }
            null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn shm_client_disconnect(handle: *mut ClientHandle) {
    if handle.is_null() {
        return;
    }
    unsafe {
        drop(Box::from_raw(client_state_from(handle)));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn shm_client_is_connected(handle: *const ClientHandle) -> bool {
    if handle.is_null() {
        return false;
    }
    let state = unsafe { &*client_state_from_const(handle) };
    state.inner.is_connected()
}

#[unsafe(no_mangle)]
pub extern "C" fn shm_client_send(
    handle: *mut ClientHandle,
    data: *const c_void,
    size: u32,
) -> shm_error_t {
    if handle.is_null() || data.is_null() || size == 0 || size as usize > MAX_MESSAGE_SIZE {
        return shm_error_t::SHM_ERROR_INVALID_PARAM;
    }
    let state = unsafe { &*client_state_from(handle) };
    let slice = unsafe { std::slice::from_raw_parts(data as *const u8, size as usize) };
    match state.inner.send_to_server(slice) {
        Ok(_) => shm_error_t::SHM_SUCCESS,
        Err(err) => err.into(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn shm_client_receive(
    handle: *mut ClientHandle,
    buffer: *mut c_void,
    size: *mut u32,
) -> shm_error_t {
    if handle.is_null() || buffer.is_null() || size.is_null() {
        return shm_error_t::SHM_ERROR_INVALID_PARAM;
    }
    let state = unsafe { &*client_state_from(handle as *mut ClientHandle) };
    let capacity = unsafe { *size } as usize;
    if capacity == 0 {
        return shm_error_t::SHM_ERROR_INVALID_PARAM;
    }

    let mut cache = state.recv_cache.lock().unwrap();
    let len = match cache.pending_len.take() {
        Some(pending_len) => pending_len,
        None => match state.inner.receive_from_server(&mut cache.buffer) {
            Ok(len) => len,
            Err(err) => return err.into(),
        },
    };
    if len > capacity {
        cache.pending_len = Some(len);
        return shm_error_t::SHM_ERROR_MEMORY;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(cache.buffer.as_ptr(), buffer as *mut u8, len);
        *size = len as u32;
    }
    shm_error_t::SHM_SUCCESS
}

#[unsafe(no_mangle)]
pub extern "C" fn shm_client_poll(handle: *mut ClientHandle, timeout_ms: u32) -> shm_error_t {
    if handle.is_null() {
        return shm_error_t::SHM_ERROR_INVALID_PARAM;
    }
    let state = unsafe { &*client_state_from(handle) };
    let timeout = if timeout_ms == u32::MAX {
        None
    } else {
        Some(Duration::from_millis(timeout_ms as u64))
    };
    match state.inner.poll_server(timeout) {
        Ok(true) => shm_error_t::SHM_SUCCESS,
        Ok(false) => shm_error_t::SHM_ERROR_TIMEOUT,
        Err(err) => err.into(),
    }
}

/// Получить event handles для передачи в kernel driver
///
/// Возвращает структуру с raw handles (isize) для event-driven IPC.
/// Для anonymous серверов (без событий) возвращает handles с нулевыми значениями.
///
/// # Parameters
/// - `handle`: Handle сервера
/// - `out`: Указатель на структуру для записи handles (может быть NULL)
///
/// # Returns
/// true если handles успешно получены, false при ошибке
#[unsafe(no_mangle)]
pub extern "C" fn shm_server_get_event_handles(
    handle: *mut ServerHandle,
    out: *mut crate::events::EventHandles,
) -> bool {
    if handle.is_null() || out.is_null() {
        return false;
    }
    let state = unsafe { &*server_state_from(handle) };
    if let Some(handles) = state.inner.get_event_handles() {
        unsafe {
            *out = handles;
        }
        true
    } else {
        // Anonymous сервер - возвращаем нулевые handles
        unsafe {
            *out = crate::events::EventHandles {
                s2c_data: 0,
                c2s_data: 0,
            };
        }
        false
    }
}
