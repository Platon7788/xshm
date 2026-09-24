//! Platform layer - прямые вызовы NT API через ntdll.dll
//!
//! Использует статическую линковку с ntdll.dll.
//! Никаких внешних зависимостей, никакого TLS.
//!
//! ВАЖНО: Поддерживаются только x86 и x86_64 архитектуры!

// Compile-time проверка архитектуры
#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
compile_error!("xShm поддерживает только x86 и x86_64 архитектуры!");

use core::ffi::c_void;
use std::ptr::{null, null_mut};
use std::time::Duration;

use crate::error::{Result, ShmError};
use crate::layout::shared_mapping_size;
use crate::ntapi::{
    // Types
    CLIENT_ID,
    EVENT_ALL_ACCESS,
    HANDLE,
    LARGE_INTEGER,
    NOTIFICATION_EVENT,
    NT_CURRENT_PROCESS,
    NTSTATUS,
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
    NtResetEvent,
    NtSetEvent,
    NtUnmapViewOfSection,
    NtWaitForMultipleObjects,
    NtWaitForSingleObject,
    NullDaclSecurityDescriptor,
    OBJ_CASE_INSENSITIVE,
    OBJ_OPENIF,
    OBJECT_ATTRIBUTES,
    PAGE_READWRITE,
    PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE,
    PVOID,
    SEC_COMMIT,
    SECTION_ALL_ACCESS,
    STATUS_OBJECT_NAME_EXISTS,
    // Constants
    STATUS_SUCCESS,
    STATUS_TIMEOUT,
    STATUS_WAIT_0,
    SYNCHRONIZATION_EVENT,
    UNICODE_STRING,
    VIEW_UNMAP,
    WAIT_ANY,
    duration_to_nt_timeout,
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
    pub const fn raw(&self) -> HANDLE {
        self.0
    }

    /// Числовое значение дескриптора.
    ///
    /// `HANDLE` -- это `*mut c_void` по типу, но не адрес памяти: ядро кладёт туда
    /// индекс в таблице дескрипторов. Поэтому берём `addr()` (strict provenance),
    /// а не `as isize`: провенанс здесь нечего сохранять.
    pub fn as_isize(&self) -> isize {
        self.0.addr() as isize
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0.addr() as isize != INVALID_HANDLE_VALUE {
            // SAFETY: `Handle` -- единственный владелец дескриптора (конструируется
            // только из свежего handle, полученного от NT), поэтому закрываем ровно
            // один раз; null/INVALID отсеяны условием выше.
            unsafe {
                let _ = NtClose(self.0);
            }
        }
    }
}

// ============================================================================
// Helper functions
// ============================================================================

const fn status_to_error(status: NTSTATUS, context: &'static str) -> ShmError {
    ShmError::WindowsError {
        code: status as u32,
        context,
    }
}

// ============================================================================
// EventHandle - NT Event через ntdll.dll
// ============================================================================

#[derive(Debug)]
pub struct EventHandle {
    handle: Handle,
}

// SAFETY: внутри только NT-дескриптор (машинное слово). Все операции над ним
// (`NtSetEvent`/`NtWaitForSingleObject`) потокобезопасны на уровне ядра и не
// трогают состояние процесса, поэтому делить `EventHandle` между потоками
// безопасно.
unsafe impl Send for EventHandle {}
// SAFETY: см. выше -- у типа нет внутренней изменяемости на стороне Rust.
unsafe impl Sync for EventHandle {}

impl EventHandle {
    /// Создание события через NtCreateEvent с NULL DACL
    pub fn create(name: &str) -> Result<Self> {
        let mut nt_name = NtName::new(name)?;
        let mut sd = NullDaclSecurityDescriptor::new();
        let mut obj_attr =
            OBJECT_ATTRIBUTES::new(nt_name.as_ptr(), OBJ_CASE_INSENSITIVE, sd.as_ptr());

        let mut handle: HANDLE = null_mut();

        // SAFETY: `handle` -- валидный out-параметр; `obj_attr` живёт до конца
        // вызова и держит внутри `nt_name`/`sd`, которые тоже ещё живы.
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
        })
    }

    /// Событие-уведомление (ручной сброс): создаётся или, если уже есть,
    /// открывается (`OBJ_OPENIF`). NULL DACL — как у остальных объектов.
    pub fn open_or_create_notification(name: &str) -> Result<Self> {
        let mut nt_name = NtName::new(name)?;
        let mut sd = NullDaclSecurityDescriptor::new();
        let mut obj_attr = OBJECT_ATTRIBUTES::new(
            nt_name.as_ptr(),
            OBJ_CASE_INSENSITIVE | OBJ_OPENIF,
            sd.as_ptr(),
        );
        let mut handle: HANDLE = null_mut();
        // SAFETY: `handle` -- валидный out-параметр; `obj_attr` живёт до конца
        // вызова и держит внутри `nt_name`/`sd`, которые тоже ещё живы.
        let status = unsafe {
            NtCreateEvent(
                &mut handle,
                EVENT_ALL_ACCESS,
                &mut obj_attr,
                NOTIFICATION_EVENT,
                0, // InitialState = FALSE (у нового; существующее не трогаем)
            )
        };
        if status != STATUS_SUCCESS && status != STATUS_OBJECT_NAME_EXISTS {
            return Err(status_to_error(status, "NtCreateEvent(notification)"));
        }
        Ok(EventHandle {
            handle: Handle(handle),
        })
    }

    /// Сброс в несигнальное состояние через NtResetEvent
    pub fn reset(&self) -> Result<()> {
        let mut previous_state: i32 = 0;
        // SAFETY: дескриптор валиден, пока жив `self`; `previous_state` --
        // валидный out-параметр на стеке.
        let status = unsafe { NtResetEvent(self.handle.raw(), &mut previous_state) };
        if status != STATUS_SUCCESS {
            return Err(status_to_error(status, "NtResetEvent"));
        }
        Ok(())
    }

    /// Открытие события через NtOpenEvent
    pub fn open(name: &str) -> Result<Self> {
        let mut nt_name = NtName::new(name)?;
        let mut obj_attr =
            OBJECT_ATTRIBUTES::new(nt_name.as_ptr(), OBJ_CASE_INSENSITIVE, null_mut());

        let mut handle: HANDLE = null_mut();

        // SAFETY: `handle` -- валидный out-параметр; `obj_attr` (и `nt_name` внутри
        // него) живут до конца вызова.
        let status = unsafe { NtOpenEvent(&mut handle, EVENT_ALL_ACCESS, &mut obj_attr) };

        if status != STATUS_SUCCESS {
            return Err(status_to_error(status, "NtOpenEvent"));
        }

        Ok(EventHandle {
            handle: Handle(handle),
        })
    }

    /// Сигнализация через NtSetEvent
    pub fn set(&self) -> Result<()> {
        let mut previous_state: i32 = 0;
        // SAFETY: дескриптор валиден, пока жив `self`; `previous_state` -- валидный
        // out-параметр на стеке.
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

        // SAFETY: дескриптор валиден, пока жив `self`; `timeout_ptr` -- либо NULL
        // (бесконечное ожидание), либо указатель на `timeout_value`, живущий до
        // конца вызова.
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
    /// Фактический размер отображённого view (из `NtMapViewOfSection`).
    /// Проверяется против `shared_mapping_size()` при открытии чужой секции --
    /// см. `Mapping::open`.
    size: usize,
}

// SAFETY: внутри дескриптор секции и адрес отображения. Само отображение живёт,
// пока жив `Mapping`, и не привязано к потоку-создателю; синхронизация доступа к
// содержимому -- забота `RingBuffer`/`SharedView` (атомарные операции).
unsafe impl Send for Mapping {}
// SAFETY: см. выше.
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Получить raw HANDLE секции (для передачи в kernel driver)
    pub fn section_handle(&self) -> isize {
        self._handle.as_isize()
    }

    /// Внутренний метод создания секции (общая логика для named и anonymous)
    fn create_internal(object_name: *mut UNICODE_STRING) -> Result<Self> {
        let size = shared_mapping_size();
        let mut sd = NullDaclSecurityDescriptor::new();
        let mut obj_attr = OBJECT_ATTRIBUTES::new(object_name, OBJ_CASE_INSENSITIVE, sd.as_ptr());

        let mut section_handle: HANDLE = null_mut();
        let mut max_size = LARGE_INTEGER {
            QuadPart: size as i64,
        };

        // SAFETY: все переданные указатели -- на локальные переменные, живущие до
        // конца вызова; `object_name` либо NULL (anonymous), либо указывает на
        // `UNICODE_STRING`, который держит вызывающий (`Mapping::create`).
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

        // SAFETY: дескриптор секции валиден (только что создан); `base_address` и
        // `view_size` -- валидные out-параметры на стеке.
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
            view: base_address.cast::<u8>(),
            // Секцию создали мы сами ровно на `size` байт; NtMapViewOfSection
            // с ViewSize=0 отображает её целиком, но округляет до страницы --
            // берём максимум, чтобы size() не занижал доступный диапазон.
            size: view_size.max(size),
        })
    }

    /// Создание секции через NtCreateSection с NULL DACL
    pub fn create(name: &str) -> Result<Self> {
        let mut nt_name = NtName::new(name)?;
        Self::create_internal(nt_name.as_ptr())
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
        Self::create_internal(null_mut())
    }

    /// Открытие секции через NtOpenSection.
    ///
    /// Отображённый view ОБЯЗАН быть не меньше `shared_mapping_size()`: имя в
    /// `\BaseNamedObjects` может занять кто угодно (NULL DACL, namespace общий),
    /// и секция меньшего размера превратила бы штатный доступ к ring buffer'ам
    /// в чтение/запись за пределами отображения. Проверка размера -- граница
    /// доверия к чужой секции наравне с magic/version (аудит 2026-07-28).
    pub fn open(name: &str) -> Result<Self> {
        let required = shared_mapping_size();
        let mut nt_name = NtName::new(name)?;
        let mut obj_attr =
            OBJECT_ATTRIBUTES::new(nt_name.as_ptr(), OBJ_CASE_INSENSITIVE, null_mut());

        let mut section_handle: HANDLE = null_mut();

        // SAFETY: out-параметр на стеке; `obj_attr` и `nt_name` внутри него живы до
        // конца вызова.
        let status =
            unsafe { NtOpenSection(&mut section_handle, SECTION_ALL_ACCESS, &mut obj_attr) };

        if status != STATUS_SUCCESS {
            return Err(status_to_error(status, "NtOpenSection"));
        }

        let handle = Handle(section_handle);

        // Map view
        let mut base_address: PVOID = null_mut();
        let mut view_size: usize = 0;

        // SAFETY: дескриптор секции валиден (только что открыт); out-параметры --
        // локальные переменные, живущие до конца вызова.
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

        let mapping = Mapping {
            _handle: handle,
            view: base_address.cast::<u8>(),
            size: view_size,
        };

        // Проверка ПОСЛЕ конструирования Mapping: так view гарантированно
        // размапится через Drop, даже если секция окажется слишком мала.
        if mapping.size < required {
            return Err(ShmError::Corrupted);
        }

        Ok(mapping)
    }

    pub const fn as_ptr(&self) -> *mut u8 {
        self.view
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        if !self.view.is_null() {
            // SAFETY: `view` получен от `NtMapViewOfSection` в этом же процессе и
            // ещё не размаплен (после размапливания поле зануляется ниже), поэтому
            // отображение снимается ровно один раз.
            unsafe {
                let _ = NtUnmapViewOfSection(NT_CURRENT_PROCESS, self.view.cast::<c_void>());
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

    // SAFETY: `handles` -- непустой срез живых NT-дескрипторов (проверено выше),
    // его длина и указатель согласованы; `timeout_ptr` живёт до конца вызова.
    let status = unsafe {
        NtWaitForMultipleObjects(
            handles.len() as u32,
            handles.as_ptr().cast::<HANDLE>(),
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
// ProcessWatch - удерживаемый handle процесса-пира (детекция смерти)
// ============================================================================

/// Удерживаемый handle процесса-пира.
///
/// Открывается ОДИН раз при подключении (`NtOpenProcess` с
/// `SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION`) и живёт, пока живёт
/// соединение. Пока handle открыт, объект-процесс не уничтожается ядром даже
/// после смерти процесса -- он просто переходит в сигнальное состояние. Поэтому:
///
/// - смерть видна надёжно (в отличие от повторного `NtOpenProcess` по PID:
///   когда последний handle мёртвого процесса закрыт, открыть его уже нельзя,
///   и «не открылся» неотличим от «нет прав»);
/// - переиспользование PID после смерти не путает наблюдателя -- handle
///   ссылается на конкретный объект-процесс, а не на номер;
/// - handle можно положить в набор `wait_any` рядом с событиями канала и
///   узнать о смерти пира без опроса.
#[derive(Debug)]
pub struct ProcessWatch {
    handle: Handle,
    pid: u32,
}

// SAFETY: внутри только NT-дескриптор процесса (машинное слово) и PID. Все
// операции над дескриптором (`NtWaitForSingleObject`/`NtWaitForMultipleObjects`
// с нулевым/любым таймаутом, `NtClose` в Drop) потокобезопасны на уровне ядра,
// у типа нет внутренней изменяемости на стороне Rust -- как у `EventHandle`.
// Без этого `SharedServer`/`SharedClient` молча потеряли бы `Sync`, которым
// обладали до 0.8 (ломающее изменение API).
unsafe impl Send for ProcessWatch {}
// SAFETY: см. выше.
unsafe impl Sync for ProcessWatch {}

impl ProcessWatch {
    /// Открыть процесс `pid` для наблюдения. `None` -- PID неизвестен (0),
    /// процесса уже нет, либо не хватает прав (например, пир -- служба в
    /// другой сессии); вызывающий тогда работает без детекции смерти.
    pub fn open(pid: u32) -> Option<Self> {
        if pid == 0 {
            return None; // 0 -- «PID не передан» (пир старой версии)
        }

        let mut client_id = CLIENT_ID {
            // PID -- не адрес: собираем «указателеподобное» значение без провенанса
            // (`without_provenance_mut`), а не через `as`-каста целого в указатель.
            UniqueProcess: core::ptr::without_provenance_mut(pid as usize),
            UniqueThread: null_mut(),
        };
        // ObjectName = NULL: процессы не именованные объекты BaseNamedObjects,
        // идентифицируются исключительно через ClientId.
        let mut obj_attr = OBJECT_ATTRIBUTES::new(null_mut(), 0, null_mut());
        let mut raw_handle: HANDLE = null_mut();

        // SAFETY: out-параметр и `obj_attr`/`client_id` -- локальные переменные,
        // живущие до конца вызова; ObjectName внутри `obj_attr` намеренно NULL.
        let open_status = unsafe {
            NtOpenProcess(
                &mut raw_handle,
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                &mut obj_attr,
                &mut client_id,
            )
        };
        if open_status != STATUS_SUCCESS || raw_handle.is_null() {
            return None;
        }
        Some(Self {
            handle: Handle(raw_handle),
            pid,
        })
    }

    /// Открыть наблюдение за пиром, чей PID пришёл в handshake. Процесс,
    /// который уже мёртв в момент подключения, НЕ наблюдается (`None`, а не
    /// «сразу умер»): живой пир не мог бы прислать HELLO из мёртвого процесса,
    /// значит PID устаревший (например, остался от упавшего посреди handshake
    /// клиента), и честный ответ -- «неизвестно».
    pub fn open_peer(pid: u32) -> Option<Self> {
        Self::open(pid).filter(|watch| !watch.has_exited())
    }

    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// `true` -- процесс подтверждённо завершился (объект-процесс сигнален).
    /// Любая неоднозначность (ошибка ожидания) трактуется как «жив».
    pub fn has_exited(&self) -> bool {
        let zero_timeout: i64 = 0; // мгновенный опрос, без блокировки
        // SAFETY: дескриптор процесса валиден (открыт в `open`, закрывается
        // через Drop `Handle`); `zero_timeout` живёт до конца вызова.
        let wait_status = unsafe { NtWaitForSingleObject(self.handle.raw(), 0, &zero_timeout) };
        wait_status == STATUS_WAIT_0
    }

    /// Числовой handle для набора `wait_any` (сигналится при смерти процесса).
    pub fn raw_handle(&self) -> isize {
        self.handle.as_isize()
    }
}

/// Проверяет, жив ли процесс с данным PID (разовая проверка без удержания).
///
/// Используется multi-client сервером как запасной путь, когда у слота нет
/// удерживаемого `ProcessWatch` (клиент старой версии не передал PID в
/// handshake) -- см. `RESERVED_OWNER_PID_INDEX`.
///
/// Консервативна по конструкции: при любой двусмысленности (процесс не
/// открылся -- PID переиспользован, нет прав, ИЛИ процесс уже полностью
/// удалён ядром, потому что никто не держал его handle) возвращает `true`
/// ("жив"). Возвращает `false` ТОЛЬКО если handle открылся и процесс
/// сигнален. Для надёжной детекции смерти держите `ProcessWatch` с момента
/// подключения.
pub fn is_process_alive(pid: u32) -> bool {
    if pid == 0 {
        return true; // 0 не бывает PID пользовательского процесса
    }
    ProcessWatch::open(pid).is_none_or(|watch| !watch.has_exited())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_is_alive() {
        assert!(is_process_alive(std::process::id()));
    }

    /// Регрессия (аудит 2026-07-28): `Mapping::open` доверял чужой секции и
    /// не сверял её размер. Любой процесс может первым занять имя в
    /// `\BaseNamedObjects` (NULL DACL, общий namespace) и создать секцию
    /// меньше `shared_mapping_size()` -- дальнейший штатный доступ к ring
    /// buffer'ам ушёл бы ЗА пределы отображения.
    #[test]
    fn open_rejects_undersized_section() {
        let name = format!("XSHM_TEST_SMALL_SECTION_{}", std::process::id());

        let mut nt_name = NtName::new(&name).expect("nt name");
        let mut sd = NullDaclSecurityDescriptor::new();
        let mut obj_attr =
            OBJECT_ATTRIBUTES::new(nt_name.as_ptr(), OBJ_CASE_INSENSITIVE, sd.as_ptr());
        let mut handle: HANDLE = null_mut();
        let mut max_size = LARGE_INTEGER { QuadPart: 4096 };

        // SAFETY: все указатели валидны и живут до конца вызова.
        let status = unsafe {
            NtCreateSection(
                &mut handle,
                SECTION_ALL_ACCESS,
                &mut obj_attr,
                &mut max_size,
                PAGE_READWRITE,
                SEC_COMMIT,
                null_mut(),
            )
        };
        assert_eq!(status, STATUS_SUCCESS, "не удалось создать тестовую секцию");
        let _owner = Handle(handle); // держим секцию живой + RAII-закрытие

        match Mapping::open(&name) {
            Err(ShmError::Corrupted) => {}
            other => panic!("секция 4 КБ должна отвергаться, получено: {other:?}"),
        }
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
