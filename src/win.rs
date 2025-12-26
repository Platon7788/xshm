//! Platform layer - NT syscalls через SSN
//!
//! Полностью использует SSN для всех операций с ядром Windows.
//! Никаких зависимостей от windows-sys/winapi.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr::{null, null_mut};
use std::time::Duration;

use crate::error::{Result, ShmError};
use crate::layout::shared_mapping_size;

// SSN imports
use ssn::hash::precomputed;
use ssn::syscall_direct;
use ssn::types::{
    security::NullDaclSecurityDescriptor,
    HANDLE, NTSTATUS, OBJECT_ATTRIBUTES, PVOID, STATUS_SUCCESS, UNICODE_STRING, ULONG,
    OBJ_CASE_INSENSITIVE, PAGE_READWRITE, SEC_COMMIT, SECTION_ALL_ACCESS, VIEW_UNMAP,
};

// ============================================================================
// Constants
// ============================================================================

const INVALID_HANDLE_VALUE: isize = -1;

// Event access rights
const EVENT_ALL_ACCESS: u32 = 0x1F0003;

// Event types
const SYNCHRONIZATION_EVENT: u32 = 0;

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
                let _ = syscall_direct!(precomputed::NTCLOSE, self.0);
            }
        }
    }
}

// ============================================================================
// Helper functions
// ============================================================================

fn to_wide(value: &str) -> Vec<u16> {
    OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Преобразование Win32 имени в NT путь
fn to_nt_path(name: &str) -> String {
    if let Some(stripped) = name.strip_prefix("Local\\") {
        format!("\\BaseNamedObjects\\{}", stripped)
    } else if let Some(stripped) = name.strip_prefix("Global\\") {
        format!("\\BaseNamedObjects\\{}", stripped)
    } else if name.starts_with('\\') {
        name.to_owned()
    } else {
        format!("\\BaseNamedObjects\\{}", name)
    }
}

/// Структура для хранения NT имени с валидным указателем
struct NtName {
    wide: Vec<u16>,
    unicode: UNICODE_STRING,
}

impl NtName {
    fn new(name: &str) -> Self {
        let nt_path = to_nt_path(name);
        let wide = to_wide(&nt_path);
        let byte_len = ((wide.len() - 1) * 2) as u16;
        
        let mut result = Self {
            wide,
            unicode: UNICODE_STRING {
                Length: byte_len,
                MaximumLength: byte_len + 2,
                Buffer: null_mut(), // Временно null
            },
        };
        
        // Теперь wide уже на своём месте, можно взять указатель
        result.unicode.Buffer = result.wide.as_ptr() as *mut u16;
        result
    }
    
    fn as_unicode_ptr(&mut self) -> *mut UNICODE_STRING {
        &mut self.unicode
    }
}

fn init_object_attributes(
    name: *mut UNICODE_STRING,
    attributes: ULONG,
    sd: PVOID,
) -> OBJECT_ATTRIBUTES {
    OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as ULONG,
        RootDirectory: null_mut(),
        ObjectName: name,
        Attributes: attributes,
        SecurityDescriptor: sd,
        SecurityQualityOfService: null_mut(),
    }
}

fn status_to_error(status: NTSTATUS, context: &'static str) -> ShmError {
    ShmError::WindowsError {
        code: status as u32,
        context,
    }
}

// ============================================================================
// EventHandle - NT Event через SSN
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
        let mut obj_attr = init_object_attributes(
            nt_name.as_unicode_ptr(),
            OBJ_CASE_INSENSITIVE,
            sd.as_ptr(),
        );

        let mut handle: HANDLE = null_mut();

        let status = unsafe {
            syscall_direct!(
                precomputed::NTCREATE_EVENT,
                &mut handle as *mut HANDLE,
                EVENT_ALL_ACCESS,
                &mut obj_attr as *mut OBJECT_ATTRIBUTES,
                SYNCHRONIZATION_EVENT,
                0u32
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
        let mut obj_attr = init_object_attributes(nt_name.as_unicode_ptr(), OBJ_CASE_INSENSITIVE, null_mut());

        let mut handle: HANDLE = null_mut();

        let status = unsafe {
            syscall_direct!(
                precomputed::NTOPEN_EVENT,
                &mut handle as *mut HANDLE,
                EVENT_ALL_ACCESS,
                &mut obj_attr as *mut OBJECT_ATTRIBUTES
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
            syscall_direct!(
                precomputed::NTSET_EVENT,
                self.handle.raw(),
                &mut previous_state as *mut i32
            )
        };

        if status != STATUS_SUCCESS {
            return Err(status_to_error(status, "NtSetEvent"));
        }
        Ok(())
    }

    /// Ожидание через NtWaitForSingleObject
    pub fn wait(&self, timeout: Option<Duration>) -> Result<bool> {
        let timeout_value: i64 = match timeout {
            Some(d) => -((d.as_nanos() / 100) as i64),
            None => 0,
        };

        let timeout_ptr = if timeout.is_some() {
            &timeout_value as *const i64
        } else {
            null()
        };

        // NtWaitForSingleObject(Handle, Alertable, Timeout)
        let status = unsafe {
            syscall_direct!(
                precomputed::NTWAIT_FOR_SINGLE_OBJECT,
                self.handle.raw() as usize,
                0usize,                    // Alertable = FALSE
                timeout_ptr as usize
            )
        };

        match status {
            STATUS_SUCCESS => Ok(true),
            0x00000102 => Ok(false), // STATUS_TIMEOUT
            _ => Err(status_to_error(status, "NtWaitForSingleObject")),
        }
    }

    pub fn raw_handle(&self) -> isize {
        self.handle.as_isize()
    }
}

// ============================================================================
// Mapping - NT Section через SSN
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
        let mut obj_attr = init_object_attributes(
            nt_name.as_unicode_ptr(),
            OBJ_CASE_INSENSITIVE,
            sd.as_ptr(),
        );

        let mut section_handle: HANDLE = null_mut();
        let mut max_size: i64 = size as i64;

        let status = unsafe {
            syscall_direct!(
                precomputed::NTCREATE_SECTION,
                &mut section_handle as *mut HANDLE,
                SECTION_ALL_ACCESS,
                &mut obj_attr as *mut OBJECT_ATTRIBUTES,
                &mut max_size as *mut i64,
                PAGE_READWRITE,
                SEC_COMMIT,
                0usize // FileHandle = NULL
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
            syscall_direct!(
                precomputed::NTMAP_VIEW_OF_SECTION,
                handle.raw(),
                -1isize as HANDLE,
                &mut base_address as *mut PVOID,
                0usize,
                0usize,
                null_mut::<i64>(),
                &mut view_size as *mut usize,
                VIEW_UNMAP,
                0u32,
                PAGE_READWRITE
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
        let mut obj_attr = init_object_attributes(nt_name.as_unicode_ptr(), OBJ_CASE_INSENSITIVE, null_mut());

        let mut section_handle: HANDLE = null_mut();

        let status = unsafe {
            syscall_direct!(
                precomputed::NTOPEN_SECTION,
                &mut section_handle as *mut HANDLE,
                SECTION_ALL_ACCESS,
                &mut obj_attr as *mut OBJECT_ATTRIBUTES
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
            syscall_direct!(
                precomputed::NTMAP_VIEW_OF_SECTION,
                handle.raw(),
                -1isize as HANDLE,
                &mut base_address as *mut PVOID,
                0usize,
                0usize,
                null_mut::<i64>(),
                &mut view_size as *mut usize,
                VIEW_UNMAP,
                0u32,
                PAGE_READWRITE
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
                let _ = syscall_direct!(
                    precomputed::NTUNMAP_VIEW_OF_SECTION,
                    -1isize as HANDLE,
                    self.view as PVOID
                );
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
        Some(d) => -((d.as_nanos() / 100) as i64),
        None => 0,
    };

    let timeout_ptr = if timeout.is_some() {
        &timeout_value as *const i64
    } else {
        null()
    };

    // NtWaitForMultipleObjects(Count, Handles, WaitType, Alertable, Timeout)
    // WaitType: 0 = WaitAll, 1 = WaitAny
    let status = unsafe {
        syscall_direct!(
            precomputed::NTWAIT_FOR_MULTIPLE_OBJECTS,
            handles.len() as usize,      // Count
            handles.as_ptr() as usize,   // Handles array
            1usize,                       // WaitType = WaitAny
            0usize,                       // Alertable = FALSE
            timeout_ptr as usize         // Timeout
        )
    };

    match status {
        s if s >= 0 && (s as usize) < handles.len() => Ok(Some(s as usize)),
        0x00000102 => Ok(None), // STATUS_TIMEOUT
        _ => Err(status_to_error(status, "NtWaitForMultipleObjects")),
    }
}
