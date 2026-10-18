//! Низкоуровневый C ABI. Указатели соединений принадлежат C-аллокатору.
//! Все вызовы одного соединения обязаны быть последовательными.
use std::ffi::{c_char, c_void};

/// Непрозрачное соединение: его внутреннюю раскладку Rust не копирует.
#[repr(C)]
#[derive(Debug)]
pub struct Conn {
    _private: [u8; 0],
}

/// Параметры одного кольца, стабильная публичная структура C.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RingConfig {
    pub capacity: u32,
    pub max_messages: u32,
    pub max_message_size: u32,
}
/// Раскладка двух направлений.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Layout {
    pub s2c: RingConfig,
    pub c2s: RingConfig,
}
/// Политика создания объектов. Дескриптор заимствован на время вызова.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Security {
    pub kind: i32,
    pub descriptor: *const c_void,
    pub descriptor_len: u32,
}
/// Конфигурация именованного канала.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ChannelConfig {
    pub layout: Layout,
    pub security: Security,
    pub ns: *const c_void,
}

const _: () = {
    assert!(size_of::<Layout>() == 24);
    assert!(std::mem::offset_of!(ChannelConfig, security) == 24);
    if cfg!(target_pointer_width = "64") {
        assert!(size_of::<Security>() == 24 && size_of::<ChannelConfig>() == 56);
        assert!(std::mem::offset_of!(ChannelConfig, ns) == 48);
    } else {
        assert!(size_of::<Security>() == 12 && size_of::<ChannelConfig>() == 40);
        assert!(std::mem::offset_of!(ChannelConfig, ns) == 36);
    }
};

// SAFETY: объявления соответствуют public-заголовкам xipc ABI 2;
// размеры repr(C)-структур проверены и rustc, и C-компилятором при сборке.
unsafe extern "C" {
    pub fn xipc_conn_alloc() -> *mut Conn;
    pub fn xipc_conn_free(conn: *mut Conn);
    pub fn xipc_channel_config_default() -> ChannelConfig;
    pub fn xipc_layout_validate(layout: *const Layout) -> i32;
    pub fn xipc_server_start(
        conn: *mut Conn,
        name: *const c_char,
        config: *const ChannelConfig,
    ) -> i32;
    pub fn xipc_server_accept(conn: *mut Conn, timeout: u32, cancel: *mut c_void) -> i32;
    pub fn xipc_server_disconnect(conn: *mut Conn) -> i32;
    pub fn xipc_client_connect(
        conn: *mut Conn,
        name: *const c_char,
        ns: *const c_void,
        timeout: u32,
        cancel: *mut c_void,
    ) -> i32;
    pub fn xipc_send(conn: *mut Conn, data: *const c_void, len: u32, overwritten: *mut u32) -> i32;
    pub fn xipc_try_send(conn: *mut Conn, data: *const c_void, len: u32) -> i32;
    pub fn xipc_recv(conn: *mut Conn, data: *mut c_void, capacity: u32, len: *mut u32) -> i32;
    pub fn xipc_wait_readable(
        conn: *mut Conn,
        timeout: u32,
        cancel: *mut c_void,
        reason: *mut i32,
    ) -> i32;
    pub fn xipc_is_connected(conn: *const Conn) -> bool;
    pub fn xipc_peer_pid(conn: *const Conn) -> u32;
    pub fn xipc_conn_generation(conn: *const Conn) -> u32;
    pub fn xipc_conn_end_reason(conn: *const Conn) -> i32;
}
