//! Platform layer - прямые вызовы NT API через ntdll.dll
//!
//! Использует статическую линковку с ntdll.dll.
//! Никаких внешних зависимостей, никакого TLS.
//!
//! ВАЖНО: Поддерживаются только x86 и x86_64 архитектуры!

// Compile-time проверка архитектуры
#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
compile_error!("xShm поддерживает только x86 и x86_64 архитектуры!");

use std::ptr::{null, null_mut};
use std::time::Duration;

use crate::error::{Result, ShmError};
use crate::layout::shared_mapping_size;
use crate::ntapi::{
    duration_to_nt_timeout,
    // Functions
    NtClose,
    NtCreateEvent,
    NtCreateSection,
    NtMapViewOfSection,
    // Helpers
    NtName,
    NtOpenEvent,
    NtOpenProcess,
    NtOpenSection,
    NtSetEvent,
    NtUnmapViewOfSection,
    NtWaitForMultipleObjects,
    NtWaitForSingleObject,
    NullDaclSecurityDescriptor,
    EVENT_ALL_ACCESS,
    // Types
    CLIENT_ID,
    HANDLE,
    LARGE_INTEGER,
    NTSTATUS,
    NT_CURRENT_PROCESS,
    OBJECT_ATTRIBUTES,
    OBJ_CASE_INSENSITIVE,
    PAGE_READWRITE,
    PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE,
    PVOID,
    SECTION_ALL_ACCESS,
    SEC_COMMIT,
    // Constants
    STATUS_SUCCESS,
    STATUS_TIMEOUT,
    STATUS_WAIT_0,
    SYNCHRONIZATION_EVENT,
    UNICODE_STRING,
    VIEW_UNMAP,
    WAIT_ANY,
};

// ============================================================================
// Constants
// ============================================================================

const INVALID_HANDLE_VALUE: isize = -1;

// ============================================================================
// Handle wrapper
// ============================================================================

#[derive(Debug)]
pub struct Handle(HANDLE);

impl Handle {
    pub fn raw(&self) -> HANDLE {
        self.0
    }

    pub fn as_isize(&self) -> isize {
        self.0 as isize
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 as isize != INVALID_HANDLE_VALUE {
            unsafe {
                let _ = NtClose(self.0);
            }
        }
    }
}

// ============================================================================
// Helper functions
// ============================================================================

fn status_to_error(status: NTSTATUS, context: &'static str) -> ShmError {
    ShmError::WindowsError {
        code: status as u32,
        context,
    }
}

// ============================================================================
// EventHandle - NT Event через ntdll.dll
// ============================================================================

pub struct EventHandle {
    handle: Handle,
    _name: String,
}

unsafe impl Send for EventHandle {}
unsafe impl Sync for EventHandle {}

impl EventHandle {
    /// Создание события через NtCreateEvent с NULL DACL
    pub fn create(name: &str) -> Result<Self> {
        let mut nt_name = NtName::new(name)?;
        let mut sd = NullDaclSecurityDescriptor::new();
        let mut obj_attr =
            OBJECT_ATTRIBUTES::new(nt_name.as_ptr(), OBJ_CASE_INSENSITIVE, sd.as_ptr());

        let mut handle: HANDLE = null_mut();

        let status = unsafe {
            NtCreateEvent(
                &mut handle,
                EVENT_ALL_ACCESS,
                &mut obj_attr,
                SYNCHRONIZATION_EVENT,
                0, // InitialState = FALSE
            )
        };

        if status != STATUS_SUCCESS {
            return Err(status_to_error(status, "NtCreateEvent"));
        }

        Ok(EventHandle {
            handle: Handle(handle),
            _name: name.to_owned(),
        })
    }

    /// Открытие события через NtOpenEvent
    pub fn open(name: &str) -> Result<Self> {
        let mut nt_name = NtName::new(name)?;
        let mut obj_attr =
            OBJECT_ATTRIBUTES::new(nt_name.as_ptr(), OBJ_CASE_INSENSITIVE, null_mut());

        let mut handle: HANDLE = null_mut();

        let status = unsafe { NtOpenEvent(&mut handle, EVENT_ALL_ACCESS, &mut obj_attr) };

        if status != STATUS_SUCCESS {
            return Err(status_to_error(status, "NtOpenEvent"));
        }

        Ok(EventHandle {
            handle: Handle(handle),
            _name: name.to_owned(),
        })
    }

    /// Сигнализация через NtSetEvent
    pub fn set(&self) -> Result<()> {
        let mut previous_state: i32 = 0;
        let status = unsafe { NtSetEvent(self.handle.raw(), &mut previous_state) };

        if status != STATUS_SUCCESS {
            return Err(status_to_error(status, "NtSetEvent"));
        }
        Ok(())
    }

    /// Ожидание через NtWaitForSingleObject
    pub fn wait(&self, timeout: Option<Duration>) -> Result<bool> {
        let timeout_value: i64 = match timeout {
            Some(d) => duration_to_nt_timeout(d),
            None => 0,
        };

        let timeout_ptr = if timeout.is_some() {
            &timeout_value as *const i64
        } else {
            null()
        };

        let status = unsafe {
            NtWaitForSingleObject(
                self.handle.raw(),
                0, // Alertable = FALSE
                timeout_ptr,
            )
        };

        match status {
            STATUS_SUCCESS => Ok(true),
            STATUS_TIMEOUT => Ok(false),
            _ => Err(status_to_error(status, "NtWaitForSingleObject")),
        }
    }

    pub fn raw_handle(&self) -> isize {
        self.handle.as_isize()
    }
}

// ============================================================================
// Mapping - NT Section через ntdll.dll
// ============================================================================

#[derive(Debug)]
pub struct Mapping {
    _handle: Handle,
    view: *mut u8,
    _size: usize,
    _name: String,
}

unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Получить raw HANDLE секции (для передачи в kernel driver)
    pub fn section_handle(&self) -> isize {
        self._handle.as_isize()
    }

    /// Внутренний метод создания секции (общая логика для named и anonymous)
    fn create_internal(object_name: *mut UNICODE_STRING, name_for_storage: String) -> Result<Self> {
        let size = shared_mapping_size();
        let mut sd = NullDaclSecurityDescriptor::new();
        let mut obj_attr = OBJECT_ATTRIBUTES::new(object_name, OBJ_CASE_INSENSITIVE, sd.as_ptr());

        let mut section_handle: HANDLE = null_mut();
        let mut max_size = LARGE_INTEGER {
            QuadPart: size as i64,
        };

        let status = unsafe {
            NtCreateSection(
                &mut section_handle,
                SECTION_ALL_ACCESS,
                &mut obj_attr,
                &mut max_size,
                PAGE_READWRITE,
                SEC_COMMIT,
                null_mut(), // FileHandle = NULL
            )
        };

        if status != STATUS_SUCCESS {
            let context = if object_name.is_null() {
                "NtCreateSection (anonymous)"
            } else {
                "NtCreateSection"
            };
            return Err(status_to_error(status, context));
        }

        // Проверка: Handle не должен быть NULL
        if section_handle.is_null() {
            return Err(status_to_error(
                0xC0000008u32 as i32,
                "NtCreateSection returned NULL handle",
            ));
        }

        let handle = Handle(section_handle);

        // Map view
        let mut base_address: PVOID = null_mut();
        let mut view_size: usize = 0;

        let status = unsafe {
            NtMapViewOfSection(
                handle.raw(),
                NT_CURRENT_PROCESS,
                &mut base_address,
                0,
                0,
                null_mut(), // SectionOffset
                &mut view_size,
                VIEW_UNMAP,
                0,
                PAGE_READWRITE,
            )
        };

        if status != STATUS_SUCCESS {
            let context = if object_name.is_null() {
                "NtMapViewOfSection (anonymous)"
            } else {
                "NtMapViewOfSection"
            };
            return Err(status_to_error(status, context));
        }

        Ok(Mapping {
            _handle: handle,
            view: base_address as *mut u8,
            _size: size,
            _name: name_for_storage,
        })
    }

    /// Создание секции через NtCreateSection с NULL DACL
    pub fn create(name: &str) -> Result<Self> {
        let mut nt_name = NtName::new(name)?;
        Self::create_internal(nt_name.as_ptr(), name.to_owned())
    }

    /// Создание anonymous секции без имени (только через handle)
    ///
    /// Anonymous section не имеет имени в глобальном namespace и доступна
    /// только через handle. Идеально для передачи handle в kernel driver.
    ///
    /// **ВАЖНО:** Это НЕ то же самое, что `create("")` (пустая строка).
    ///
    /// **Технические детали:**
    /// - `create("")` → `NtName::new("")` → `to_nt_path("")` → `"\\BaseNamedObjects\\"`
    ///   → создается `UNICODE_STRING` с валидным указателем → **именованная секция**
    /// - `create_anonymous()` → передает `null_mut()` в `OBJECT_ATTRIBUTES.ObjectName`
    ///   → Windows NT API видит `ObjectName = NULL` → **anonymous section**
    ///
    /// Windows NT API интерпретирует только `ObjectName = NULL` в `OBJECT_ATTRIBUTES`
    /// как anonymous (unnamed) объект. Пустой `UNICODE_STRING` (даже с `Length = 0`)
    /// все равно является указателем на структуру, а не NULL, поэтому создаст
    /// именованную секцию (которая, вероятно, завершится ошибкой из-за невалидного имени).
    pub fn create_anonymous() -> Result<Self> {
        Self::create_internal(null_mut(), String::new())
    }

    /// Открытие секции через NtOpenSection
    pub fn open(name: &str) -> Result<Self> {
        let size = shared_mapping_size();
        let mut nt_name = NtName::new(name)?;
        let mut obj_attr =
            OBJECT_ATTRIBUTES::new(nt_name.as_ptr(), OBJ_CASE_INSENSITIVE, null_mut());

        let mut section_handle: HANDLE = null_mut();

        let status =
            unsafe { NtOpenSection(&mut section_handle, SECTION_ALL_ACCESS, &mut obj_attr) };

        if status != STATUS_SUCCESS {
            return Err(status_to_error(status, "NtOpenSection"));
        }

        let handle = Handle(section_handle);

        // Map view
        let mut base_address: PVOID = null_mut();
        let mut view_size: usize = 0;

        let status = unsafe {
            NtMapViewOfSection(
                handle.raw(),
                NT_CURRENT_PROCESS,
                &mut base_address,
                0,
                0,
                null_mut(),
                &mut view_size,
                VIEW_UNMAP,
                0,
                PAGE_READWRITE,
            )
        };

        if status != STATUS_SUCCESS {
            return Err(status_to_error(status, "NtMapViewOfSection"));
        }

        Ok(Mapping {
            _handle: handle,
            view: base_address as *mut u8,
            _size: size,
            _name: name.to_owned(),
        })
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.view
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        if !self.view.is_null() {
            unsafe {
                let _ = NtUnmapViewOfSection(NT_CURRENT_PROCESS, self.view as PVOID);
            }
            self.view = null_mut();
        }
    }
}

// ============================================================================
// wait_any - NtWaitForMultipleObjects
// ============================================================================

pub fn wait_any(handles: &[isize], timeout: Option<Duration>) -> Result<Option<usize>> {
    if handles.is_empty() {
        return Ok(None);
    }

    let timeout_value: i64 = match timeout {
        Some(d) => duration_to_nt_timeout(d),
        None => 0,
    };

    let timeout_ptr = if timeout.is_some() {
        &timeout_value as *const i64
    } else {
        null()
    };

    let status = unsafe {
        NtWaitForMultipleObjects(
            handles.len() as u32,
            handles.as_ptr() as *const HANDLE,
            WAIT_ANY,
            0, // Alertable = FALSE
            timeout_ptr,
        )
    };

    // STATUS_TIMEOUT (0x102) проверяем ДО диапазона валидных индексов: это
    // значение >= 0 и по чистой случайности совпало бы с индексом 258,
    // если бы handles.len() когда-нибудь превысил этот порог. Сейчас это
    // не достижимо (максимум 62 хендла из-за MAX_MULTI_CLIENTS = 31), но
    // порядок веток не должен полагаться на этот внешний инвариант.
    match status {
        STATUS_TIMEOUT => Ok(None),
        s if s >= 0 && (s as usize) < handles.len() => Ok(Some(s as usize)),
        _ => Err(status_to_error(status, "NtWaitForMultipleObjects")),
    }
}

// ============================================================================
// is_process_alive - liveness-проверка процесса по PID (NtOpenProcess)
// ============================================================================

/// Проверяет, жив ли процесс с данным PID.
///
/// Используется multi-client сервером для liveness-детекции connected-слотов,
/// чей клиент мог упасть ПОСЛЕ завершения handshake, не освободив claim (в
/// этом случае никаких событий от мёртвого процесса не придёт, и слот иначе
/// был бы потерян навсегда — см. `RESERVED_OWNER_PID_INDEX`).
///
/// Консервативна по конструкции: при любой двусмысленности (PID уже
/// переиспользован под другой процесс, недостаточно прав, иная ошибка NT)
/// возвращает `true` ("жив") — чтобы не отключить силой ещё легитимно
/// работающего клиента. Возвращает `false` ТОЛЬКО при однозначном
/// подтверждении: handle открыт и находится в сигнальном состоянии
/// (WaitForSingleObject с нулевым таймаутом вернул STATUS_WAIT_0 — процесс
/// завершился).
pub fn is_process_alive(pid: u32) -> bool {
    if pid == 0 {
        return true; // 0 не бывает PID пользовательского процесса
    }

    let mut client_id = CLIENT_ID {
        UniqueProcess: pid as usize as HANDLE,
        UniqueThread: null_mut(),
    };
    // ObjectName = NULL: процессы не именованные объекты BaseNamedObjects,
    // идентифицируются исключительно через ClientId.
    let mut obj_attr = OBJECT_ATTRIBUTES::new(null_mut(), 0, null_mut());
    let mut raw_handle: HANDLE = null_mut();

    let open_status = unsafe {
        NtOpenProcess(
            &mut raw_handle,
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            &mut obj_attr,
            &mut client_id,
        )
    };

    if open_status != STATUS_SUCCESS {
        // Не удалось открыть — PID переиспользован, отказано в доступе или
        // иная ошибка. Ни один из этих случаев не является подтверждением
        // смерти процесса-владельца claim, поэтому НЕ считаем его мёртвым.
        return true;
    }

    // RAII: handle закроется через NtClose при выходе из функции.
    let handle = Handle(raw_handle);

    let zero_timeout: i64 = 0; // мгновенный опрос, не блокируем worker
    let wait_status = unsafe { NtWaitForSingleObject(handle.raw(), 0, &zero_timeout) };

    match wait_status {
        // Объект-процесс сигнален => процесс завершился.
        STATUS_WAIT_0 => false,
        // Таймаут (объект не сигнален) => процесс всё ещё выполняется.
        STATUS_TIMEOUT => true,
        // Любой иной статус — двусмысленность, консервативно трактуем как "жив".
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_is_alive() {
        assert!(is_process_alive(std::process::id()));
    }

    #[test]
    fn pid_zero_is_conservatively_alive() {
        // 0 не бывает PID пользовательского процесса — не должен читаться
        // как "подтверждённо мёртв".
        assert!(is_process_alive(0));
    }

    /// Регрессия (аудит 2026-07-10, orphan-слот bug): для РЕАЛЬНО завершённого
    /// процесса `is_process_alive` обязана вернуть `false` — иначе
    /// liveness-детекция никогда не сработает.
    #[test]
    fn exited_process_is_detected_as_dead() {
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "exit", "0"])
            .spawn()
            .expect("spawn short-lived child process");
        let pid = child.id();
        child.wait().expect("wait for child exit");

        // Небольшой запас: на некоторых системах PID-объект остаётся
        // открываемым ещё короткое время после выхода, пока wait() полностью
        // не разрешит завершение с точки зрения ядра.
        for _ in 0..50 {
            if !is_process_alive(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("is_process_alive(pid={pid}) должен был вернуть false после child.wait()");
    }
}
