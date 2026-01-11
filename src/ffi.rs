//! FFI-интерфейс совместимый с существующей C-библиотекой.

use std::ffi::CStr;
use std::os::raw::{c_char, c_void};
use std::ptr::null_mut;
use std::sync::Arc;
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
    pub buffer_bytes: u32,
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
            ShmError::NotConnected => shm_error_t::SHM_ERROR_NOT_READY,
            ShmError::AlreadyConnected => shm_error_t::SHM_ERROR_EXISTS,
            ShmError::HandshakeFailed | ShmError::Corrupted => shm_error_t::SHM_ERROR_PROTOCOL,
            ShmError::WindowsError { .. } => shm_error_t::SHM_ERROR_ACCESS,
            ShmError::NoFreeSlot => shm_error_t::SHM_ERROR_NO_SLOT,
        }
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
    pub wait_timeout_ms: u32,
    pub reconnect_delay_ms: u32,
    pub connect_timeout_ms: u32,
    pub max_send_queue: u32,
    pub recv_batch: u32,
}

impl Default for shm_auto_options_t {
    fn default() -> Self {
        Self {
            wait_timeout_ms: 50,
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

struct ServerState {
    inner: SharedServer,
    callbacks: Option<shm_callbacks_t>,
}

struct ClientState {
    inner: SharedClient,
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
        wait_timeout: Duration::from_millis(opts.wait_timeout_ms as u64),
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

    let mut storage = Vec::with_capacity(MAX_MESSAGE_SIZE);
    match state.inner.receive_from_client(&mut storage) {
        Ok(len) => {
            if len > capacity {
                return shm_error_t::SHM_ERROR_MEMORY;
            }
            unsafe {
                std::ptr::copy_nonoverlapping(storage.as_ptr(), buffer as *mut u8, len);
                *size = len as u32;
            }
            shm_error_t::SHM_SUCCESS
        }
        Err(err) => err.into(),
    }
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
            Box::into_raw(Box::new(ClientState { inner: client })) as *mut ClientHandle
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
    let state = unsafe { &*client_state_from(handle) };
    let capacity = unsafe { *size } as usize;
    if capacity == 0 {
        return shm_error_t::SHM_ERROR_INVALID_PARAM;
    }

    let mut storage = Vec::with_capacity(MAX_MESSAGE_SIZE);
    match state.inner.receive_from_server(&mut storage) {
        Ok(len) => {
            if len > capacity {
                return shm_error_t::SHM_ERROR_MEMORY;
            }
            unsafe {
                std::ptr::copy_nonoverlapping(storage.as_ptr(), buffer as *mut u8, len);
                *size = len as u32;
            }
            shm_error_t::SHM_SUCCESS
        }
        Err(err) => err.into(),
    }
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
