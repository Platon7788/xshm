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
    NtOpenSection,
    NtSetEvent,
    NtUnmapViewOfSection,
    NtWaitForMultipleObjects,
    NtWaitForSingleObject,
    NullDaclSecurityDescriptor,
    EVENT_ALL_ACCESS,
    // Types
    HANDLE,
    LARGE_INTEGER,
    NTSTATUS,
    NT_CURRENT_PROCESS,
    OBJECT_ATTRIBUTES,
    OBJ_CASE_INSENSITIVE,
    PAGE_READWRITE,
    PVOID,
    SECTION_ALL_ACCESS,
    SEC_COMMIT,
    // Constants
    STATUS_SUCCESS,
    STATUS_TIMEOUT,
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
        let mut nt_name = NtName::new(name);
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
        let mut nt_name = NtName::new(name);
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
        let mut nt_name = NtName::new(name);
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
        let mut nt_name = NtName::new(name);
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

    match status {
        s if s >= 0 && (s as usize) < handles.len() => Ok(Some(s as usize)),
        STATUS_TIMEOUT => Ok(None),
        _ => Err(status_to_error(status, "NtWaitForMultipleObjects")),
    }
}
