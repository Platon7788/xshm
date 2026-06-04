//! Вспомогательные функции для работы с NT API

#![allow(dead_code)]

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use super::funcs::{NtQueryInformationProcess, NT_CURRENT_PROCESS};
use super::types::*;

// ============================================================================
// NT Path conversion
// ============================================================================

/// Session id текущего процесса через NtQueryInformationProcess.
/// При ошибке возвращает 0 (трактуется как глобальный namespace, безопасный
/// дефолт для служб/сессии 0).
fn current_session_id() -> u32 {
    let mut info = PROCESS_SESSION_INFORMATION { SessionId: 0 };
    let mut ret_len: ULONG = 0;
    // SAFETY: передаём валидный буфер нужного размера; NT_CURRENT_PROCESS = -1.
    let status = unsafe {
        NtQueryInformationProcess(
            NT_CURRENT_PROCESS,
            PROCESS_SESSION_INFORMATION_CLASS,
            &mut info as *mut _ as PVOID,
            core::mem::size_of::<PROCESS_SESSION_INFORMATION>() as ULONG,
            &mut ret_len,
        )
    };
    if status == STATUS_SUCCESS {
        info.SessionId
    } else {
        0
    }
}

/// Построить session-local NT путь. В сессии 0 локальный namespace совпадает
/// с глобальным (\BaseNamedObjects) — как и в Win32.
fn session_local_path(name: &str) -> String {
    let sid = current_session_id();
    if sid == 0 {
        format!("\\BaseNamedObjects\\{name}")
    } else {
        format!("\\Sessions\\{sid}\\BaseNamedObjects\\{name}")
    }
}

/// Преобразование Win32 имени в NT путь.
///
/// - `"Global\\X"`  -> `\BaseNamedObjects\X` (глобальный namespace; требует
///   привилегии SeCreateGlobalPrivilege при создании из сессии != 0)
/// - `"Local\\X"`   -> `\Sessions\<SessionId>\BaseNamedObjects\X` (изоляция по
///   сессии; в сессии 0 -> `\BaseNamedObjects\X`)
/// - `"X"`          -> session-local (как `Local\X`)
/// - `"\\..."`      -> уже NT путь, возвращается как есть
pub fn to_nt_path(name: &str) -> String {
    if let Some(stripped) = name.strip_prefix("Global\\") {
        format!("\\BaseNamedObjects\\{stripped}")
    } else if let Some(stripped) = name.strip_prefix("Local\\") {
        session_local_path(stripped)
    } else if name.starts_with('\\') {
        name.to_owned()
    } else {
        session_local_path(name)
    }
}

/// Конвертация строки в wide (UTF-16) с null-терминатором
pub fn to_wide(value: &str) -> Vec<u16> {
    OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

// ============================================================================
// NtName - безопасная обёртка для UNICODE_STRING
// ============================================================================

/// Структура для хранения NT имени с валидным указателем
///
/// Гарантирует что wide буфер живёт пока жива структура,
/// и UNICODE_STRING.Buffer указывает на валидную память.
pub struct NtName {
    wide: Vec<u16>,
    unicode: UNICODE_STRING,
}

impl NtName {
    /// Создание NtName из строки
    ///
    /// Автоматически конвертирует в NT путь и UTF-16
    pub fn new(name: &str) -> Self {
        let nt_path = to_nt_path(name);
        let wide = to_wide(&nt_path);
        let byte_len = ((wide.len() - 1) * 2) as u16; // без null-терминатора

        let mut result = Self {
            wide,
            unicode: UNICODE_STRING {
                Length: byte_len,
                MaximumLength: byte_len + 2,
                Buffer: core::ptr::null_mut(),
            },
        };

        // Теперь wide на своём месте, можно взять указатель
        result.unicode.Buffer = result.wide.as_ptr() as *mut u16;
        result
    }

    /// Получить указатель на UNICODE_STRING для передачи в NT функции
    pub fn as_ptr(&mut self) -> *mut UNICODE_STRING {
        &mut self.unicode
    }
}

// ============================================================================
// NTSTATUS helpers
// ============================================================================

/// Проверка успешности NTSTATUS
#[inline]
pub fn nt_success(status: NTSTATUS) -> bool {
    status >= 0
}

/// Проверка на timeout
#[inline]
pub fn is_timeout(status: NTSTATUS) -> bool {
    status == STATUS_TIMEOUT
}

// ============================================================================
// Timeout conversion
// ============================================================================

/// Конвертация Duration в NT timeout (100-наносекундные интервалы)
///
/// Возвращает отрицательное значение для относительного timeout
pub fn duration_to_nt_timeout(duration: std::time::Duration) -> i64 {
    -((duration.as_nanos() / 100) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_maps_to_base_named_objects() {
        assert_eq!(to_nt_path("Global\\Foo"), "\\BaseNamedObjects\\Foo");
    }

    #[test]
    fn raw_nt_path_unchanged() {
        assert_eq!(to_nt_path("\\Device\\Bar"), "\\Device\\Bar");
    }

    #[test]
    fn local_is_session_scoped() {
        let p = to_nt_path("Local\\Foo");
        // Либо сессия 0 (глобальный), либо session-local.
        assert!(
            p == "\\BaseNamedObjects\\Foo"
                || (p.starts_with("\\Sessions\\") && p.ends_with("\\BaseNamedObjects\\Foo")),
            "unexpected local path: {p}"
        );
    }

    #[test]
    fn no_prefix_is_session_scoped() {
        assert_eq!(to_nt_path("Foo"), to_nt_path("Local\\Foo"));
    }
}
