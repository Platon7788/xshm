//! NT API типы для прямых вызовов ntdll.dll
//!
//! Минимальный набор типов без внешних зависимостей.

#![expect(
    non_camel_case_types,
    reason = "имена типов NT API воспроизводятся дословно"
)]
#![expect(
    non_snake_case,
    reason = "имена полей NT-структур воспроизводятся дословно"
)]
#![expect(
    clippy::upper_case_acronyms,
    reason = "HANDLE/NTSTATUS/PVOID -- имена из NT API"
)]

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
    pub const fn new(
        name: *mut UNICODE_STRING,
        attributes: ULONG,
        security_descriptor: PVOID,
    ) -> Self {
        Self {
            Length: size_of::<OBJECT_ATTRIBUTES>() as ULONG,
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
    pub const fn new() -> Self {
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

    pub const fn as_ptr(&mut self) -> PVOID {
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

        // SAFETY: `sd` -- локальная структура нужного размера, живущая до конца
        // функции; обе Rtl-функции только заполняют её поля по переданной ссылке.
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

    pub const fn as_ptr(&mut self) -> PVOID {
        self.sd.as_ptr()
    }
}

impl Default for NullDaclSecurityDescriptor {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Process information (для session id)
// ============================================================================

/// PROCESSINFOCLASS::ProcessSessionInformation
pub const PROCESS_SESSION_INFORMATION_CLASS: ULONG = 24;

#[repr(C)]
pub struct PROCESS_SESSION_INFORMATION {
    pub SessionId: ULONG,
}

// ============================================================================
// CLIENT_ID / NtOpenProcess (для liveness-проверки процесса по PID)
// ============================================================================

/// CLIENT_ID -- идентифицирует процесс/поток для NtOpenProcess/NtOpenThread.
/// UniqueProcess -- это PID, но передаётся как HANDLE-подобное значение
/// (см. `PsGetProcessId`/`ClientId` в WDK) -- отсюда тип HANDLE, а не ULONG.
#[repr(C)]
pub struct CLIENT_ID {
    pub UniqueProcess: HANDLE,
    pub UniqueThread: HANDLE,
}

/// Достаточно для WaitForSingleObject (определить, жив ли процесс) и не
/// требует повышенных привилегий -- в отличие от PROCESS_ALL_ACCESS.
pub const PROCESS_QUERY_LIMITED_INFORMATION: ACCESS_MASK = 0x1000;
/// Право на ожидание сигнального состояния процесса (завершение).
pub const PROCESS_SYNCHRONIZE: ACCESS_MASK = 0x0010_0000;

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
/// Событие-уведомление: остаётся взведённым, будит всех ждущих, сбрасывается
/// явно (`NtResetEvent`).
pub const NOTIFICATION_EVENT: ULONG = 0;
/// Открыть существующий именованный объект вместо ошибки коллизии имени.
pub const OBJ_OPENIF: ULONG = 0x0000_0080;
/// Информационный успех `OBJ_OPENIF`: объект уже существовал и открыт.
pub const STATUS_OBJECT_NAME_EXISTS: NTSTATUS = 0x4000_0000;
/// Имя занято объектом другого типа (например, мьютекс лобби -- событием).
pub const STATUS_OBJECT_TYPE_MISMATCH: NTSTATUS = 0xC000_0024_u32 as NTSTATUS;

/// `EVENT_INFORMATION_CLASS::EventBasicInformation` для `NtQueryEvent` (0.9).
pub const EVENT_BASIC_INFORMATION_CLASS: ULONG = 0;

/// Ответ `NtQueryEvent(EventBasicInformation)`: тип события
/// (`NOTIFICATION_EVENT`/`SYNCHRONIZATION_EVENT`) и текущее состояние.
#[repr(C)]
#[derive(Debug, Default)]
pub struct EVENT_BASIC_INFORMATION {
    pub EventType: ULONG,
    pub EventState: i32,
}

// ============================================================================
// Константы для Mutant (мьютекс лобби Dispatch, 0.9)
// ============================================================================

/// `MUTANT_ALL_ACCESS` = STANDARD_RIGHTS_REQUIRED | SYNCHRONIZE | MUTANT_QUERY_STATE.
pub const MUTANT_ALL_ACCESS: ACCESS_MASK = 0x001F_0001;
/// Ожидание мьютекса завершилось захватом «брошенного» мьютекса: прежний
/// поток-владелец завершился, не освободив его. Захват при этом состоялся.
pub const STATUS_ABANDONED_WAIT_0: NTSTATUS = 0x0000_0080;

// ============================================================================
// Константы для Wait
// ============================================================================

/// WaitAny - вернуться когда любой объект сигнализирован
pub const WAIT_ANY: ULONG = 1;
