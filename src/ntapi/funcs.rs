//! Прямые объявления NT функций из ntdll.dll
//!
//! Импорт через `#[link(name = "ntdll", kind = "raw-dylib")]`: линкер сам
//! строит таблицу импорта, `ntdll.lib` из Windows SDK не нужна.
//! Никакого GetProcAddress, никакого TLS.
//!
//! На x86 `raw-dylib` по умолчанию импортирует stdcall-функции под
//! декорированными именами (`_NtClose@4`), а ntdll экспортирует их без
//! декорации -- загрузчик падает с `STATUS_ENTRYPOINT_NOT_FOUND`. Поэтому на
//! x86 явно `import_name_type = "undecorated"` (на x64 атрибут запрещён).

use super::types::*;

#[cfg_attr(
    target_arch = "x86",
    link(name = "ntdll", kind = "raw-dylib", import_name_type = "undecorated")
)]
#[cfg_attr(not(target_arch = "x86"), link(name = "ntdll", kind = "raw-dylib"))]
unsafe extern "system" {
    // ========================================================================
    // Handle operations
    // ========================================================================

    /// Закрытие handle
    pub fn NtClose(Handle: HANDLE) -> NTSTATUS;

    /// Запрос информации о процессе (используем ProcessSessionInformation)
    pub fn NtQueryInformationProcess(
        ProcessHandle: HANDLE,
        ProcessInformationClass: ULONG,
        ProcessInformation: PVOID,
        ProcessInformationLength: ULONG,
        ReturnLength: *mut ULONG,
    ) -> NTSTATUS;

    /// Открытие процесса по PID (через CLIENT_ID.UniqueProcess).
    ///
    /// ObjectAttributes с ObjectName = NULL: процессы не именованные объекты
    /// BaseNamedObjects, идентифицируются только через ClientId.
    pub fn NtOpenProcess(
        ProcessHandle: *mut HANDLE,
        DesiredAccess: ACCESS_MASK,
        ObjectAttributes: *mut OBJECT_ATTRIBUTES,
        ClientId: *mut CLIENT_ID,
    ) -> NTSTATUS;

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
    pub fn NtSetEvent(EventHandle: HANDLE, PreviousState: *mut i32) -> NTSTATUS;

    /// Сброс события в несигнальное состояние
    pub fn NtResetEvent(EventHandle: HANDLE, PreviousState: *mut i32) -> NTSTATUS;

    /// Сведения о событии (0.9: проверка типа существующего маяка)
    ///
    /// EventInformationClass: EVENT_BASIC_INFORMATION_CLASS (0)
    pub fn NtQueryEvent(
        EventHandle: HANDLE,
        EventInformationClass: ULONG,
        EventInformation: PVOID,
        EventInformationLength: ULONG,
        ReturnLength: *mut ULONG,
    ) -> NTSTATUS;

    // ========================================================================
    // Mutant operations (0.9: сериализация клиентов лобби Dispatch)
    // ========================================================================

    /// Создание (или открытие с `OBJ_OPENIF`) мьютекса
    pub fn NtCreateMutant(
        MutantHandle: *mut HANDLE,
        DesiredAccess: ACCESS_MASK,
        ObjectAttributes: *mut OBJECT_ATTRIBUTES,
        InitialOwner: BOOLEAN,
    ) -> NTSTATUS;

    /// Освобождение мьютекса потоком-владельцем
    pub fn NtReleaseMutant(MutantHandle: HANDLE, PreviousCount: *mut i32) -> NTSTATUS;

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
    pub fn NtUnmapViewOfSection(ProcessHandle: HANDLE, BaseAddress: PVOID) -> NTSTATUS;

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

    /// Установка SACL (метка целостности) в Security Descriptor
    pub fn RtlSetSaclSecurityDescriptor(
        SecurityDescriptor: *mut SECURITY_DESCRIPTOR,
        SaclPresent: BOOLEAN,
        Sacl: *mut ACL,
        SaclDefaulted: BOOLEAN,
    ) -> NTSTATUS;

    /// Инициализация пустого ACL в буфере длины `AclLength`
    pub fn RtlCreateAcl(Acl: *mut ACL, AclLength: ULONG, AclRevision: ULONG) -> NTSTATUS;

    /// Добавление разрешающей записи ACE для SID
    pub fn RtlAddAccessAllowedAce(
        Acl: *mut ACL,
        AceRevision: ULONG,
        AccessMask: ACCESS_MASK,
        Sid: PSID,
    ) -> NTSTATUS;

    /// Добавление обязательной метки (SYSTEM_MANDATORY_LABEL_ACE) в SACL
    pub fn RtlAddMandatoryAce(
        Acl: *mut ACL,
        AceRevision: ULONG,
        AceFlags: ULONG,
        Sid: PSID,
        AceType: u8,
        AccessMask: ACCESS_MASK,
    ) -> NTSTATUS;

    /// Длина SID в байтах
    pub fn RtlLengthSid(Sid: PSID) -> ULONG;

    /// Проверка self-relative дескриптора безопасности длины
    /// `SecurityDescriptorLength` (все смещения -- в пределах буфера);
    /// `RequiredInformation` -- `SECURITY_INFORMATION`, обязательные части
    /// (0 -- никаких). Ненулевой результат -- дескриптор корректен.
    pub fn RtlValidRelativeSecurityDescriptor(
        SecurityDescriptorInput: PVOID,
        SecurityDescriptorLength: ULONG,
        RequiredInformation: ULONG,
    ) -> BOOLEAN;

    // ========================================================================
    // Section/Token queries (0.10)
    // ========================================================================

    /// Сведения о секции (SectionBasicInformation: реальный размер, атрибуты)
    pub fn NtQuerySection(
        SectionHandle: HANDLE,
        SectionInformationClass: ULONG,
        SectionInformation: PVOID,
        SectionInformationLength: usize,
        ReturnLength: *mut usize,
    ) -> NTSTATUS;

    /// Открытие токена процесса
    pub fn NtOpenProcessToken(
        ProcessHandle: HANDLE,
        DesiredAccess: ACCESS_MASK,
        TokenHandle: *mut HANDLE,
    ) -> NTSTATUS;

    /// Запрос сведений о токене (TokenUser -- SID пользователя)
    pub fn NtQueryInformationToken(
        TokenHandle: HANDLE,
        TokenInformationClass: ULONG,
        TokenInformation: PVOID,
        TokenInformationLength: ULONG,
        ReturnLength: *mut ULONG,
    ) -> NTSTATUS;

    // ========================================================================
    // Private namespaces (0.10)
    // ========================================================================
    //
    // Сигнатуры сверены с phnt (ntrtl.h / ntobapi.h): все NTAPI (stdcall на
    // x86), `PCUNICODE_STRING` -- указатель на неизменяемую строку.

    /// Создание дескриптора границы; NULL при нехватке памяти
    pub fn RtlCreateBoundaryDescriptor(
        Name: *const UNICODE_STRING,
        Flags: ULONG,
    ) -> POBJECT_BOUNDARY_DESCRIPTOR;

    /// Добавление SID в дескриптор границы (может перевыделить дескриптор)
    pub fn RtlAddSIDToBoundaryDescriptor(
        BoundaryDescriptor: *mut POBJECT_BOUNDARY_DESCRIPTOR,
        RequiredSid: PSID,
    ) -> NTSTATUS;

    /// Добавление метки целостности в дескриптор границы (может
    /// перевыделить дескриптор)
    pub fn RtlAddIntegrityLabelToBoundaryDescriptor(
        BoundaryDescriptor: *mut POBJECT_BOUNDARY_DESCRIPTOR,
        IntegrityLabel: PSID,
    ) -> NTSTATUS;

    /// Освобождение дескриптора границы
    pub fn RtlDeleteBoundaryDescriptor(BoundaryDescriptor: POBJECT_BOUNDARY_DESCRIPTOR);

    /// Создание частного пространства имён
    pub fn NtCreatePrivateNamespace(
        NamespaceHandle: *mut HANDLE,
        DesiredAccess: ACCESS_MASK,
        ObjectAttributes: *mut OBJECT_ATTRIBUTES,
        BoundaryDescriptor: POBJECT_BOUNDARY_DESCRIPTOR,
    ) -> NTSTATUS;

    /// Открытие существующего частного пространства имён
    pub fn NtOpenPrivateNamespace(
        NamespaceHandle: *mut HANDLE,
        DesiredAccess: ACCESS_MASK,
        ObjectAttributes: *mut OBJECT_ATTRIBUTES,
        BoundaryDescriptor: POBJECT_BOUNDARY_DESCRIPTOR,
    ) -> NTSTATUS;

    // `NtDeletePrivateNamespace` намеренно не объявлен: xshm пространство не
    // удаляет (см. `namespace.rs` -- пиры остались бы в «висящем» каталоге).

    // ========================================================================
    // IO completion и пакеты ожидания завершения (0.10)
    // ========================================================================

    /// Создание порта завершения ввода-вывода
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "потребитель -- реактор на IOCP (T15)")
    )]
    pub fn NtCreateIoCompletion(
        IoCompletionHandle: *mut HANDLE,
        DesiredAccess: ACCESS_MASK,
        ObjectAttributes: *mut OBJECT_ATTRIBUTES,
        Count: ULONG,
    ) -> NTSTATUS;

    /// Извлечение до `Count` пакетов из порта
    ///
    /// Timeout: NULL = бесконечно, отрицательное = относительное (100ns units)
    #[expect(dead_code, reason = "потребитель -- реактор на IOCP (T15)")]
    pub fn NtRemoveIoCompletionEx(
        IoCompletionHandle: HANDLE,
        IoCompletionInformation: *mut FILE_IO_COMPLETION_INFORMATION,
        Count: ULONG,
        NumEntriesRemoved: *mut ULONG,
        Timeout: *const i64,
        Alertable: BOOLEAN,
    ) -> NTSTATUS;

    /// Постановка пакета в порт
    #[expect(dead_code, reason = "потребитель -- реактор на IOCP (T15)")]
    pub fn NtSetIoCompletion(
        IoCompletionHandle: HANDLE,
        KeyContext: PVOID,
        ApcContext: PVOID,
        IoStatus: NTSTATUS,
        IoStatusInformation: ULONG_PTR,
    ) -> NTSTATUS;

    /// Создание пакета ожидания завершения
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "потребитель -- реактор на IOCP (T15)")
    )]
    pub fn NtCreateWaitCompletionPacket(
        WaitCompletionPacketHandle: *mut HANDLE,
        DesiredAccess: ACCESS_MASK,
        ObjectAttributes: *mut OBJECT_ATTRIBUTES,
    ) -> NTSTATUS;

    /// Привязка пакета: при сигнале `TargetObjectHandle` пакет попадёт в порт
    #[expect(dead_code, reason = "потребитель -- реактор на IOCP (T15)")]
    pub fn NtAssociateWaitCompletionPacket(
        WaitCompletionPacketHandle: HANDLE,
        IoCompletionHandle: HANDLE,
        TargetObjectHandle: HANDLE,
        KeyContext: PVOID,
        ApcContext: PVOID,
        IoStatus: NTSTATUS,
        IoStatusInformation: ULONG_PTR,
        AlreadySignaled: *mut BOOLEAN,
    ) -> NTSTATUS;

    /// Отмена привязки пакета
    #[expect(dead_code, reason = "потребитель -- реактор на IOCP (T15)")]
    pub fn NtCancelWaitCompletionPacket(
        WaitCompletionPacketHandle: HANDLE,
        RemoveSignaledPacket: BOOLEAN,
    ) -> NTSTATUS;
}

// ============================================================================
// Вспомогательные константы
// ============================================================================

/// Псевдо-handle текущего процесса
pub const NT_CURRENT_PROCESS: HANDLE = -1isize as HANDLE;

/// SECURITY_DESCRIPTOR_REVISION
pub const SECURITY_DESCRIPTOR_REVISION: ULONG = 1;
