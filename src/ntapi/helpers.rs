//! Вспомогательные функции для работы с NT API

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use super::funcs::{NT_CURRENT_PROCESS, NtQueryInformationProcess};
use super::types::*;
use crate::error::{Result, ShmError};

// ============================================================================
// NT Path conversion
// ============================================================================

/// Session id текущего процесса через NtQueryInformationProcess.
///
/// Находка 22: раньше ошибка запроса молча давала 0, и `Local\` имя
/// процесса из пользовательской сессии уезжало в ГЛОБАЛЬНЫЙ
/// `\BaseNamedObjects` -- мимо своего пира и навстречу чужим объектам.
/// Теперь ошибка возвращается вызывающему.
fn current_session_id() -> Result<u32> {
    let mut info = PROCESS_SESSION_INFORMATION { SessionId: 0 };
    let mut ret_len: ULONG = 0;
    // Размер структуры (4 байта) заведомо помещается в ULONG.
    let info_len = ULONG::try_from(size_of::<PROCESS_SESSION_INFORMATION>()).unwrap_or(4);
    // SAFETY: передаём валидный буфер нужного размера; NT_CURRENT_PROCESS = -1.
    let status = unsafe {
        NtQueryInformationProcess(
            NT_CURRENT_PROCESS,
            PROCESS_SESSION_INFORMATION_CLASS,
            (&raw mut info).cast(),
            info_len,
            &raw mut ret_len,
        )
    };
    if status == STATUS_SUCCESS {
        Ok(info.SessionId)
    } else {
        Err(ShmError::WindowsError {
            code: status.cast_unsigned(),
            context: "NtQueryInformationProcess(session)",
        })
    }
}

/// Построить session-local NT путь. В сессии 0 локальный namespace совпадает
/// с глобальным (\BaseNamedObjects) — как и в Win32. Сессию узнать не
/// удалось -- ошибка (не подмена глобальным namespace).
fn session_local_path(name: &str) -> Result<String> {
    let sid = current_session_id()?;
    Ok(if sid == 0 {
        format!("\\BaseNamedObjects\\{name}")
    } else {
        format!("\\Sessions\\{sid}\\BaseNamedObjects\\{name}")
    })
}

/// Преобразование Win32 имени в NT путь.
///
/// - `"Global\\X"`  -> `\BaseNamedObjects\X` (глобальный namespace; требует
///   привилегии SeCreateGlobalPrivilege при создании из сессии != 0)
/// - `"Local\\X"`   -> `\Sessions\<SessionId>\BaseNamedObjects\X` (изоляция по
///   сессии; в сессии 0 -> `\BaseNamedObjects\X`)
/// - `"X"`          -> session-local (как `Local\X`)
/// - `"\\..."`      -> уже NT путь, возвращается как есть
///
/// ВНИМАНИЕ (смена поведения относительно v<=0.3.0): раньше и `Local\`, и имена
/// без префикса уходили в ГЛОБАЛЬНЫЙ `\BaseNamedObjects`. Теперь они session-local.
/// Для IPC МЕЖДУ сессиями (например, служба в сессии 0 <-> процесс на десктопе)
/// обе стороны должны явно использовать префикс `Global\` (и иметь
/// SeCreateGlobalPrivilege для создания глобальных объектов).
///
/// `Err(WindowsError)` -- для session-local имени не удалось узнать сессию
/// процесса (находка 22).
pub fn to_nt_path(name: &str) -> Result<String> {
    if let Some(stripped) = name.strip_prefix("Global\\") {
        Ok(format!("\\BaseNamedObjects\\{stripped}"))
    } else if let Some(stripped) = name.strip_prefix("Local\\") {
        session_local_path(stripped)
    } else if name.starts_with('\\') {
        Ok(name.to_owned())
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

/// Максимальное число UTF-16 единиц (без null-терминатора) в имени, которое
/// ещё помещается в `UNICODE_STRING.Length`/`MaximumLength` (оба `u16`,
/// `MaximumLength = Length + 2`): `units*2 + 2 <= u16::MAX`.
const MAX_NT_NAME_UNITS: usize = (u16::MAX as usize - 2) / 2;

impl NtName {
    /// Создание NtName из строки.
    ///
    /// Автоматически конвертирует в NT путь и UTF-16. Возвращает ошибку,
    /// если итоговое имя (после конвертации в NT путь) не помещается в
    /// `UNICODE_STRING.Length` (u16) -- раньше это молча заворачивалось
    /// (`as u16`), давая NT укороченную длину при полностью записанном
    /// длинном буфере, т.е. рассинхронизацию Length/Buffer.
    /// `Err(WindowsError)` -- сессия процесса неизвестна (см. `to_nt_path`).
    pub fn new(name: &str) -> Result<Self> {
        let nt_path = to_nt_path(name)?;
        let wide = to_wide(&nt_path);
        let units = wide.len() - 1; // без null-терминатора

        if units > MAX_NT_NAME_UNITS {
            return Err(ShmError::InvalidConfig(
                "object name too long for UNICODE_STRING (max ~32766 UTF-16 units)",
            ));
        }

        let byte_len = (units * 2) as u16;

        let mut result = Self {
            wide,
            unicode: UNICODE_STRING {
                Length: byte_len,
                MaximumLength: byte_len + 2,
                Buffer: core::ptr::null_mut(),
            },
        };

        // Теперь wide на своём месте, можно взять указатель
        result.unicode.Buffer = result.wide.as_ptr().cast_mut();
        Ok(result)
    }

    /// Получить указатель на UNICODE_STRING для передачи в NT функции
    pub const fn as_ptr(&mut self) -> *mut UNICODE_STRING {
        &raw mut self.unicode
    }
}

// ============================================================================
// Timeout conversion
// ============================================================================

/// Конвертация Duration в NT timeout (100-наносекундные интервалы)
///
/// Возвращает отрицательное значение для относительного timeout.
///
/// Два граничных случая, которые нельзя обрабатывать наивным `as i64`:
/// - Экстремально большой `Duration` (`as_nanos()/100 > i64::MAX`) насыщаем
///   до `i64::MAX`, а не даём знаковому биту завернуться -- иначе итоговое
///   значение стало бы положительным, а NT трактует положительный timeout
///   как АБСОЛЮТНЫЙ момент времени (эпоха 1601 г.), а не относительный.
/// - Ненулевой, но короче одного кванта (100нс) `Duration` округляем вверх
///   до -1 (минимум один квант ожидания), а не до 0 -- 0 NT трактует как
///   «вернуться немедленно» (poll), что превратило бы ожидание в busy-poll.
///   Явный `Duration::ZERO` по-прежнему даёт 0 (осознанный poll вызывающего).
pub fn duration_to_nt_timeout(duration: std::time::Duration) -> i64 {
    let units_100ns = duration.as_nanos() / 100;
    if units_100ns == 0 {
        if duration.is_zero() { 0 } else { -1 }
    } else {
        -i64::try_from(units_100ns).unwrap_or(i64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Находка 22: сессия известна (ошибка больше не маскируется сессией 0)
    /// и совпадает с независимым повторным запросом.
    #[test]
    fn session_id_is_known() {
        let mut info = PROCESS_SESSION_INFORMATION {
            SessionId: u32::MAX,
        };
        let mut ret_len: ULONG = 0;
        // SAFETY: валидный буфер нужного размера; NT_CURRENT_PROCESS = -1.
        let status = unsafe {
            NtQueryInformationProcess(
                NT_CURRENT_PROCESS,
                PROCESS_SESSION_INFORMATION_CLASS,
                (&raw mut info).cast(),
                ULONG::try_from(size_of::<PROCESS_SESSION_INFORMATION>()).unwrap(),
                &raw mut ret_len,
            )
        };
        assert_eq!(status, STATUS_SUCCESS);
        assert_eq!(current_session_id().unwrap(), info.SessionId);
    }

    #[test]
    fn global_maps_to_base_named_objects() {
        assert_eq!(
            to_nt_path("Global\\Foo").unwrap(),
            "\\BaseNamedObjects\\Foo"
        );
    }

    #[test]
    fn raw_nt_path_unchanged() {
        assert_eq!(to_nt_path("\\Device\\Bar").unwrap(), "\\Device\\Bar");
    }

    #[test]
    fn local_is_session_scoped() {
        let p = to_nt_path("Local\\Foo").unwrap();
        // Либо сессия 0 (глобальный), либо session-local.
        assert!(
            p == "\\BaseNamedObjects\\Foo"
                || (p.starts_with("\\Sessions\\") && p.ends_with("\\BaseNamedObjects\\Foo")),
            "unexpected local path: {p}"
        );
    }

    #[test]
    fn no_prefix_is_session_scoped() {
        assert_eq!(
            to_nt_path("Foo").unwrap(),
            to_nt_path("Local\\Foo").unwrap()
        );
    }

    #[test]
    fn duration_zero_is_immediate_poll() {
        assert_eq!(duration_to_nt_timeout(std::time::Duration::ZERO), 0);
    }

    #[test]
    fn duration_shorter_than_one_quantum_rounds_up_not_down_to_zero() {
        // 50нс < 100нс кванта -- не должно превратиться в 0 (busy-poll).
        assert_eq!(
            duration_to_nt_timeout(std::time::Duration::from_nanos(50)),
            -1
        );
    }

    #[test]
    fn duration_normal_value_converts_exactly() {
        // 1мс = 10_000 * 100нс.
        assert_eq!(
            duration_to_nt_timeout(std::time::Duration::from_millis(1)),
            -10_000
        );
    }

    #[test]
    fn nt_name_rejects_pathologically_long_name() {
        let huge_name = "A".repeat(MAX_NT_NAME_UNITS + 100);
        match NtName::new(&huge_name) {
            Err(ShmError::InvalidConfig(_)) => {}
            _ => panic!("must reject overlong name with InvalidConfig"),
        }
    }

    #[test]
    fn nt_name_accepts_name_at_the_limit() {
        // to_nt_path добавляет префикс (`\Sessions\<sid>\BaseNamedObjects\` или
        // `\BaseNamedObjects\`), поэтому берём запас под него.
        let prefix_budget = 64;
        let name = "A".repeat(MAX_NT_NAME_UNITS - prefix_budget);
        assert!(NtName::new(&name).is_ok());
    }

    #[test]
    fn duration_extreme_saturates_instead_of_flipping_sign() {
        let huge = std::time::Duration::from_secs(u64::MAX);
        let result = duration_to_nt_timeout(huge);
        // Обязано остаться отрицательным (относительный timeout), не завернуться
        // в положительное значение (которое NT истолковал бы как абсолютное).
        assert!(result < 0, "must stay negative (relative), got {result}");
        assert_eq!(result, -(i64::MAX));
    }
}
