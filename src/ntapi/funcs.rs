//! Прямые объявления NT функций из ntdll.dll
//!
//! Статическая линковка через #[link(name = "ntdll")].
//! Никакого GetProcAddress, никакого TLS.

#![allow(non_snake_case)]
#![allow(dead_code)]

use super::types::*;

#[link(name = "ntdll")]
extern "system" {
    // ========================================================================
    // Handle operations
    // ========================================================================

    /// Закрытие handle
    pub fn NtClose(Handle: HANDLE) -> NTSTATUS;

    // ========================================================================
    // Event operations
    // ========================================================================

    /// Создание события
    ///
    /// EventType: NOTIFICATION_EVENT (0) или SYNCHRONIZATION_EVENT (1)
    pub fn NtCreateEvent(
        EventHandle: *mut HANDLE,
        DesiredAccess: ACCESS_MASK,
        ObjectAttributes: *mut OBJECT_ATTRIBUTES,
        EventType: ULONG,
        InitialState: BOOLEAN,
    ) -> NTSTATUS;

    /// Открытие существующего события
    pub fn NtOpenEvent(
        EventHandle: *mut HANDLE,
        DesiredAccess: ACCESS_MASK,
        ObjectAttributes: *mut OBJECT_ATTRIBUTES,
    ) -> NTSTATUS;

    /// Установка события в сигнальное состояние
    pub fn NtSetEvent(
        EventHandle: HANDLE,
        PreviousState: *mut i32,
    ) -> NTSTATUS;

    /// Сброс события
    pub fn NtResetEvent(
        EventHandle: HANDLE,
        PreviousState: *mut i32,
    ) -> NTSTATUS;

    // ========================================================================
    // Wait operations
    // ========================================================================

    /// Ожидание одного объекта
    ///
    /// Timeout: NULL = бесконечно, отрицательное = относительное (100ns units)
    pub fn NtWaitForSingleObject(
        Handle: HANDLE,
        Alertable: BOOLEAN,
        Timeout: *const i64,
    ) -> NTSTATUS;

    /// Ожидание нескольких объектов
    ///
    /// WaitType: WAIT_ALL (0) или WAIT_ANY (1)
    pub fn NtWaitForMultipleObjects(
        Count: ULONG,
        Handles: *const HANDLE,
        WaitType: ULONG,
        Alertable: BOOLEAN,
        Timeout: *const i64,
    ) -> NTSTATUS;

    // ========================================================================
    // Section operations (Shared Memory)
    // ========================================================================

    /// Создание секции (file mapping)
    pub fn NtCreateSection(
        SectionHandle: *mut HANDLE,
        DesiredAccess: ACCESS_MASK,
        ObjectAttributes: *mut OBJECT_ATTRIBUTES,
        MaximumSize: *mut LARGE_INTEGER,
        SectionPageProtection: ULONG,
        AllocationAttributes: ULONG,
        FileHandle: HANDLE,
    ) -> NTSTATUS;

    /// Открытие существующей секции
    pub fn NtOpenSection(
        SectionHandle: *mut HANDLE,
        DesiredAccess: ACCESS_MASK,
        ObjectAttributes: *mut OBJECT_ATTRIBUTES,
    ) -> NTSTATUS;

    /// Маппинг секции в адресное пространство процесса
    ///
    /// ProcessHandle: -1 (NtCurrentProcess) для текущего процесса
    /// InheritDisposition: VIEW_UNMAP (2) - стандартное значение
    pub fn NtMapViewOfSection(
        SectionHandle: HANDLE,
        ProcessHandle: HANDLE,
        BaseAddress: *mut PVOID,
        ZeroBits: ULONG_PTR,
        CommitSize: usize,
        SectionOffset: *mut LARGE_INTEGER,
        ViewSize: *mut usize,
        InheritDisposition: ULONG,
        AllocationType: ULONG,
        Win32Protect: ULONG,
    ) -> NTSTATUS;

    /// Размаппинг секции
    pub fn NtUnmapViewOfSection(
        ProcessHandle: HANDLE,
        BaseAddress: PVOID,
    ) -> NTSTATUS;

    // ========================================================================
    // Security Descriptor helpers (Rtl* functions)
    // ========================================================================

    /// Инициализация Security Descriptor
    pub fn RtlCreateSecurityDescriptor(
        SecurityDescriptor: *mut SECURITY_DESCRIPTOR,
        Revision: ULONG,
    ) -> NTSTATUS;

    /// Установка DACL в Security Descriptor
    pub fn RtlSetDaclSecurityDescriptor(
        SecurityDescriptor: *mut SECURITY_DESCRIPTOR,
        DaclPresent: BOOLEAN,
        Dacl: PVOID,
        DaclDefaulted: BOOLEAN,
    ) -> NTSTATUS;
}

// ============================================================================
// Вспомогательные константы
// ============================================================================

/// Псевдо-handle текущего процесса
pub const NT_CURRENT_PROCESS: HANDLE = -1isize as HANDLE;

/// SECURITY_DESCRIPTOR_REVISION
pub const SECURITY_DESCRIPTOR_REVISION: ULONG = 1;
