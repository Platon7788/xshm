//! Низкоуровневый C ABI. Указатели соединений принадлежат C-аллокатору.
//! Все вызовы одного соединения обязаны быть последовательными.
use std::ffi::{c_char, c_void};

/// Непрозрачное соединение: его внутреннюю раскладку Rust не копирует.
#[repr(C)]
#[derive(Debug)]
pub struct Conn {
    _private: [u8; 0],
}

/// Непрозрачное пространство имён, которым владеет C.
#[repr(C)]
#[derive(Debug)]
pub struct Namespace {
    _private: [u8; 0],
}
/// Непрозрачный маяк, которым владеет C.
#[repr(C)]
#[derive(Debug)]
pub struct Beacon {
    _private: [u8; 0],
}

/// Непрозрачный многоклиентский сервер и заимствованное хранилище слотов.
#[repr(C)]
#[derive(Debug)]
pub struct Multi {
    _private: [u8; 0],
}
#[repr(C)]
#[derive(Debug)]
pub struct MultiSlot {
    _private: [u8; 0],
}
/// Непрозрачный Dispatch-сервер и его заимствованные слоты.
#[repr(C)]
#[derive(Debug)]
pub struct Dispatch {
    _private: [u8; 0],
}
#[repr(C)]
#[derive(Debug)]
pub struct DispatchSlot {
    _private: [u8; 0],
}

/// Декодированная регистрация; имя содержит name_len байт, включая возможные NUL.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Registration {
    pub pid: u32,
    pub revision: u16,
    pub name_len: u8,
    pub name: [u8; 65],
}
pub type AdmitFn = unsafe extern "C" fn(*mut c_void, *const Registration) -> bool;
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DispatchOptions {
    pub max_clients: u32,
    pub lobby_timeout_ms: u32,
    pub channel_connect_timeout_ms: u32,
    pub lobby_layout: Layout,
    pub channel_layout: Layout,
    pub security: Security,
    pub ns: *const Namespace,
    pub admit: Option<AdmitFn>,
    pub admit_ctx: *mut c_void,
}
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DispatchClientOptions {
    pub lobby_timeout_ms: u32,
    pub response_timeout_ms: u32,
    pub channel_timeout_ms: u32,
    pub security: Security,
    pub ns: *const Namespace,
}
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DispatchEvent {
    pub kind: i32,
    pub client_id: u32,
    pub len: u32,
    pub reason: i32,
    pub error: i32,
    pub reg: *const Registration,
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
    pub ns: *const Namespace,
}

/// Публичные параметры и событие Multi; enum C представлен числом.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MultiOptions {
    pub layout: Layout,
    pub security: Security,
    pub ns: *const Namespace,
    pub recv_batch: u32,
}
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct MultiEvent {
    pub kind: i32,
    pub slot: u32,
    pub len: u32,
    pub reason: i32,
    pub error: i32,
}
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MultiClientOptions {
    pub slot_timeout_ms: u32,
    pub retry_delay_ms: u32,
    pub ns: *const Namespace,
    pub discovery_fallback_ms: u32,
    pub security: *const Security,
}
/// Поиск доступного одиночного канала через уведомления C-ядра.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ClientWaitOptions {
    pub handshake_timeout_ms: u32,
    pub retry_delay_ms: u32,
    pub discovery_fallback_ms: u32,
    pub security: *const Security,
    pub ns: *const Namespace,
}

const _: () = {
    assert!(size_of::<Layout>() == 24);
    assert!(size_of::<MultiEvent>() == 20);
    assert!(size_of::<Registration>() == 72);
    assert!(std::mem::offset_of!(Registration, name) == 7);
    assert!(std::mem::offset_of!(DispatchOptions, channel_layout) == 36);
    assert!(std::mem::offset_of!(ChannelConfig, security) == 24);
    if cfg!(target_pointer_width = "64") {
        assert!(size_of::<Security>() == 24 && size_of::<ChannelConfig>() == 56);
        assert!(std::mem::offset_of!(ChannelConfig, ns) == 48);
        assert!(size_of::<MultiOptions>() == 64);
        assert!(size_of::<MultiClientOptions>() == 32);
        assert!(std::mem::offset_of!(MultiClientOptions, security) == 24);
        assert!(size_of::<DispatchOptions>() == 112);
        assert!(size_of::<DispatchClientOptions>() == 48);
        assert!(size_of::<DispatchEvent>() == 32);
        assert!(size_of::<ClientWaitOptions>() == 32);
        assert!(std::mem::offset_of!(ClientWaitOptions, security) == 16);
        assert!(std::mem::offset_of!(DispatchOptions, admit) == 96);
        assert!(std::mem::offset_of!(DispatchEvent, reg) == 24);
    } else {
        assert!(size_of::<Security>() == 12 && size_of::<ChannelConfig>() == 40);
        assert!(std::mem::offset_of!(ChannelConfig, ns) == 36);
        assert!(size_of::<MultiOptions>() == 44);
        assert!(size_of::<MultiClientOptions>() == 20);
        assert!(std::mem::offset_of!(MultiClientOptions, security) == 16);
        assert!(size_of::<DispatchOptions>() == 84);
        assert!(size_of::<DispatchClientOptions>() == 28);
        assert!(size_of::<DispatchEvent>() == 24);
        assert!(size_of::<ClientWaitOptions>() == 20);
        assert!(std::mem::offset_of!(ClientWaitOptions, security) == 12);
        assert!(std::mem::offset_of!(DispatchOptions, admit) == 76);
        assert!(std::mem::offset_of!(DispatchEvent, reg) == 20);
    }
};

// SAFETY: объявления соответствуют public-заголовкам xipc ABI 2;
// размеры repr(C)-структур проверены и rustc, и C-компилятором при сборке.
unsafe extern "C" {
    pub fn xipc_client_wait_options_default() -> ClientWaitOptions;
    pub fn xipc_client_connect_wait(
        conn: *mut Conn,
        name: *const c_char,
        options: *const ClientWaitOptions,
        timeout: u32,
        cancel: *mut c_void,
    ) -> i32;
    pub fn xipc_wait_for_space(
        conn: *mut Conn,
        payload_len: u32,
        timeout: u32,
        cancel: *mut c_void,
    ) -> i32;
    pub fn xipc_wait_activity(
        conn: *mut Conn,
        pending_len: u32,
        timeout: u32,
        cancel: *mut c_void,
        ready: *mut u32,
        reason: *mut i32,
    ) -> i32;
    pub fn xipc_conn_layout(conn: *const Conn) -> *const Layout;
    pub fn xipc_process_exit_open(pid: u32, handle: *mut *mut c_void) -> i32;
    pub fn xipc_process_exit_close(handle: *mut c_void);
    pub fn xipc_dispatch_options_default() -> DispatchOptions;
    pub fn xipc_dispatch_client_options_default() -> DispatchClientOptions;
    pub fn xipc_dispatch_server_alloc(count: u32, slots: *mut *mut DispatchSlot) -> *mut Dispatch;
    pub fn xipc_dispatch_server_free(server: *mut Dispatch);
    pub fn xipc_dispatch_server_start(
        server: *mut Dispatch,
        slots: *mut DispatchSlot,
        count: u32,
        lobby: *const c_char,
        options: *const DispatchOptions,
    ) -> i32;
    pub fn xipc_dispatch_server_next(
        server: *mut Dispatch,
        buffer: *mut c_void,
        capacity: u32,
        timeout: u32,
        event: *mut DispatchEvent,
    ) -> i32;
    pub fn xipc_dispatch_try_send(
        server: *mut Dispatch,
        id: u32,
        data: *const c_void,
        len: u32,
    ) -> i32;
    pub fn xipc_dispatch_send(server: *mut Dispatch, id: u32, data: *const c_void, len: u32)
    -> i32;
    pub fn xipc_dispatch_disconnect_client(server: *mut Dispatch, id: u32) -> i32;
    pub fn xipc_dispatch_wake(server: *mut Dispatch) -> i32;
    pub fn xipc_dispatch_connect(
        lobby: *const c_char,
        revision: u16,
        name: *const c_char,
        options: *const DispatchClientOptions,
        cancel: *mut c_void,
        conn: *mut Conn,
        id: *mut u32,
        channel_name: *mut c_char,
    ) -> i32;
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
        ns: *const Namespace,
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
    pub fn xipc_namespace_alloc() -> *mut Namespace;
    pub fn xipc_namespace_free(ns: *mut Namespace);
    pub fn xipc_namespace_open(
        ns: *mut Namespace,
        boundary: *const c_char,
        alias: *const c_char,
        security: *const Security,
    ) -> i32;
    pub fn xipc_beacon_alloc() -> *mut Beacon;
    pub fn xipc_beacon_free(beacon: *mut Beacon);
    pub fn xipc_beacon_open(
        beacon: *mut Beacon,
        name: *const c_char,
        security: *const Security,
        ns: *const Namespace,
    ) -> i32;
    pub fn xipc_beacon_unnamed(beacon: *mut Beacon) -> i32;
    pub fn xipc_beacon_raise(beacon: *mut Beacon) -> i32;
    pub fn xipc_beacon_lower(beacon: *mut Beacon) -> i32;
    pub fn xipc_beacon_clear_if_stale(beacon: *mut Beacon, cleared: *mut bool) -> i32;
    pub fn xipc_beacon_wait(beacon: *const Beacon, timeout: u32) -> i32;
    pub fn xipc_beacon_handle(beacon: *const Beacon) -> *mut c_void;
    pub fn xipc_wait_any(handles: *const *mut c_void, count: u32, timeout: u32) -> i32;
    pub fn xipc_multi_options_default() -> MultiOptions;
    pub fn xipc_multi_client_options_default() -> MultiClientOptions;
    pub fn xipc_multi_alloc(count: u32, slots: *mut *mut MultiSlot) -> *mut Multi;
    pub fn xipc_multi_free(server: *mut Multi);
    pub fn xipc_multi_start(
        server: *mut Multi,
        slots: *mut MultiSlot,
        count: u32,
        base: *const c_char,
        options: *const MultiOptions,
    ) -> i32;
    pub fn xipc_multi_next(
        server: *mut Multi,
        buffer: *mut c_void,
        capacity: u32,
        timeout: u32,
        event: *mut MultiEvent,
    ) -> i32;
    pub fn xipc_multi_try_send(server: *mut Multi, slot: u32, data: *const c_void, len: u32)
    -> i32;
    pub fn xipc_multi_send(server: *mut Multi, slot: u32, data: *const c_void, len: u32) -> i32;
    pub fn xipc_multi_disconnect_client(server: *mut Multi, slot: u32) -> i32;
    pub fn xipc_multi_wake(server: *mut Multi) -> i32;
    pub fn xipc_multi_client_connect(
        conn: *mut Conn,
        base: *const c_char,
        options: *const MultiClientOptions,
        timeout: u32,
        cancel: *mut c_void,
        slot: *mut u32,
    ) -> i32;
}
