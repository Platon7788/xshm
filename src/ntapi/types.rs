//! NT API типы для прямых вызовов ntdll.dll
//!
//! Минимальный набор типов без внешних зависимостей.

#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)]
#![allow(clippy::upper_case_acronyms)]

use core::ffi::c_void;

// ============================================================================
// Базовые типы
// ============================================================================

pub type HANDLE = *mut c_void;
pub type PVOID = *mut c_void;
pub type NTSTATUS = i32;
pub type ULONG = u32;
pub type BOOLEAN = u8;
pub type ACCESS_MASK = u32;
pub type ULONG_PTR = usize;

/// LARGE_INTEGER - 64-bit signed integer
#[repr(C)]
#[derive(Copy, Clone)]
pub union LARGE_INTEGER {
    pub QuadPart: i64,
    pub u: LARGE_INTEGER_PARTS,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct LARGE_INTEGER_PARTS {
    pub LowPart: u32,
    pub HighPart: i32,
}

// ============================================================================
// UNICODE_STRING
// ============================================================================

#[repr(C)]
pub struct UNICODE_STRING {
    pub Length: u16,
    pub MaximumLength: u16,
    pub Buffer: *mut u16,
}

impl Default for UNICODE_STRING {
    fn default() -> Self {
        Self {
            Length: 0,
            MaximumLength: 0,
            Buffer: core::ptr::null_mut(),
        }
    }
}

// ============================================================================
// OBJECT_ATTRIBUTES
// ============================================================================

#[repr(C)]
pub struct OBJECT_ATTRIBUTES {
    pub Length: ULONG,
    pub RootDirectory: HANDLE,
    pub ObjectName: *mut UNICODE_STRING,
    pub Attributes: ULONG,
    pub SecurityDescriptor: PVOID,
    pub SecurityQualityOfService: PVOID,
}

impl OBJECT_ATTRIBUTES {
    pub fn new(name: *mut UNICODE_STRING, attributes: ULONG, security_descriptor: PVOID) -> Self {
        Self {
            Length: core::mem::size_of::<OBJECT_ATTRIBUTES>() as ULONG,
            RootDirectory: core::ptr::null_mut(),
            ObjectName: name,
            Attributes: attributes,
            SecurityDescriptor: security_descriptor,
            SecurityQualityOfService: core::ptr::null_mut(),
        }
    }
}

// ============================================================================
// SECURITY_DESCRIPTOR (для NULL DACL)
// ============================================================================

#[repr(C)]
pub struct SECURITY_DESCRIPTOR {
    pub Revision: u8,
    pub Sbz1: u8,
    pub Control: u16,
    pub Owner: PVOID,
    pub Group: PVOID,
    pub Sacl: PVOID,
    pub Dacl: PVOID,
}

impl SECURITY_DESCRIPTOR {
    /// Создаёт пустой SECURITY_DESCRIPTOR
    pub fn new() -> Self {
        Self {
            Revision: 0,
            Sbz1: 0,
            Control: 0,
            Owner: core::ptr::null_mut(),
            Group: core::ptr::null_mut(),
            Sacl: core::ptr::null_mut(),
            Dacl: core::ptr::null_mut(),
        }
    }

    pub fn as_ptr(&mut self) -> PVOID {
        self as *mut _ as PVOID
    }
}

impl Default for SECURITY_DESCRIPTOR {
    fn default() -> Self {
        Self::new()
    }
}

/// Обёртка для создания NULL DACL Security Descriptor через Rtl функции
pub struct NullDaclSecurityDescriptor {
    sd: SECURITY_DESCRIPTOR,
}

impl NullDaclSecurityDescriptor {
    /// Создаёт SECURITY_DESCRIPTOR с NULL DACL (полный доступ для всех)
    /// Использует RtlCreateSecurityDescriptor и RtlSetDaclSecurityDescriptor
    pub fn new() -> Self {
        use super::funcs::{
            RtlCreateSecurityDescriptor, RtlSetDaclSecurityDescriptor, SECURITY_DESCRIPTOR_REVISION,
        };

        let mut sd = SECURITY_DESCRIPTOR::new();

        unsafe {
            // Инициализируем SD
            let _ = RtlCreateSecurityDescriptor(&mut sd, SECURITY_DESCRIPTOR_REVISION);
            // Устанавливаем NULL DACL
            let _ = RtlSetDaclSecurityDescriptor(
                &mut sd,
                1,                     // DaclPresent = TRUE
                core::ptr::null_mut(), // Dacl = NULL (full access)
                0,                     // DaclDefaulted = FALSE
            );
        }

        Self { sd }
    }

    pub fn as_ptr(&mut self) -> PVOID {
        self.sd.as_ptr()
    }
}

impl Default for NullDaclSecurityDescriptor {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Константы NTSTATUS
// ============================================================================

pub const STATUS_SUCCESS: NTSTATUS = 0;
pub const STATUS_TIMEOUT: NTSTATUS = 0x00000102;
pub const STATUS_WAIT_0: NTSTATUS = 0;

// ============================================================================
// Константы OBJECT_ATTRIBUTES
// ============================================================================

pub const OBJ_CASE_INSENSITIVE: ULONG = 0x00000040;

// ============================================================================
// Константы для Section
// ============================================================================

pub const SECTION_ALL_ACCESS: ACCESS_MASK = 0x000F001F;
pub const PAGE_READWRITE: ULONG = 0x04;
pub const SEC_COMMIT: ULONG = 0x08000000;

/// ViewUnmap - секция будет размаппена при закрытии handle
pub const VIEW_UNMAP: ULONG = 2;

// ============================================================================
// Константы для Event
// ============================================================================

pub const EVENT_ALL_ACCESS: ACCESS_MASK = 0x001F0003;

/// SynchronizationEvent - auto-reset event
pub const SYNCHRONIZATION_EVENT: ULONG = 1;
/// NotificationEvent - manual-reset event
pub const NOTIFICATION_EVENT: ULONG = 0;

// ============================================================================
// Константы для Wait
// ============================================================================

/// WaitAny - вернуться когда любой объект сигнализирован
pub const WAIT_ANY: ULONG = 1;
/// WaitAll - вернуться когда все объекты сигнализированы
pub const WAIT_ALL: ULONG = 0;
