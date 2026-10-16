//! Platform layer - прямые вызовы NT API через ntdll.dll
//!
//! Использует статическую линковку с ntdll.dll.
//! Никаких внешних зависимостей, никакого TLS.
//!
//! ВАЖНО: Поддерживаются только x86 и x86_64 архитектуры!

// Compile-time проверка архитектуры
#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
compile_error!("xShm поддерживает только x86 и x86_64 архитектуры!");
#[cfg(not(windows))]
compile_error!("xShm поддерживает только Windows (прямые вызовы NT API)!");

use core::ffi::c_void;
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, RawHandle};
use std::ptr::{null, null_mut};
use std::time::Duration;

use crate::error::{Result, ShmError};
use crate::ntapi::{
    // Types
    CLIENT_ID,
    EVENT_ALL_ACCESS,
    EVENT_BASIC_INFORMATION,
    EVENT_BASIC_INFORMATION_CLASS,
    HANDLE,
    KERNEL_USER_TIMES,
    LARGE_INTEGER,
    MUTANT_ALL_ACCESS,
    NOTIFICATION_EVENT,
    NT_CURRENT_PROCESS,
    NTSTATUS,
    // Functions
    NtClose,
    NtCreateEvent,
    NtCreateMutant,
    NtCreateSection,
    NtMapViewOfSection,
    // Helpers
    NtName,
    NtOpenEvent,
    NtOpenProcess,
    NtOpenSection,
    NtQueryEvent,
    NtQueryInformationProcess,
    NtQuerySection,
    NtReleaseMutant,
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
    ProcessTimes,
    SEC_COMMIT,
    SEC_FILE,
    SEC_IMAGE,
    SEC_RESERVE,
    SECTION_ALL_ACCESS,
    SECTION_BASIC_INFORMATION,
    STATUS_ABANDONED_WAIT_0,
    STATUS_INVALID_CID,
    STATUS_INVALID_PARAMETER,
    STATUS_OBJECT_NAME_EXISTS,
    STATUS_OBJECT_TYPE_MISMATCH,
    // Constants
    STATUS_SUCCESS,
    STATUS_TIMEOUT,
    STATUS_WAIT_0,
    SYNCHRONIZATION_EVENT,
    SectionBasicInformation,
    ULONG,
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
        self.0.addr().cast_signed()
    }
}

impl AsRawHandle for Handle {
    fn as_raw_handle(&self) -> RawHandle {
        self.0
    }
}

impl AsHandle for Handle {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        // SAFETY: `Handle` строится только из дескриптора, который вернул
        // успешный NT-вызов (создание/открытие объекта), владеет им и
        // закрывает только в `Drop`; заимствование не переживает `&self`,
        // значит дескриптор открыт всё время жизни `BorrowedHandle`.
        unsafe { BorrowedHandle::borrow_raw(self.0) }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0.addr().cast_signed() != INVALID_HANDLE_VALUE {
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
        code: status.cast_unsigned(),
        context,
    }
}

// ============================================================================
// EventHandle - NT Event через ntdll.dll
// ============================================================================

#[derive(Debug)]
#[expect(
    unnameable_types,
    reason = "тип публичных полей `SharedEvents` (`SharedServer::events`): конструкторы (NULL DACL) не входят в публичный API"
)]
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

impl AsHandle for EventHandle {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.handle.as_handle()
    }
}

impl AsRawHandle for EventHandle {
    fn as_raw_handle(&self) -> RawHandle {
        self.handle.as_raw_handle()
    }
}

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
                &raw mut handle,
                EVENT_ALL_ACCESS,
                &raw mut obj_attr,
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
                &raw mut handle,
                EVENT_ALL_ACCESS,
                &raw mut obj_attr,
                NOTIFICATION_EVENT,
                0, // InitialState = FALSE (у нового; существующее не трогаем)
            )
        };
        if status != STATUS_SUCCESS && status != STATUS_OBJECT_NAME_EXISTS {
            return Err(status_to_error(status, "NtCreateEvent(notification)"));
        }
        let event = EventHandle {
            handle: Handle(handle),
        };
        // Существующее событие открыто «как есть» -- любого типа. Автосбросное
        // событие под именем маяка ломает всех (взвод будит одного ждущего,
        // `is_raised` начинает сбрасывать маяк), поэтому подмену отвергаем.
        // Ограничивающий DACL отсекается раньше: `EVENT_ALL_ACCESS` не выдан.
        if status == STATUS_OBJECT_NAME_EXISTS && event.event_type()? != NOTIFICATION_EVENT {
            return Err(status_to_error(
                STATUS_OBJECT_TYPE_MISMATCH,
                "existing event is not a NotificationEvent",
            ));
        }
        Ok(event)
    }

    /// Тип события (`NOTIFICATION_EVENT`/`SYNCHRONIZATION_EVENT`) через
    /// `NtQueryEvent(EventBasicInformation)`.
    pub fn event_type(&self) -> Result<u32> {
        let mut info = EVENT_BASIC_INFORMATION::default();
        let mut returned: u32 = 0;
        // SAFETY: дескриптор валиден, пока жив `self`; `info` -- `repr(C)`
        // буфер ровно того размера, что передан; `returned` -- out-параметр
        // на стеке. Всё живёт до конца вызова.
        let status = unsafe {
            NtQueryEvent(
                self.handle.raw(),
                EVENT_BASIC_INFORMATION_CLASS,
                (&raw mut info).cast::<c_void>(),
                size_of::<EVENT_BASIC_INFORMATION>() as u32,
                &raw mut returned,
            )
        };
        if status != STATUS_SUCCESS {
            return Err(status_to_error(status, "NtQueryEvent"));
        }
        Ok(info.EventType)
    }

    /// Безымянное событие (`ObjectAttributes = NULL`): не попадает в
    /// `\BaseNamedObjects`, открыть его по имени из другого процесса нельзя —
    /// ни угадать, ни занять заранее. Для сигналов внутри процесса (остановка
    /// потока, «в очереди есть команда»). `notification` — ручной сброс
    /// (будит всех и остаётся взведённым), иначе автосброс (будит одного
    /// ждущего и сбрасывается).
    pub fn create_unnamed(notification: bool) -> Result<Self> {
        let mut handle: HANDLE = null_mut();
        let kind = if notification {
            NOTIFICATION_EVENT
        } else {
            SYNCHRONIZATION_EVENT
        };
        // SAFETY: `handle` -- валидный out-параметр на стеке; ObjectAttributes
        // намеренно NULL -- NtCreateEvent это допускает и создаёт безымянный
        // объект с дескриптором безопасности по умолчанию (токен процесса).
        let status = unsafe {
            NtCreateEvent(
                &raw mut handle,
                EVENT_ALL_ACCESS,
                null_mut(),
                kind,
                0, // InitialState = FALSE
            )
        };
        if status != STATUS_SUCCESS || handle.is_null() {
            return Err(status_to_error(status, "NtCreateEvent(unnamed)"));
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
        let status = unsafe { NtResetEvent(self.handle.raw(), &raw mut previous_state) };
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
        let status = unsafe { NtOpenEvent(&raw mut handle, EVENT_ALL_ACCESS, &raw mut obj_attr) };

        if status != STATUS_SUCCESS {
            return Err(status_to_error(status, "NtOpenEvent"));
        }

        Ok(EventHandle {
            handle: Handle(handle),
        })
    }

    /// Открытие чужого события канала с проверкой типа: все события канала
    /// -- автосбросные (`SYNCHRONIZATION_EVENT`). Событие с ручным сбросом,
    /// заранее созданное под тем же именем чужим процессом (NULL DACL,
    /// общий namespace), не гасилось бы ожиданием и гоняло бы worker на
    /// 100% CPU -- `Corrupted` (аудит 2026-10-16).
    pub fn open_auto_reset(name: &str) -> Result<Self> {
        let event = Self::open(name)?;
        if event.event_type()? != SYNCHRONIZATION_EVENT {
            return Err(ShmError::Corrupted);
        }
        Ok(event)
    }

    /// Сигнализация через NtSetEvent
    pub fn set(&self) -> Result<()> {
        let mut previous_state: i32 = 0;
        // SAFETY: дескриптор валиден, пока жив `self`; `previous_state` -- валидный
        // out-параметр на стеке.
        let status = unsafe { NtSetEvent(self.handle.raw(), &raw mut previous_state) };

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
            &raw const timeout_value
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
// NamedMutex - NT Mutant (сериализация клиентов лобби, 0.9)
// ============================================================================

/// Именованный мьютекс ядра (Mutant): create-or-open (`OBJ_OPENIF`), NULL DACL.
///
/// Владение мьютексом у Windows -- по ПОТОКУ: освобождать обязан тот же поток,
/// что захватил. Поэтому guard не `Send` (см. `NamedMutexGuard`). Если
/// поток-владелец завершился, не освободив мьютекс (в т.ч. процесс убит),
/// следующий ждущий получает его «брошенным» (`STATUS_ABANDONED_WAIT_0`) --
/// это тоже успешный захват.
#[derive(Debug)]
pub struct NamedMutex {
    handle: Handle,
}

// SAFETY: внутри только NT-дескриптор; ожидание/освобождение потокобезопасны
// на уровне ядра. Привязку владения к потоку соблюдает `NamedMutexGuard`
// (он не `Send`), сам дескриптор делить между потоками можно.
unsafe impl Send for NamedMutex {}
// SAFETY: см. выше.
unsafe impl Sync for NamedMutex {}

impl AsHandle for NamedMutex {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.handle.as_handle()
    }
}

impl AsRawHandle for NamedMutex {
    fn as_raw_handle(&self) -> RawHandle {
        self.handle.as_raw_handle()
    }
}

/// Захваченный мьютекс; освобождается в Drop тем же потоком.
#[derive(Debug)]
pub struct NamedMutexGuard<'a> {
    mutex: &'a NamedMutex,
    /// Захват достался «брошенным»: прежний владелец умер посреди работы
    /// (лобби Dispatch относится к этому одинаково -- сервер сам заметит
    /// смерть по handle процесса; флаг нужен тестам).
    #[cfg_attr(not(test), expect(dead_code, reason = "читается только тестами"))]
    abandoned: bool,
    /// Не `Send`: освобождать обязан поток-владелец.
    _not_send: std::marker::PhantomData<*const ()>,
}

impl NamedMutex {
    /// Создать мьютекс `name` или открыть существующий. Имя, занятое объектом
    /// другого типа, -- ошибка (`STATUS_OBJECT_TYPE_MISMATCH`).
    pub fn open_or_create(name: &str) -> Result<Self> {
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
            NtCreateMutant(
                &raw mut handle,
                MUTANT_ALL_ACCESS,
                &raw mut obj_attr,
                0, // InitialOwner = FALSE
            )
        };
        if status != STATUS_SUCCESS && status != STATUS_OBJECT_NAME_EXISTS {
            return Err(status_to_error(status, "NtCreateMutant"));
        }
        Ok(Self {
            handle: Handle(handle),
        })
    }

    /// Захватить, ожидая не дольше `timeout`. `Ok(None)` -- таймаут.
    pub fn lock(&self, timeout: Option<Duration>) -> Result<Option<NamedMutexGuard<'_>>> {
        let timeout_value: i64 = timeout.map_or(0, duration_to_nt_timeout);
        let timeout_ptr = if timeout.is_some() {
            &raw const timeout_value
        } else {
            null()
        };
        // SAFETY: дескриптор валиден, пока жив `self`; `timeout_ptr` -- NULL
        // либо указатель на `timeout_value`, живущий до конца вызова.
        let status = unsafe { NtWaitForSingleObject(self.handle.raw(), 0, timeout_ptr) };
        let abandoned = match status {
            STATUS_SUCCESS => false,
            STATUS_ABANDONED_WAIT_0 => true,
            STATUS_TIMEOUT => return Ok(None),
            _ => return Err(status_to_error(status, "NtWaitForSingleObject(mutant)")),
        };
        Ok(Some(NamedMutexGuard {
            mutex: self,
            abandoned,
            _not_send: std::marker::PhantomData,
        }))
    }
}

impl NamedMutexGuard<'_> {
    /// Прежний владелец умер, не освободив мьютекс.
    #[cfg(test)]
    pub const fn was_abandoned(&self) -> bool {
        self.abandoned
    }
}

impl Drop for NamedMutexGuard<'_> {
    fn drop(&mut self) {
        let mut previous: i32 = 0;
        // SAFETY: мьютекс захвачен этим потоком (guard не `Send`), дескриптор
        // жив, пока жив `NamedMutex`, на который ссылается guard.
        unsafe {
            let _ = NtReleaseMutant(self.mutex.handle.raw(), &raw mut previous);
        }
    }
}

// ============================================================================
// Mapping - NT Section через ntdll.dll
// ============================================================================

#[derive(Debug)]
pub struct Mapping {
    _handle: Handle,
    view: *mut u8,
    /// Фактический размер отображённого view (из `NtMapViewOfSection`,
    /// округлён до страницы). Граница, против которой `SharedView::attach`
    /// проверяет раскладку чужого сегмента.
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
    fn create_internal(object_name: *mut UNICODE_STRING, size: usize) -> Result<Self> {
        let mut sd = NullDaclSecurityDescriptor::new();
        let mut obj_attr = OBJECT_ATTRIBUTES::new(object_name, OBJ_CASE_INSENSITIVE, sd.as_ptr());

        let mut section_handle: HANDLE = null_mut();
        // Размер секции -- `LARGE_INTEGER` (знаковый): больше `i64::MAX` не
        // бывает ни одной раскладки, но молча завернуть его в минус нельзя.
        let mut max_size = LARGE_INTEGER {
            QuadPart: i64::try_from(size)
                .map_err(|_| ShmError::InvalidConfig("section size exceeds i64::MAX"))?,
        };

        // SAFETY: все переданные указатели -- на локальные переменные, живущие до
        // конца вызова; `object_name` либо NULL (anonymous), либо указывает на
        // `UNICODE_STRING`, который держит вызывающий (`Mapping::create`).
        let status = unsafe {
            NtCreateSection(
                &raw mut section_handle,
                SECTION_ALL_ACCESS,
                &raw mut obj_attr,
                &raw mut max_size,
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
                0xC000_0008_u32.cast_signed(), // STATUS_INVALID_HANDLE
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
                &raw mut base_address,
                0,
                0,
                null_mut(), // SectionOffset
                &raw mut view_size,
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

    /// Создание секции размером `size` байт через NtCreateSection с NULL DACL
    pub fn create(name: &str, size: usize) -> Result<Self> {
        let mut nt_name = NtName::new(name)?;
        Self::create_internal(nt_name.as_ptr(), size)
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
    pub fn create_anonymous(size: usize) -> Result<Self> {
        Self::create_internal(null_mut(), size)
    }

    /// Открытие чужой секции целиком.
    ///
    /// Размер view (= размер секции, округлённый до страницы) -- граница
    /// доверия: имя в `\BaseNamedObjects` может занять кто угодно (NULL DACL,
    /// namespace общий), поэтому раскладку, прочитанную из сегмента,
    /// `SharedView::attach` сверяет именно с ним, прежде чем трогать кольца.
    pub fn open(name: &str) -> Result<Self> {
        Self::open_view(name, 0)
    }

    /// Открытие чужой секции с отображением только заголовка сегмента
    /// (первая страница): перебор слотов Multi не отображает кольца целиком
    /// ради чтения `ControlBlock`.
    pub fn open_header(name: &str) -> Result<Self> {
        Self::open_view(name, crate::constants::SEGMENT_HEADER_SIZE)
    }

    /// `view_size = 0` -- секция целиком.
    fn open_view(name: &str, view_size: usize) -> Result<Self> {
        let mut nt_name = NtName::new(name)?;
        let mut obj_attr =
            OBJECT_ATTRIBUTES::new(nt_name.as_ptr(), OBJ_CASE_INSENSITIVE, null_mut());

        let mut section_handle: HANDLE = null_mut();

        // SAFETY: out-параметр на стеке; `obj_attr` и `nt_name` внутри него живы до
        // конца вызова.
        let status = unsafe {
            NtOpenSection(
                &raw mut section_handle,
                SECTION_ALL_ACCESS,
                &raw mut obj_attr,
            )
        };

        if status != STATUS_SUCCESS {
            return Err(status_to_error(status, "NtOpenSection"));
        }

        let handle = Handle(section_handle);
        Self::check_commit(&handle)?;
        Self::map_section(handle, view_size)
    }

    /// Секция `name` размером не меньше `size`: создаётся (NULL DACL,
    /// `SEC_COMMIT`) или, если имя уже занято, открывается (`OBJ_OPENIF`).
    /// Существующая секция проверяется как чужая: тип `SEC_COMMIT` и view не
    /// меньше `size`, иначе `Corrupted`. Для объектов, которые создаёт
    /// первая пришедшая сторона (состояние маяка).
    pub fn open_or_create(name: &str, size: usize) -> Result<Self> {
        let mut nt_name = NtName::new(name)?;
        let mut sd = NullDaclSecurityDescriptor::new();
        let mut obj_attr = OBJECT_ATTRIBUTES::new(
            nt_name.as_ptr(),
            OBJ_CASE_INSENSITIVE | OBJ_OPENIF,
            sd.as_ptr(),
        );
        let mut section_handle: HANDLE = null_mut();
        let mut max_size = LARGE_INTEGER {
            QuadPart: i64::try_from(size)
                .map_err(|_| ShmError::InvalidConfig("section size exceeds i64::MAX"))?,
        };
        // SAFETY: все переданные указатели -- на локальные переменные, живущие
        // до конца вызова (`obj_attr` держит `nt_name`/`sd`, они тоже живы).
        let status = unsafe {
            NtCreateSection(
                &raw mut section_handle,
                SECTION_ALL_ACCESS,
                &raw mut obj_attr,
                &raw mut max_size,
                PAGE_READWRITE,
                SEC_COMMIT,
                null_mut(),
            )
        };
        if status != STATUS_SUCCESS && status != STATUS_OBJECT_NAME_EXISTS {
            return Err(status_to_error(status, "NtCreateSection(open-if)"));
        }
        if section_handle.is_null() {
            return Err(status_to_error(
                0xC000_0008_u32.cast_signed(), // STATUS_INVALID_HANDLE
                "NtCreateSection returned NULL handle",
            ));
        }
        let handle = Handle(section_handle);
        if status == STATUS_OBJECT_NAME_EXISTS {
            // Имя занял кто-то раньше -- секция чужая, проверяем как при open.
            Self::check_commit(&handle)?;
        }
        let mapping = Self::map_section(handle, 0)?;
        if mapping.size < size {
            return Err(ShmError::Corrupted);
        }
        Ok(mapping)
    }

    /// Тип секции -- до отображения: под именем канала враждебный процесс
    /// может держать `SEC_RESERVE` (страницы не выделены -- первое
    /// обращение к ним даёт access violation) или секцию файла/образа.
    /// Сегмент канала всегда `SEC_COMMIT` из файла подкачки.
    fn check_commit(handle: &Handle) -> Result<()> {
        let attributes = Self::section_attributes(handle)?;
        if attributes & (SEC_RESERVE | SEC_IMAGE | SEC_FILE) != 0 || attributes & SEC_COMMIT == 0 {
            return Err(ShmError::Corrupted);
        }
        Ok(())
    }

    /// Отобразить секцию (`view_size = 0` -- целиком).
    fn map_section(handle: Handle, view_size: usize) -> Result<Self> {
        let mut base_address: PVOID = null_mut();
        let mut view_size: usize = view_size;

        // SAFETY: дескриптор секции валиден (держит `handle`); out-параметры --
        // локальные переменные, живущие до конца вызова.
        let status = unsafe {
            NtMapViewOfSection(
                handle.raw(),
                NT_CURRENT_PROCESS,
                &raw mut base_address,
                0,
                0,
                null_mut(),
                &raw mut view_size,
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
            view: base_address.cast::<u8>(),
            size: view_size,
        })
    }

    /// `AllocationAttributes` секции (`NtQuerySection(SectionBasicInformation)`).
    fn section_attributes(handle: &Handle) -> Result<u32> {
        let mut info = core::mem::MaybeUninit::<SECTION_BASIC_INFORMATION>::zeroed();
        let mut returned: usize = 0;
        // SAFETY: дескриптор секции валиден (держит `handle`); буфер -- локальная
        // структура ровно того размера, что передаётся в `SectionInformationLength`;
        // `returned` -- out-параметр на стеке.
        let status = unsafe {
            NtQuerySection(
                handle.raw(),
                SectionBasicInformation,
                info.as_mut_ptr().cast::<c_void>(),
                size_of::<SECTION_BASIC_INFORMATION>(),
                &raw mut returned,
            )
        };
        if status != STATUS_SUCCESS {
            return Err(status_to_error(status, "NtQuerySection"));
        }
        // SAFETY: структура обнулена до вызова (все её поля -- целые и
        // указатель, нулевой битовый шаблон допустим) и заполнена ядром.
        let info = unsafe { info.assume_init() };
        Ok(info.AllocationAttributes)
    }

    /// Размер отображённого view в байтах.
    pub const fn size(&self) -> usize {
        self.size
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

/// Ожидание любого из `handles` (`NtWaitForMultipleObjects`, `WaitAny`).
///
/// Набор -- заимствованные дескрипторы (`BorrowedHandle`): объекты обязаны
/// жить всё ожидание, и это проверяет компилятор, а не соглашение о `isize`.
/// `Ok(Some(i))` -- сигнален `handles[i]` (при одновременном сигнале --
/// наименьший индекс), `Ok(None)` -- таймаут или пустой набор.
pub fn wait_any(
    handles: &[BorrowedHandle<'_>],
    timeout: Option<Duration>,
) -> Result<Option<usize>> {
    if handles.is_empty() {
        return Ok(None);
    }

    let timeout_value: i64 = match timeout {
        Some(d) => duration_to_nt_timeout(d),
        None => 0,
    };

    let timeout_ptr = if timeout.is_some() {
        &raw const timeout_value
    } else {
        null()
    };

    // Ядро принимает не больше MAXIMUM_WAIT_OBJECTS = 64 дескрипторов;
    // длина больше `u32::MAX` -- заведомо ошибка параметра.
    let count = u32::try_from(handles.len())
        .map_err(|_| ShmError::InvalidConfig("too many handles to wait on"))?;
    // SAFETY: `handles` -- непустой срез живых NT-дескрипторов (проверено
    // выше; время жизни -- `BorrowedHandle`), `BorrowedHandle` --
    // `repr(transparent)` над `RawHandle` = `HANDLE`, поэтому срез -- это
    // массив `HANDLE` длины `count`; `timeout_ptr` живёт до конца вызова.
    let status = unsafe {
        NtWaitForMultipleObjects(
            count,
            handles.as_ptr().cast::<HANDLE>(),
            WAIT_ANY,
            0, // Alertable = FALSE
            timeout_ptr,
        )
    };

    // STATUS_TIMEOUT (0x102) проверяем ДО диапазона валидных индексов: это
    // значение >= 0 и по чистой случайности совпало бы с индексом 258,
    // если бы handles.len() когда-нибудь превысил этот порог. Сейчас это
    // не достижимо: ядро принимает не больше MAXIMUM_WAIT_OBJECTS = 64
    // handle (больше -- STATUS_INVALID_PARAMETER), самый большой набор --
    // worker Multi (3 * SLOTS_PER_WORKER + wake = 64), -- но порядок веток
    // не должен полагаться на этот внешний инвариант.
    match status {
        STATUS_TIMEOUT => Ok(None),
        s if let Ok(index) = usize::try_from(s)
            && index < handles.len() =>
        {
            Ok(Some(index))
        }
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

impl AsHandle for ProcessWatch {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.handle.as_handle()
    }
}

impl AsRawHandle for ProcessWatch {
    fn as_raw_handle(&self) -> RawHandle {
        self.handle.as_raw_handle()
    }
}

impl ProcessWatch {
    /// Открыть процесс `pid` для наблюдения. `None` -- PID неизвестен (0),
    /// процесса уже нет, либо не хватает прав (например, пир -- служба в
    /// другой сессии); вызывающий тогда работает без детекции смерти.
    pub fn open(pid: u32) -> Option<Self> {
        if pid == 0 {
            return None; // 0 -- «PID не передан» (пир старой версии)
        }
        Self::open_status(pid).ok()
    }

    /// `open` с кодом отказа `NtOpenProcess`: отличает «процесса с таким PID
    /// нет» (`STATUS_INVALID_CID`/`STATUS_INVALID_PARAMETER`) от «нет прав».
    fn open_status(pid: u32) -> core::result::Result<Self, NTSTATUS> {
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
                &raw mut raw_handle,
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                &raw mut obj_attr,
                &raw mut client_id,
            )
        };
        if open_status != STATUS_SUCCESS {
            return Err(open_status);
        }
        if raw_handle.is_null() {
            return Err(0xC000_0008_u32.cast_signed()); // STATUS_INVALID_HANDLE
        }
        Ok(Self {
            handle: Handle(raw_handle),
            pid,
        })
    }

    /// Открыть наблюдение за пиром, чей PID пришёл в handshake. Процесс,
    /// который уже мёртв в момент подключения, НЕ наблюдается (`None`, а не
    /// «сразу умер»): живой пир не мог бы прислать HELLO из мёртвого процесса,
    /// значит PID устаревший (например, остался от упавшего посреди handshake
    /// клиента), и честный ответ -- «неизвестно».
    ///
    /// Находка 23: `start_time` -- время создания процесса пира
    /// (`KERNEL_USER_TIMES::CreateTime`), которое тот сообщил в сегменте.
    /// `0` -- пир время не сообщил (старая версия): сверки нет. Иначе процесс
    /// с этим PID, созданный в другое время (PID переиспользован после смерти
    /// пира или подделан), -- НЕ пир: `None`, пир считается умершим. Время,
    /// которое не удалось узнать, тоже даёт `None` -- подтвердить личность
    /// пира нечем, а наблюдать чужой процесс хуже, чем не наблюдать никого.
    pub fn open_peer(pid: u32, start_time: u64) -> Option<Self> {
        Self::open(pid)
            .filter(|watch| !watch.has_exited())
            .filter(|watch| start_time == 0 || watch.start_time() == Some(start_time))
    }

    /// Время создания наблюдаемого процесса (`CreateTime`, 100-нс FILETIME);
    /// `None` -- запрос не удался.
    #[must_use]
    pub fn start_time(&self) -> Option<u64> {
        query_start_time(self.handle.raw())
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
        let wait_status =
            unsafe { NtWaitForSingleObject(self.handle.raw(), 0, &raw const zero_timeout) };
        wait_status == STATUS_WAIT_0
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
///
/// Находка 23: `start_time != 0` -- время создания, сообщённое владельцем
/// PID. Открывшийся процесс, созданный в ДРУГОЕ время, -- уже не тот
/// процесс (PID переиспользован): `false`. Время, которое не удалось
/// узнать, -- двусмысленность, `true`.
#[must_use]
pub fn is_process_alive(pid: u32, start_time: u64) -> bool {
    if pid == 0 {
        return true; // 0 не бывает PID пользовательского процесса
    }
    ProcessWatch::open(pid).is_none_or(|watch| {
        if watch.has_exited() {
            return false;
        }
        start_time == 0 || watch.start_time().is_none_or(|actual| actual == start_time)
    })
}

/// Процесс `pid`, созданный в `start_time`, подтверждённо не существует
/// (обратная консервативность к [`is_process_alive`]): `true` ТОЛЬКО если
/// процесса с таким PID нет вовсе (`NtOpenProcess` ->
/// `STATUS_INVALID_CID`/`STATUS_INVALID_PARAMETER`), он завершился, или
/// процесс с этим PID создан в другое время (`start_time != 0`, своё время
/// известно и не совпало -- PID переиспользован). Любая двусмысленность (нет
/// прав, время не узнать, PID 0) -- `false`. Для решений «погасить чужой
/// сигнал за упавший процесс» (маяк): ошибочное «умер» здесь дороже
/// ошибочного «жив».
#[must_use]
pub fn is_process_gone(pid: u32, start_time: u64) -> bool {
    if pid == 0 {
        return false;
    }
    match ProcessWatch::open_status(pid) {
        Ok(watch) => {
            watch.has_exited()
                || (start_time != 0 && watch.start_time().is_some_and(|t| t != start_time))
        }
        Err(status) => status == STATUS_INVALID_CID || status == STATUS_INVALID_PARAMETER,
    }
}

/// Время создания процесса `pid` (`KERNEL_USER_TIMES::CreateTime`, 100-нс
/// FILETIME). `None` -- PID 0, процесс не открылся (нет прав, не существует)
/// или запрос не удался.
#[must_use]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "разовая сверка по PID; в библиотеке пока только тесты"
    )
)]
pub fn process_start_time(pid: u32) -> Option<u64> {
    ProcessWatch::open(pid)?.start_time()
}

/// Время создания текущего процесса; `0` -- запрос не удался (тогда пиру
/// сообщается «время неизвестно», и он не сверяет его).
#[must_use]
pub fn current_process_start_time() -> u64 {
    query_start_time(NT_CURRENT_PROCESS).unwrap_or(0)
}

/// `NtQueryInformationProcess(ProcessTimes)` -> `CreateTime`. Нулевое время
/// трактуется как «неизвестно» (`None`): `0` в сегменте зарезервирован под
/// «не сообщено».
fn query_start_time(process: HANDLE) -> Option<u64> {
    let mut times = KERNEL_USER_TIMES::default();
    let mut returned: ULONG = 0;
    // SAFETY: буфер -- локальная структура ровно того размера, что передан
    // (32 байта на обеих разрядностях -- проверено тестом раскладки в
    // `ntapi`); `process` -- валидный handle с правом
    // PROCESS_QUERY_LIMITED_INFORMATION или псевдо-handle текущего процесса.
    let status = unsafe {
        NtQueryInformationProcess(
            process,
            ProcessTimes,
            core::ptr::from_mut(&mut times).cast(),
            size_of::<KERNEL_USER_TIMES>() as ULONG,
            &raw mut returned,
        )
    };
    if status != STATUS_SUCCESS {
        return None;
    }
    // SAFETY: LARGE_INTEGER -- объединение одинаковых по размеру i64 и пары
    // u32; любое битовое значение допустимо для `QuadPart`.
    let create_time = unsafe { times.CreateTime.QuadPart };
    // FILETIME -- беззнаковое по смыслу; битовый каст, без проверки знака.
    Some(create_time.cast_unsigned()).filter(|&time| time != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_is_alive() {
        assert!(is_process_alive(std::process::id(), 0));
    }

    /// Находка 23: процесс с тем же PID, но другим временем создания -- не
    /// пир (PID переиспользован или подделан). Точное время -- пир.
    #[test]
    fn start_time_mismatch_is_not_the_peer() {
        let me = std::process::id();
        let start = current_process_start_time();
        assert_ne!(start, 0);
        assert_eq!(process_start_time(me), Some(start));

        assert!(ProcessWatch::open_peer(me, start.wrapping_add(1)).is_none());
        assert!(!is_process_alive(me, start.wrapping_add(1)));

        assert!(ProcessWatch::open_peer(me, start).is_some());
        assert!(is_process_alive(me, start));
        // 0 -- пир время не сообщил: поведение как до 0.10.
        assert!(ProcessWatch::open_peer(me, 0).is_some());
        assert!(is_process_alive(me, 0));
    }

    /// Регрессия (аудит 2026-07-28): `Mapping::open` доверял чужой секции и
    /// не сверял её размер. Любой процесс может первым занять имя в
    /// `\BaseNamedObjects` (NULL DACL, общий namespace) и создать секцию
    /// меньше заявленной раскладки -- дальнейший штатный доступ к ring
    /// buffer'ам ушёл бы ЗА пределы отображения.
    #[test]
    fn attach_rejects_undersized_section() {
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
                &raw mut handle,
                SECTION_ALL_ACCESS,
                &raw mut obj_attr,
                &raw mut max_size,
                PAGE_READWRITE,
                SEC_COMMIT,
                null_mut(),
            )
        };
        assert_eq!(status, STATUS_SUCCESS, "не удалось создать тестовую секцию");
        let _owner = Handle(handle); // держим секцию живой + RAII-закрытие

        // Формат 0x0002_0000: размер проверяется против раскладки,
        // записанной в самом сегменте. Враждебная секция 4 КБ, которая
        // выдаёт себя за опубликованный сегмент на 4 МБ, -- отказ.
        let mapping = Mapping::open(&name).expect("open");
        let word = |offset: usize| {
            // SAFETY: view не меньше страницы (4 КБ), `offset` < 4 КБ.
            let ptr = unsafe { mapping.as_ptr().add(offset) };
            // SAFETY: адрес выровнен на 4 (view выровнен на страницу, смещения
            // кратны 4) и живёт до конца теста; пишем заголовок атомарно, как
            // это сделал бы чужой процесс.
            unsafe { std::sync::atomic::AtomicU32::from_ptr(ptr.cast()) }
        };
        let relaxed = std::sync::atomic::Ordering::Relaxed;
        word(4).store(crate::constants::SHARED_VERSION, relaxed);
        word(64).store(
            crate::layout::ChannelLayout::DEFAULT.segment_size() as u32,
            relaxed,
        );
        word(0).store(crate::constants::SHARED_MAGIC, relaxed);
        // SAFETY: отображение живо до конца теста.
        let attached =
            unsafe { crate::shared::SharedView::attach(mapping.as_ptr(), mapping.size()) };
        match attached {
            Err(ShmError::Corrupted) => {}
            other => panic!("секция 4 КБ должна отвергаться, получено: {other:?}"),
        }
    }

    /// Создать именованную секцию 64 КиБ с заданными атрибутами (для тестов
    /// на подмену секции чужим процессом).
    fn create_test_section(name: &str, attributes: u32) -> Handle {
        let mut nt_name = NtName::new(name).expect("nt name");
        let mut sd = NullDaclSecurityDescriptor::new();
        let mut obj_attr =
            OBJECT_ATTRIBUTES::new(nt_name.as_ptr(), OBJ_CASE_INSENSITIVE, sd.as_ptr());
        let mut handle: HANDLE = null_mut();
        let mut max_size = LARGE_INTEGER {
            QuadPart: 64 * 1024,
        };
        // SAFETY: все указатели валидны и живут до конца вызова.
        let status = unsafe {
            NtCreateSection(
                &raw mut handle,
                SECTION_ALL_ACCESS,
                &raw mut obj_attr,
                &raw mut max_size,
                PAGE_READWRITE,
                attributes,
                null_mut(),
            )
        };
        assert_eq!(status, STATUS_SUCCESS, "не удалось создать тестовую секцию");
        Handle(handle)
    }

    /// Находка 14 (аудит 0.10): чужая секция `SEC_RESERVE` под именем канала
    /// отображается, но её страницы не выделены -- первое обращение даёт
    /// access violation. `Mapping::open` обязан отвергать всё, что не
    /// `SEC_COMMIT`; честная секция `SEC_COMMIT` открывается.
    #[test]
    fn open_rejects_reserved_or_image_section() {
        let reserved = format!("Local\\XSHM_TEST_SEC_RESERVE_{}", std::process::id());
        let _reserved_owner = create_test_section(&reserved, SEC_RESERVE);
        match Mapping::open(&reserved) {
            Err(ShmError::Corrupted) => {}
            Ok(_) => panic!("секция SEC_RESERVE принята"),
            Err(other) => panic!("ожидался Corrupted, получено {other:?}"),
        }
        match Mapping::open_header(&reserved) {
            Err(ShmError::Corrupted) => {}
            Ok(_) => panic!("секция SEC_RESERVE принята (open_header)"),
            Err(other) => panic!("ожидался Corrupted, получено {other:?}"),
        }

        let commit = format!("Local\\XSHM_TEST_SEC_COMMIT_{}", std::process::id());
        let _commit_owner = create_test_section(&commit, SEC_COMMIT);
        assert!(Mapping::open(&commit).is_ok());
        assert!(Mapping::open_header(&commit).is_ok());
    }

    /// Находка 11 (состояние маяка): `open_or_create` создаёт секцию, второй
    /// вызов открывает ту же; занятое заранее имя `SEC_RESERVE` -- отказ,
    /// а не отображение невыделенных страниц.
    #[test]
    fn open_or_create_shares_section_and_rejects_reserved_squatter() {
        let name = format!("Local\\XSHM_TEST_OPENIF_{}", std::process::id());
        let first = Mapping::open_or_create(&name, 4096).unwrap();
        let second = Mapping::open_or_create(&name, 4096).unwrap();
        assert!(first.size() >= 4096 && second.size() >= 4096);
        // SAFETY: view не меньше страницы, адрес выровнен на страницу и живёт
        // до конца теста.
        let a = unsafe { std::sync::atomic::AtomicU32::from_ptr(first.as_ptr().cast()) };
        // SAFETY: то же для второго отображения.
        let b = unsafe { std::sync::atomic::AtomicU32::from_ptr(second.as_ptr().cast()) };
        a.store(0x1234_5678, std::sync::atomic::Ordering::Release);
        assert_eq!(b.load(std::sync::atomic::Ordering::Acquire), 0x1234_5678);

        let reserved = format!("Local\\XSHM_TEST_OPENIF_RESERVE_{}", std::process::id());
        let _owner = create_test_section(&reserved, SEC_RESERVE);
        assert_eq!(
            Mapping::open_or_create(&reserved, 4096).err(),
            Some(ShmError::Corrupted)
        );
    }

    /// `is_process_gone` консервативен в сторону «жив»: свой процесс и PID 0
    /// не «исчезли»; завершившийся процесс с закрытым handle -- исчез.
    #[test]
    fn is_process_gone_only_for_confirmed_exit() {
        let me = std::process::id();
        assert!(!is_process_gone(me, 0));
        assert!(!is_process_gone(me, current_process_start_time()));
        assert!(is_process_gone(me, current_process_start_time() ^ 1));
        assert!(!is_process_gone(0, 0));
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "exit", "0"])
            .spawn()
            .expect("cmd");
        let pid = child.id();
        let start = process_start_time(pid).expect("время создания");
        drop(child.wait());
        assert!(
            is_process_gone(pid, start),
            "handle ещё открыт, процесс сигнален"
        );
        drop(child);
        assert!(is_process_gone(pid, start), "процесса больше нет");
    }

    /// Подменённое событие канала (ручной сброс под именем автосбросного)
    /// отвергается при открытии, настоящее -- открывается.
    #[test]
    fn open_auto_reset_rejects_manual_reset_squatter() {
        let fake = format!("Local\\XSHM_TEST_SQUAT_{}", std::process::id());
        let _squatter = EventHandle::open_or_create_notification(&fake).unwrap();
        assert_eq!(
            EventHandle::open_auto_reset(&fake).err(),
            Some(ShmError::Corrupted)
        );
        let real = format!("Local\\XSHM_TEST_AUTO_{}", std::process::id());
        let _owner = EventHandle::create(&real).unwrap();
        assert!(EventHandle::open_auto_reset(&real).is_ok());
    }

    /// Мьютекс лобби: второй захват ждёт первого; поток-владелец, умерший без
    /// освобождения, оставляет мьютекс «брошенным» -- его забирают сразу.
    #[test]
    fn named_mutex_serializes_and_survives_abandoned_owner() {
        let name = format!("Local\\XSHM_TEST_MUTEX_{}", std::process::id());
        let mutex = NamedMutex::open_or_create(&name).unwrap();
        let guard = mutex.lock(Some(Duration::from_secs(1))).unwrap().unwrap();
        assert!(!guard.was_abandoned());
        {
            let name = name.clone();
            std::thread::spawn(move || {
                let other = NamedMutex::open_or_create(&name).unwrap();
                assert!(
                    other
                        .lock(Some(Duration::from_millis(50)))
                        .unwrap()
                        .is_none(),
                    "занятый мьютекс захвачен вторым потоком"
                );
            })
            .join()
            .unwrap();
        }
        drop(guard);

        // Владелец-поток завершается, не освободив мьютекс.
        {
            std::thread::spawn(move || {
                let owner = NamedMutex::open_or_create(&name).unwrap();
                let guard = owner.lock(Some(Duration::from_secs(1))).unwrap().unwrap();
                std::mem::forget(guard);
            })
            .join()
            .unwrap();
        }
        let t0 = std::time::Instant::now();
        let guard = mutex.lock(Some(Duration::from_secs(5))).unwrap().unwrap();
        assert!(guard.was_abandoned(), "ожидался брошенный мьютекс");
        assert!(t0.elapsed() < Duration::from_secs(1));
    }

    /// Имя мьютекса, занятое объектом другого типа, -- ошибка, а не чужой объект.
    #[test]
    fn named_mutex_rejects_other_object_type() {
        let name = format!("Local\\XSHM_TEST_MUTEX_TYPE_{}", std::process::id());
        let _event = EventHandle::create(&name).unwrap();
        match NamedMutex::open_or_create(&name) {
            Err(ShmError::WindowsError { code, .. }) => {
                assert_eq!(code, STATUS_OBJECT_TYPE_MISMATCH.cast_unsigned());
            }
            other => panic!("ожидалась ошибка типа, получено {other:?}"),
        }
    }

    /// Маяк (NotificationEvent), подменённый автосбросным событием, --
    /// ошибка открытия; честное событие-уведомление открывается.
    #[test]
    fn notification_open_rejects_synchronization_event() {
        let name = format!("Local\\XSHM_TEST_BEACON_TYPE_{}", std::process::id());
        let _squatter = EventHandle::create(&name).unwrap(); // SynchronizationEvent
        match EventHandle::open_or_create_notification(&name) {
            Err(ShmError::WindowsError { code, .. }) => {
                assert_eq!(code, STATUS_OBJECT_TYPE_MISMATCH.cast_unsigned());
            }
            other => panic!("подмена маяка не отвергнута: {other:?}"),
        }

        let fair = format!("Local\\XSHM_TEST_BEACON_OK_{}", std::process::id());
        let first = EventHandle::open_or_create_notification(&fair).unwrap();
        let second = EventHandle::open_or_create_notification(&fair).unwrap();
        assert_eq!(second.event_type().unwrap(), NOTIFICATION_EVENT);
        drop(first);
    }

    /// Набор ожидания -- заимствованные handle (`BorrowedHandle`): время
    /// жизни объекта проверяет компилятор. Сигнал второго события -- индекс 1.
    #[test]
    fn wait_any_accepts_borrowed_handles() {
        let first = EventHandle::create_unnamed(false).unwrap();
        let second = EventHandle::create_unnamed(false).unwrap();
        assert_eq!(
            wait_any(
                &[first.as_handle(), second.as_handle()],
                Some(Duration::ZERO)
            ),
            Ok(None)
        );
        second.set().unwrap();
        assert_eq!(
            wait_any(
                &[first.as_handle(), second.as_handle()],
                Some(Duration::from_secs(1))
            ),
            Ok(Some(1))
        );
    }

    #[test]
    fn pid_zero_is_conservatively_alive() {
        // 0 не бывает PID пользовательского процесса — не должен читаться
        // как "подтверждённо мёртв".
        assert!(is_process_alive(0, 0));
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
            if !is_process_alive(pid, 0) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("is_process_alive(pid={pid}) должен был вернуть false после child.wait()");
    }
}
