//! Platform layer - прямые вызовы NT API через ntdll.dll
//!
//! Использует статическую линковку с ntdll.dll.
//! Никаких внешних зависимостей, никакого TLS.

use std::ptr::{null, null_mut};
use std::time::Duration;

use crate::error::{Result, ShmError};
use crate::layout::shared_mapping_size;
use crate::ntapi::{
    // Types
    HANDLE, NTSTATUS, OBJECT_ATTRIBUTES, PVOID, LARGE_INTEGER,
    NullDaclSecurityDescriptor,
    // Constants
    STATUS_SUCCESS, STATUS_TIMEOUT,
    OBJ_CASE_INSENSITIVE,
    SECTION_ALL_ACCESS, PAGE_READWRITE, SEC_COMMIT, VIEW_UNMAP,
    EVENT_ALL_ACCESS, SYNCHRONIZATION_EVENT,
    WAIT_ANY,
    // Functions
    NtClose, NtCreateEvent, NtOpenEvent, NtSetEvent,
    NtWaitForSingleObject, NtWaitForMultipleObjects,
    NtCreateSection, NtOpenSection, NtMapViewOfSection, NtUnmapViewOfSection,
    NT_CURRENT_PROCESS,
    // Helpers
    NtName, duration_to_nt_timeout,
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
        let mut obj_attr = OBJECT_ATTRIBUTES::new(
            nt_name.as_ptr(),
            OBJ_CASE_INSENSITIVE,
            sd.as_ptr(),
        );

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
        let mut obj_attr = OBJECT_ATTRIBUTES::new(
            nt_name.as_ptr(),
            OBJ_CASE_INSENSITIVE,
            null_mut(),
        );

        let mut handle: HANDLE = null_mut();

        let status = unsafe {
            NtOpenEvent(
                &mut handle,
                EVENT_ALL_ACCESS,
                &mut obj_attr,
            )
        };

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
        let status = unsafe {
            NtSetEvent(self.handle.raw(), &mut previous_state)
        };

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
    /// Создание секции через NtCreateSection с NULL DACL
    pub fn create(name: &str) -> Result<Self> {
        let size = shared_mapping_size();
        let mut nt_name = NtName::new(name);
        let mut sd = NullDaclSecurityDescriptor::new();
        let mut obj_attr = OBJECT_ATTRIBUTES::new(
            nt_name.as_ptr(),
            OBJ_CASE_INSENSITIVE,
            sd.as_ptr(),
        );

        let mut section_handle: HANDLE = null_mut();
        let mut max_size = LARGE_INTEGER { QuadPart: size as i64 };

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
            return Err(status_to_error(status, "NtCreateSection"));
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
            return Err(status_to_error(status, "NtMapViewOfSection"));
        }

        Ok(Mapping {
            _handle: handle,
            view: base_address as *mut u8,
            _size: size,
            _name: name.to_owned(),
        })
    }

    /// Открытие секции через NtOpenSection
    pub fn open(name: &str) -> Result<Self> {
        let size = shared_mapping_size();
        let mut nt_name = NtName::new(name);
        let mut obj_attr = OBJECT_ATTRIBUTES::new(
            nt_name.as_ptr(),
            OBJ_CASE_INSENSITIVE,
            null_mut(),
        );

        let mut section_handle: HANDLE = null_mut();

        let status = unsafe {
            NtOpenSection(
                &mut section_handle,
                SECTION_ALL_ACCESS,
                &mut obj_attr,
            )
        };

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
