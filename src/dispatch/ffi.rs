//! C FFI для dispatch-сервера и клиента.
//!
//! Следует тем же паттернам, что и `crate::ffi` и `crate::multi::ffi`.

use std::ffi::CStr;
use std::os::raw::{c_char, c_void};
use std::ptr::null_mut;
use std::sync::Arc;
use std::time::Duration;

use crate::constants::MAX_MESSAGE_SIZE;
use crate::error::ShmError;
use crate::ffi::shm_error_t;

use super::{
    ClientRegistration, DispatchClient, DispatchClientHandler, DispatchClientOptions,
    DispatchHandler, DispatchOptions, DispatchServer,
};

// ─── Типы FFI ────────────────────────────────────────────────────────────────

/// Данные регистрации, передаваемые от C-клиента.
#[repr(C)]
pub struct shm_dispatch_registration_t {
    pub pid: u32,
    pub revision: u16,
    pub name: *const c_char,
}

/// Callbacks на стороне сервера.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct shm_dispatch_callbacks_t {
    pub on_client_connect: Option<
        extern "C" fn(
            client_id: u32,
            pid: u32,
            revision: u16,
            name: *const c_char,
            user_data: *mut c_void,
        ),
    >,
    pub on_client_disconnect: Option<extern "C" fn(client_id: u32, user_data: *mut c_void)>,
    pub on_message: Option<
        extern "C" fn(client_id: u32, data: *const c_void, size: u32, user_data: *mut c_void),
    >,
    pub on_error: Option<extern "C" fn(client_id: i32, error: shm_error_t, user_data: *mut c_void)>,
    pub user_data: *mut c_void,
}

/// Callbacks на стороне клиента.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct shm_dispatch_client_callbacks_t {
    pub on_connect:
        Option<extern "C" fn(client_id: u32, channel_name: *const c_char, user_data: *mut c_void)>,
    pub on_disconnect: Option<extern "C" fn(user_data: *mut c_void)>,
    pub on_message: Option<extern "C" fn(data: *const c_void, size: u32, user_data: *mut c_void)>,
    pub on_error: Option<extern "C" fn(error: shm_error_t, user_data: *mut c_void)>,
    pub user_data: *mut c_void,
}

/// Настройки сервера.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct shm_dispatch_options_t {
    pub lobby_timeout_ms: u32,
    pub channel_connect_timeout_ms: u32,
    pub poll_timeout_ms: u32,
    pub recv_batch: u32,
}

impl Default for shm_dispatch_options_t {
    fn default() -> Self {
        Self {
            lobby_timeout_ms: 5000,
            channel_connect_timeout_ms: 30000,
            poll_timeout_ms: 50,
            recv_batch: 32,
        }
    }
}

/// Настройки клиента.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct shm_dispatch_client_options_t {
    pub lobby_timeout_ms: u32,
    pub response_timeout_ms: u32,
    pub channel_timeout_ms: u32,
    pub poll_timeout_ms: u32,
    pub recv_batch: u32,
    pub max_send_queue: u32,
}

impl Default for shm_dispatch_client_options_t {
    fn default() -> Self {
        Self {
            lobby_timeout_ms: 5000,
            response_timeout_ms: 5000,
            channel_timeout_ms: 10000,
            poll_timeout_ms: 50,
            recv_batch: 32,
            max_send_queue: 256,
        }
    }
}

// ─── Внутреннее состояние ─────────────────────────────────────────────────────

pub type DispatchServerHandle = c_void;
pub type DispatchClientHandle = c_void;

struct DispatchServerState {
    inner: Arc<DispatchServer>,
}

struct DispatchClientState {
    inner: DispatchClient,
}

unsafe impl Send for FfiDispatchHandler {}
unsafe impl Sync for FfiDispatchHandler {}

struct FfiDispatchHandler {
    callbacks: shm_dispatch_callbacks_t,
}

unsafe impl Send for FfiDispatchClientHandler {}
unsafe impl Sync for FfiDispatchClientHandler {}

struct FfiDispatchClientHandler {
    callbacks: shm_dispatch_client_callbacks_t,
}

// ─── Реализации handler'ов ────────────────────────────────────────────────────

impl DispatchHandler for FfiDispatchHandler {
    fn on_client_connect(&self, client_id: u32, info: &ClientRegistration) {
        if let Some(cb) = self.callbacks.on_client_connect {
            let name_cstr = to_c_string_lossy(info.name.as_str());
            cb(
                client_id,
                info.pid,
                info.revision,
                name_cstr.as_ptr(),
                self.callbacks.user_data,
            );
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
            let id = client_id.map(|i| i as i32).unwrap_or(-1);
            cb(id, err.into(), self.callbacks.user_data);
        }
    }
}

impl DispatchClientHandler for FfiDispatchClientHandler {
    fn on_connect(&self, client_id: u32, channel_name: &str) {
        if let Some(cb) = self.callbacks.on_connect {
            let name_cstr = to_c_string_lossy(channel_name);
            cb(client_id, name_cstr.as_ptr(), self.callbacks.user_data);
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

// ─── Вспомогательные функции ──────────────────────────────────────────────────

/// # Safety
/// `ptr` обязан быть валидной null-terminated C-строкой либо null.
unsafe fn to_rust_str(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let cstr = unsafe { CStr::from_ptr(ptr) };
    Some(cstr.to_string_lossy().into_owned())
}

/// Конвертирует Rust-строку в `CString` для передачи в callback, устойчиво
/// к embedded NUL-байтам -- усекает по первому NUL вместо потери всей строки.
///
/// `CString::new` проваливается целиком при embedded NUL; раньше на этот
/// случай делался `.unwrap_or_default()`, что тихо отдавало C-колбэку ПУСТУЮ
/// строку вместо реального (потенциально пришедшего от клиента по проводу)
/// имени. C-строка всё равно не может представить байты после NUL, так что
/// усечение там -- честный best-effort, а не дополнительная потеря данных.
fn to_c_string_lossy(s: &str) -> std::ffi::CString {
    match std::ffi::CString::new(s) {
        Ok(c) => c,
        Err(e) => {
            let nul_pos = e.nul_position();
            let bytes = e.into_vec();
            // bytes[..nul_pos] по определению NulError не содержит NUL,
            // поэтому CString::new здесь не может провалиться повторно.
            std::ffi::CString::new(&bytes[..nul_pos]).unwrap_or_default()
        }
    }
}

/// # Safety
/// `ptr` обязан быть валидным указателем на `shm_dispatch_options_t` либо null.
unsafe fn to_dispatch_options(ptr: *const shm_dispatch_options_t) -> DispatchOptions {
    if ptr.is_null() {
        return DispatchOptions::default();
    }
    let opts = unsafe { *ptr };
    DispatchOptions {
        lobby_timeout: Duration::from_millis(opts.lobby_timeout_ms as u64),
        channel_connect_timeout: Duration::from_millis(opts.channel_connect_timeout_ms as u64),
        poll_timeout: Duration::from_millis(opts.poll_timeout_ms as u64),
        recv_batch: opts.recv_batch as usize,
    }
}

/// # Safety
/// `ptr` обязан быть валидным указателем на `shm_dispatch_client_options_t` либо null.
unsafe fn to_dispatch_client_options(
    ptr: *const shm_dispatch_client_options_t,
) -> DispatchClientOptions {
    if ptr.is_null() {
        return DispatchClientOptions::default();
    }
    let opts = unsafe { *ptr };
    DispatchClientOptions {
        lobby_timeout: Duration::from_millis(opts.lobby_timeout_ms as u64),
        response_timeout: Duration::from_millis(opts.response_timeout_ms as u64),
        channel_timeout: Duration::from_millis(opts.channel_timeout_ms as u64),
        poll_timeout: Duration::from_millis(opts.poll_timeout_ms as u64),
        recv_batch: opts.recv_batch as usize,
        max_send_queue: opts.max_send_queue as usize,
    }
}

// ─── FFI сервера ─────────────────────────────────────────────────────────────

/// # Safety
/// Все указатели обязаны быть валидны либо null там, где это задокументировано.
/// `name` обязан быть валидной C-строкой.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn shm_dispatch_server_start(
    name: *const c_char,
    callbacks: *const shm_dispatch_callbacks_t,
    options: *const shm_dispatch_options_t,
) -> *mut DispatchServerHandle {
    let name_str = match unsafe { to_rust_str(name) } {
        Some(n) => n,
        None => return null_mut(),
    };

    if callbacks.is_null() {
        return null_mut();
    }
    let callbacks_val = unsafe { *callbacks };

    let handler = Arc::new(FfiDispatchHandler {
        callbacks: callbacks_val,
    });

    let opts = unsafe { to_dispatch_options(options) };

    match DispatchServer::start(&name_str, handler, opts) {
        Ok(inner) => {
            Box::into_raw(Box::new(DispatchServerState { inner })) as *mut DispatchServerHandle
        }
        Err(err) => {
            if let Some(cb) = callbacks_val.on_error {
                cb(-1, err.into(), callbacks_val.user_data);
            }
            null_mut()
        }
    }
}

/// # Safety
/// `handle` обязан быть валидным DispatchServerHandle. `data` обязан указывать на `size` байт.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn shm_dispatch_server_send_to(
    handle: *mut DispatchServerHandle,
    client_id: u32,
    data: *const c_void,
    size: u32,
) -> shm_error_t {
    if handle.is_null() || data.is_null() || size == 0 || size as usize > MAX_MESSAGE_SIZE {
        return shm_error_t::SHM_ERROR_INVALID_PARAM;
    }
    let state = unsafe { &*(handle as *const DispatchServerState) };
    let slice = unsafe { std::slice::from_raw_parts(data as *const u8, size as usize) };
    match state.inner.send_to(client_id, slice) {
        Ok(()) => shm_error_t::SHM_SUCCESS,
        Err(err) => err.into(),
    }
}

/// # Safety
/// `handle` обязан быть валидным. `data` обязан указывать на `size` байт. `sent_count` может быть null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn shm_dispatch_server_broadcast(
    handle: *mut DispatchServerHandle,
    data: *const c_void,
    size: u32,
    sent_count: *mut u32,
) -> shm_error_t {
    if handle.is_null() || data.is_null() || size == 0 || size as usize > MAX_MESSAGE_SIZE {
        return shm_error_t::SHM_ERROR_INVALID_PARAM;
    }
    let state = unsafe { &*(handle as *const DispatchServerState) };
    let slice = unsafe { std::slice::from_raw_parts(data as *const u8, size as usize) };
    match state.inner.broadcast(slice) {
        Ok(count) => {
            if !sent_count.is_null() {
                unsafe { *sent_count = count };
            }
            shm_error_t::SHM_SUCCESS
        }
        Err(err) => err.into(),
    }
}

/// # Safety
/// `handle` обязан быть валидным DispatchServerHandle либо null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn shm_dispatch_server_client_count(
    handle: *const DispatchServerHandle,
) -> u32 {
    if handle.is_null() {
        return 0;
    }
    let state = unsafe { &*(handle as *const DispatchServerState) };
    state.inner.client_count()
}

/// # Safety
/// `handle` обязан быть валидным DispatchServerHandle либо null. Поглощает handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn shm_dispatch_server_stop(handle: *mut DispatchServerHandle) {
    if handle.is_null() {
        return;
    }
    unsafe {
        let state = Box::from_raw(handle as *mut DispatchServerState);
        // Раньше .stop() тут вообще не вызывался: lobby worker держит
        // собственный клон Arc<DispatchServer>, поэтому простой drop(state)
        // оставлял worker-поток работать вечно (некому было сказать
        // остановиться) -- перманентная утечка потока/ресурсов, плюс риск
        // UAF, если C-вызывающий код освобождает user_data сразу после
        // этого вызова, считая сервер остановленным.
        state.inner.stop();
        // state drops здесь, освобождая остальное
    }
}

// ─── FFI клиента ─────────────────────────────────────────────────────────────

/// # Safety
/// Все указатели обязаны быть валидны. `name` и `reg.name` обязаны быть валидными C-строками.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn shm_dispatch_client_connect(
    name: *const c_char,
    reg: *const shm_dispatch_registration_t,
    callbacks: *const shm_dispatch_client_callbacks_t,
    options: *const shm_dispatch_client_options_t,
) -> *mut DispatchClientHandle {
    let name_str = match unsafe { to_rust_str(name) } {
        Some(n) => n,
        None => return null_mut(),
    };

    if reg.is_null() || callbacks.is_null() {
        return null_mut();
    }

    let reg_val = unsafe { &*reg };
    let proc_name = unsafe { to_rust_str(reg_val.name) }.unwrap_or_default();
    let registration = ClientRegistration {
        pid: reg_val.pid,
        revision: reg_val.revision,
        name: proc_name,
    };

    let callbacks_val = unsafe { *callbacks };
    let handler = Arc::new(FfiDispatchClientHandler {
        callbacks: callbacks_val,
    });
    let opts = unsafe { to_dispatch_client_options(options) };

    match DispatchClient::connect(&name_str, registration, handler, opts) {
        Ok(inner) => {
            Box::into_raw(Box::new(DispatchClientState { inner })) as *mut DispatchClientHandle
        }
        Err(err) => {
            if let Some(cb) = callbacks_val.on_error {
                cb(err.into(), callbacks_val.user_data);
            }
            null_mut()
        }
    }
}

/// # Safety
/// `handle` обязан быть валидным DispatchClientHandle. `data` обязан указывать на `size` байт.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn shm_dispatch_client_send(
    handle: *mut DispatchClientHandle,
    data: *const c_void,
    size: u32,
) -> shm_error_t {
    if handle.is_null() || data.is_null() || size == 0 || size as usize > MAX_MESSAGE_SIZE {
        return shm_error_t::SHM_ERROR_INVALID_PARAM;
    }
    let state = unsafe { &*(handle as *const DispatchClientState) };
    let slice = unsafe { std::slice::from_raw_parts(data as *const u8, size as usize) };
    match state.inner.send(slice) {
        Ok(()) => shm_error_t::SHM_SUCCESS,
        Err(err) => err.into(),
    }
}

/// # Safety
/// `handle` обязан быть валидным DispatchClientHandle либо null. Поглощает handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn shm_dispatch_client_stop(handle: *mut DispatchClientHandle) {
    if handle.is_null() {
        return;
    }
    unsafe {
        drop(Box::from_raw(handle as *mut DispatchClientState));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn shm_dispatch_options_default() -> shm_dispatch_options_t {
    shm_dispatch_options_t::default()
}

#[unsafe(no_mangle)]
pub extern "C" fn shm_dispatch_client_options_default() -> shm_dispatch_client_options_t {
    shm_dispatch_client_options_t::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Регрессия (аудит 2026-07-10): embedded NUL раньше приводил к тому,
    /// что C-колбэк получал ПУСТУЮ строку (`CString::new(...).unwrap_or_default()`).
    /// Теперь -- усечение по первому NUL, сохраняя всё, что было до него.
    #[test]
    fn to_c_string_lossy_truncates_at_first_nul_instead_of_going_empty() {
        let with_nul = "before\0after";
        let result = to_c_string_lossy(with_nul);
        assert_eq!(result.to_str().unwrap(), "before");
        assert_ne!(result.to_str().unwrap(), "", "не должно тихо стать пустым");
    }

    #[test]
    fn to_c_string_lossy_passes_through_clean_string() {
        let clean = "normal_name.exe";
        let result = to_c_string_lossy(clean);
        assert_eq!(result.to_str().unwrap(), clean);
    }

    #[test]
    fn to_c_string_lossy_handles_leading_nul() {
        let result = to_c_string_lossy("\0anything");
        assert_eq!(result.to_str().unwrap(), "");
    }
}
