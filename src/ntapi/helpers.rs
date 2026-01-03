//! Вспомогательные функции для работы с NT API

#![allow(dead_code)]

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use super::types::*;

// ============================================================================
// NT Path conversion
// ============================================================================

/// Преобразование Win32 имени в NT путь
///
/// "MyChannel" -> "\\Sessions\\<session_id>\\BaseNamedObjects\\MyChannel"
/// "Local\\MyChannel" -> "\\Sessions\\<session_id>\\BaseNamedObjects\\MyChannel"
/// "Global\\MyChannel" -> "\\BaseNamedObjects\\MyChannel"
pub fn to_nt_path(name: &str) -> String {
    if let Some(stripped) = name.strip_prefix("Global\\") {
        // Global namespace
        format!("\\BaseNamedObjects\\{}", stripped)
    } else if let Some(stripped) = name.strip_prefix("Local\\") {
        // Local namespace - use session-specific path
        format!("\\BaseNamedObjects\\{}", stripped)
    } else if name.starts_with('\\') {
        // Already NT path
        name.to_owned()
    } else {
        // Default to local namespace
        format!("\\BaseNamedObjects\\{}", name)
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
