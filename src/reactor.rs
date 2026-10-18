//! Реактор на IOCP + Wait Completion Packets (0.10, внутренний).
//!
//! Ожидание объектов ядра без потока на канал: объект привязывается к
//! порту завершения пакетом ожидания (`NtAssociateWaitCompletionPacket`),
//! при сигнале объекта ядро ставит пакет в порт, один поток извлекает пакеты
//! пачками (`NtRemoveIoCompletionEx`) и будит `Waker` по ключу.
//!
//! Два слоя:
//! - [`CompletionPort`] + [`WaitPacket`] -- переиспользуемая обёртка над
//!   своим IOCP: привязка объекта с ключом, ожидание ключа с таймаутом.
//!   Multi (Task 17) держит свой порт в своём потоке, и его дедлайны
//!   остаются таймаутом ожидания.
//! - [`Reactor`] -- таблица регистраций и поток `xsr` поверх порта; общий
//!   экземпляр -- [`Reactor::global`] (потребитель -- async API, Task 16).
//!
//! # Регистрация -- это ожидание
//!
//! Привязка пакета к объекту удовлетворяет ожидание так же, как
//! `NtWaitForSingleObject`: сигнал автосбросного события
//! (`SynchronizationEvent`) пакет **потребляет** -- событие сбрасывается,
//! другой ждущий его уже не увидит. Если событие было взведено до привязки,
//! ядро потребляет сигнал сразу, сообщает `AlreadySignaled = TRUE` и всё
//! равно ставит пакет в порт (проверено тестом
//! `port_already_signaled_packet_is_queued`): отдельно будить не нужно.
//!
//! # Отмена: потреблён ли сигнал
//!
//! Отмена двухшаговая (проверено тестами на x64 и i686):
//! `NtCancelWaitCompletionPacket(.., RemoveSignaledPacket = FALSE)` отвечает
//! `STATUS_SUCCESS` -- ожидание снято до сигнала (сигнал цел),
//! `STATUS_PENDING` -- сигнал потреблён, пакет стоит в очереди порта (тогда
//! вторым шагом `TRUE` пакет вынимается), `STATUS_CANCELLED` -- пакет уже
//! извлечён из порта (или не привязывался). Итог -- [`CancelOutcome`]:
//! `Registration::cancel` сообщает его владельцу, чтобы async API (Task 16)
//! мог вернуть потреблённый сигнал автосбросного события (`NtSetEvent`) или
//! перепроверить кольцо. Сам реактор сигналы не восстанавливает -- он не
//! знает, что за объект ждёт.
//!
//! # Ключи и гонка отмены
//!
//! Ключ пакета -- не указатель, а пара (индекс слота таблицы, поколение
//! слота): `KeyContext` = индекс, `ApcContext` = поколение -- оба полного
//! размера указателя, на x86 тоже. Снятие регистрации (срабатывание или
//! отмена) увеличивает поколение слота. Отмена может проиграть гонку: поток
//! реактора уже извлёк пакет, но ещё не дошёл до таблицы. Тогда он находит
//! слот пустым или с новым поколением и молча отбрасывает пакет -- ни
//! обращения к освобождённому, ни пробуждения чужого `Waker`.
//!
//! Обратная гонка: реактор уже вынул запись из таблицы и будит `Waker` вне
//! блокировки, а отмена в это время видит слот снятым и возвращается
//! (`CancelOutcome::AlreadyDelivered`). Поэтому после возврата из отмены
//! или `Drop` возможно **не более одного** уже начатого пробуждения --
//! контракт `Waker` допускает ложные пробуждения. Ждать его в `Drop` нельзя:
//! `Drop` из самого `Waker` на потоке `xsr` зависал бы навсегда.
//!
//! # Пул пакетов
//!
//! Пакет ожидания -- объект ядра. Создавать его на каждую регистрацию
//! (`recv_async`/`send_async` регистрируются на каждом `Pending`) -- лишние
//! системные вызовы и, главное, лишний режим отказа посреди живой сессии:
//! под давлением ресурсов (`STATUS_INSUFFICIENT_RESOURCES`, квота handle-ов)
//! `NtCreateWaitCompletionPacket` отказывает, хотя само ожидание возможно.
//! Поэтому снятые пакеты возвращаются в пул таблицы и переиспользуются:
//! в установившемся режиме регистрация не создаёт объектов ядра, отказ
//! создания возможен только при росте числа одновременных регистраций.
//!
//! В пул попадает только **заведомо простаивающий** пакет -- не привязанный
//! и не стоящий в очереди порта:
//! - извлечённый реактором из порта и доставленный (`process`);
//! - снятый отменой с «чистым» кодом (`WaitPacket::cancel_for_reuse`):
//!   `STATUS_SUCCESS` (ожидание снято до сигнала), `STATUS_PENDING` ->
//!   `STATUS_SUCCESS` (вынут из очереди), `STATUS_CANCELLED` (уже извлечён
//!   из порта -- в том числе реактором, который ещё не дошёл до таблицы);
//! - не привязавшийся, когда отказ `NtAssociateWaitCompletionPacket`
//!   доказанно -- вина объекта ожидания: код допускает её **и** проба
//!   пакета отменой отвечает `STATUS_CANCELLED`
//!   (`WaitPacket::survives_arm_failure`; одни коды не различают пакет и
//!   объект -- `STATUS_INVALID_HANDLE`, `STATUS_OBJECT_TYPE_MISMATCH` ядро
//!   отвечает и на негодный пакет). Иначе пакет закрывается, а регистрация
//!   с пакетом из пула повторяется один раз с новым: «грязный» пакет на
//!   вершине пула не отказывает каждой следующей регистрации.
//!
//! Любой иной код отмены (невозможен при живом дескрипторе) -- пакет
//! закрывается, а не переиспользуется. Завершение, уже извлечённое
//! реактором, но ещё не обработанное, несёт ключ прежней регистрации
//! (индекс слота, старое поколение): переиспользованный пакет привязан с
//! новым ключом, и это завершение отбрасывается как устаревшее (тест
//! `pooled_packet_is_clean_after_every_outcome`). В порт реактора пишет
//! только ядро по привязкам: чужих `NtSetIoCompletion` с живым ключом нет,
//! поэтому завершение с живым ключом -- всегда завершение пакета этой
//! записи. Пул ограничен [`POOL_LIMIT`]: сверх предела пакеты закрываются,
//! удерживаемые handle-ы ограничены. Пул не сжимается: после всплеска
//! одновременных регистраций до `POOL_LIMIT` (64) простаивающих пакетов
//! (handle-ов) остаются у общего реактора до конца процесса -- это цена
//! того, что следующий всплеск не создаёт объектов ядра. Закрытие и пробуждение -- вне
//! блокировки таблицы; пакеты пачки возвращаются в пул за тот же захват
//! блокировки, в котором снимаются записи.

use std::mem::MaybeUninit;
use std::os::windows::io::{AsRawHandle, BorrowedHandle};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, mpsc};
use std::task::Waker;
use std::thread;
use std::time::Duration;

use crate::auto::map_spawn_error;
use crate::error::{Result, ShmError};
use crate::ntapi::{
    FILE_IO_COMPLETION_INFORMATION, GENERIC_ALL, HANDLE, IO_COMPLETION_ALL_ACCESS, NTSTATUS,
    NtAssociateWaitCompletionPacket, NtCancelWaitCompletionPacket, NtCreateIoCompletion,
    NtCreateWaitCompletionPacket, NtRemoveIoCompletionEx, NtSetIoCompletion, STATUS_ACCESS_DENIED,
    STATUS_CANCELLED, STATUS_INVALID_HANDLE, STATUS_INVALID_PARAMETER_3,
    STATUS_OBJECT_TYPE_MISMATCH, STATUS_PENDING, STATUS_SUCCESS, STATUS_TIMEOUT, ULONG,
    duration_to_nt_timeout,
};
use crate::sync_ext::LockExt;
use crate::win::{Handle, status_to_error};

/// Сколько пакетов забирает один вызов `NtRemoveIoCompletionEx`.
const BATCH: ULONG = 64;
/// То же для размеров массивов (u32 -> usize без потерь на x86/x64).
const BATCH_LEN: usize = BATCH as usize;

/// Пауза потока реактора после отказа `NtRemoveIoCompletionEx`. Отказ
/// возможен только при негодном handle порта, а порт живёт до конца
/// процесса, -- страховка от горячего цикла, а не рабочий путь.
const ERROR_BACKOFF: Duration = Duration::from_millis(100);

/// Максимум простаивающих пакетов ожидания в пуле реактора (см. «Пул
/// пакетов»): ограничивает удерживаемые handle-ы; сверх предела снятые
/// пакеты закрываются.
const POOL_LIMIT: usize = 64;

// ============================================================================
// Порт завершения
// ============================================================================

/// Извлечённый из порта пакет: `KeyContext` и `ApcContext` привязки
/// (или `NtSetIoCompletion`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Completion {
    /// Ключ (`KeyContext`).
    pub(crate) key: usize,
    /// Метка (`ApcContext`); у [`Reactor`] -- поколение слота.
    pub(crate) tag: usize,
}

/// Исход двухшаговой отмены ожидания (коды проверены тестами
/// `port_cancel_outcomes` и `registration_cancel_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CancelOutcome {
    /// Ожидание снято до сигнала (`STATUS_SUCCESS` при `FALSE`): сигнал
    /// объекта не потреблён, `Waker` не будится.
    NotFired,
    /// Сигнал потреблён, но не доставлен: пакет вынут из очереди порта
    /// (`STATUS_PENDING`, затем `TRUE`) или извлечён реактором уже после
    /// снятия записи (будет отброшен как устаревший). `Waker` не будится;
    /// у автосбросного события сигнал потерян -- его возвращает владелец
    /// (`NtSetEvent`) или перепроверяет состояние сам.
    FiredNotDelivered,
    /// Пакет уже извлечён из порта (`STATUS_CANCELLED`): у [`Registration`]
    /// -- `Waker` разбужен или вот-вот будет (ровно один раз). У голого
    /// [`WaitPacket`] -- то же, что «извлечён своим владельцем» или «не
    /// привязывался».
    AlreadyDelivered,
}

/// Свой порт завершения ввода-вывода (IOCP) с ожиданием объектов через
/// пакеты ожидания.
#[derive(Debug)]
pub(crate) struct CompletionPort {
    handle: Handle,
    /// Возвраты из `NtRemoveIoCompletionEx` -- доказательство «ноль
    /// пробуждений в простое».
    #[cfg(test)]
    removals: AtomicU64,
}

// SAFETY: внутри -- NT-дескриптор порта (машинное слово); операции над
// портом (`NtSetIoCompletion`, `NtRemoveIoCompletionEx`, привязка) ядро
// синхронизирует само, состояния на стороне Rust нет.
unsafe impl Send for CompletionPort {}
// SAFETY: см. `Send`: через `&CompletionPort` доступны только вызовы ядра.
unsafe impl Sync for CompletionPort {}

impl CompletionPort {
    /// Новый безымянный порт.
    pub(crate) fn new() -> Result<Self> {
        let mut handle: HANDLE = ptr::null_mut();
        // SAFETY: out-параметр -- локальная переменная; атрибутов нет
        // (безымянный объект); 0 потоков = по числу процессоров.
        let status = unsafe {
            NtCreateIoCompletion(
                &raw mut handle,
                IO_COMPLETION_ALL_ACCESS,
                ptr::null_mut(),
                0,
            )
        };
        if status != STATUS_SUCCESS || handle.is_null() {
            return Err(status_to_error(status, "NtCreateIoCompletion"));
        }
        // SAFETY: свежий дескриптор успешного создания, владелец -- только он.
        let handle = unsafe { Handle::from_raw(handle) };
        Ok(Self {
            handle,
            #[cfg(test)]
            removals: AtomicU64::new(0),
        })
    }

    /// Привязать объект `target` с ключом `key` (метка 0): при сигнале
    /// объекта в порт придёт пакет с этим ключом. Привязка -- это ожидание
    /// (см. модуль): сигнал автосбросного события она потребляет.
    pub(crate) fn associate(&self, target: BorrowedHandle<'_>, key: usize) -> Result<WaitPacket> {
        let mut packet = WaitPacket::new()?;
        self.arm(&mut packet, target.as_raw_handle(), key, 0)?;
        Ok(packet)
    }

    /// `associate` по числовому handle и с меткой (`ApcContext`).
    pub(crate) fn associate_raw(
        &self,
        target: isize,
        key: usize,
        tag: usize,
    ) -> Result<WaitPacket> {
        let mut packet = WaitPacket::new()?;
        self.arm(&mut packet, raw_handle(target), key, tag)?;
        Ok(packet)
    }

    /// Привязать (или перепривязать сработавший) пакет к объекту. Негодный
    /// или закрытый `target` -- `Err(WindowsError)` (ядро проверяет handle
    /// при привязке и держит ссылку на объект до срабатывания или отмены).
    pub(crate) fn arm(
        &self,
        packet: &mut WaitPacket,
        target: HANDLE,
        key: usize,
        tag: usize,
    ) -> Result<()> {
        match self.arm_status(packet, target, key, tag) {
            STATUS_SUCCESS => Ok(()),
            status => Err(status_to_error(status, "NtAssociateWaitCompletionPacket")),
        }
    }

    /// [`arm`](Self::arm) с сырым кодом: по нему и по пробе пакета
    /// ([`WaitPacket::survives_arm_failure`]) владелец решает, годен ли
    /// пакет после отказа.
    pub(crate) fn arm_status(
        &self,
        packet: &mut WaitPacket,
        target: HANDLE,
        key: usize,
        tag: usize,
    ) -> NTSTATUS {
        let mut signaled: u8 = 0;
        // SAFETY: пакет и порт -- живые дескрипторы своих владельцев;
        // `target` ядро проверяет само (негодный -- код ошибки, не UB);
        // ключ и метка -- непрозрачные числа, ядро их не разыменовывает;
        // `signaled` -- локальный out-параметр.
        let status = unsafe {
            NtAssociateWaitCompletionPacket(
                packet.handle.raw(),
                self.handle.raw(),
                target,
                ptr::without_provenance_mut(key),
                ptr::without_provenance_mut(tag),
                STATUS_SUCCESS,
                0,
                &raw mut signaled,
            )
        };
        if status == STATUS_SUCCESS {
            packet.already_signaled = signaled != 0;
        }
        status
    }

    /// Дождаться одного пакета: `Some(key)` или `None` по таймауту
    /// (`None` таймаута -- бесконечно).
    pub(crate) fn wait(&self, timeout: Option<Duration>) -> Result<Option<usize>> {
        let mut entries = [const { MaybeUninit::uninit() }; 1];
        let removed = self.remove(&mut entries, timeout)?;
        Ok(entries[..removed].first().map(|entry| {
            // SAFETY: ядро заполнило первые `removed` элементов.
            let entry = unsafe { entry.assume_init_ref() };
            entry.KeyContext.addr()
        }))
    }

    /// Дождаться пачки пакетов (до 64) в `out` (очищается); возвращает их
    /// число, 0 -- таймаут.
    pub(crate) fn wait_batch(
        &self,
        out: &mut Vec<Completion>,
        timeout: Option<Duration>,
    ) -> Result<usize> {
        out.clear();
        let mut entries = [const { MaybeUninit::uninit() }; BATCH_LEN];
        let removed = self.remove(&mut entries, timeout)?;
        out.extend(entries[..removed].iter().map(|entry| {
            // SAFETY: ядро заполнило первые `removed` элементов.
            let entry = unsafe { entry.assume_init_ref() };
            Completion {
                key: entry.KeyContext.addr(),
                tag: entry.ApcContext.addr(),
            }
        }));
        Ok(removed)
    }

    /// Поставить пакет в порт вручную (`NtSetIoCompletion`): пробуждение
    /// ждущего порт потока без объекта ядра.
    pub(crate) fn post(&self, key: usize, tag: usize) -> Result<()> {
        // SAFETY: порт -- живой дескриптор; ключ и метка -- непрозрачные
        // числа, ядро их не разыменовывает.
        let status = unsafe {
            NtSetIoCompletion(
                self.handle.raw(),
                ptr::without_provenance_mut(key),
                ptr::without_provenance_mut(tag),
                STATUS_SUCCESS,
                0,
            )
        };
        if status != STATUS_SUCCESS {
            return Err(status_to_error(status, "NtSetIoCompletion"));
        }
        Ok(())
    }

    /// Сколько раз ожидание порта возвращалось (тесты простоя).
    #[cfg(test)]
    pub(crate) fn removals(&self) -> u64 {
        self.removals.load(Ordering::Relaxed)
    }

    /// `NtRemoveIoCompletionEx` в `entries`; число заполненных элементов.
    fn remove(
        &self,
        entries: &mut [MaybeUninit<FILE_IO_COMPLETION_INFORMATION>],
        timeout: Option<Duration>,
    ) -> Result<usize> {
        let timeout = timeout.map(duration_to_nt_timeout);
        let timeout_ptr = timeout.as_ref().map_or(ptr::null(), ptr::from_ref);
        // Длина -- не больше `BATCH_LEN`, в `ULONG` помещается.
        let count = ULONG::try_from(entries.len()).unwrap_or(BATCH);
        let mut removed: ULONG = 0;
        // SAFETY: массив `entries` живёт до конца вызова и вмещает `count`
        // элементов (`MaybeUninit<T>` раскладкой совпадает с `T`); таймаут --
        // NULL (бесконечно) или указатель на локальное значение; без APC.
        let status = unsafe {
            NtRemoveIoCompletionEx(
                self.handle.raw(),
                entries.as_mut_ptr().cast(),
                count,
                &raw mut removed,
                timeout_ptr,
                0,
            )
        };
        // Relaxed: тестовая статистика, ничего не публикует.
        #[cfg(test)]
        self.removals.fetch_add(1, Ordering::Relaxed);
        match status {
            STATUS_SUCCESS => Ok((removed as usize).min(entries.len())),
            STATUS_TIMEOUT => Ok(0),
            _ => Err(status_to_error(status, "NtRemoveIoCompletionEx")),
        }
    }
}

/// Числовой handle -> `HANDLE` (без провенанса: это индекс таблицы ядра).
const fn raw_handle(handle: isize) -> HANDLE {
    ptr::without_provenance_mut(handle.cast_unsigned())
}

/// Пакет ожидания завершения: одна привязка объекта к порту за раз.
/// `Drop` отменяет привязку (вынимая уже поставленный в очередь пакет) и
/// закрывает дескриптор.
#[derive(Debug)]
pub(crate) struct WaitPacket {
    handle: Handle,
    already_signaled: bool,
}

// SAFETY: внутри -- NT-дескриптор пакета; привязку и отмену ядро
// синхронизирует само, закрывает дескриптор только `Drop` владельца.
unsafe impl Send for WaitPacket {}

impl WaitPacket {
    /// Новый непривязанный пакет.
    pub(crate) fn new() -> Result<Self> {
        let mut handle: HANDLE = ptr::null_mut();
        // SAFETY: out-параметр -- локальная переменная; атрибутов нет.
        let status =
            unsafe { NtCreateWaitCompletionPacket(&raw mut handle, GENERIC_ALL, ptr::null_mut()) };
        if status != STATUS_SUCCESS || handle.is_null() {
            return Err(status_to_error(status, "NtCreateWaitCompletionPacket"));
        }
        // SAFETY: свежий дескриптор успешного создания, владелец -- только он.
        let handle = unsafe { Handle::from_raw(handle) };
        Ok(Self {
            handle,
            already_signaled: false,
        })
    }

    /// Объект был взведён уже в момент последней привязки (`AlreadySignaled`):
    /// сигнал потреблён, пакет всё равно поставлен в порт.
    pub(crate) const fn already_signaled(&self) -> bool {
        self.already_signaled
    }

    /// Снять привязку в два шага (см. модуль): `RemoveSignaledPacket =
    /// FALSE` различает «не сработало» / «сработало, в очереди» / «уже
    /// извлечено»; во втором случае `TRUE` вынимает пакет из очереди.
    pub(crate) fn cancel(&self) -> CancelOutcome {
        self.cancel_for_reuse().0
    }

    /// [`cancel`](Self::cancel) и признак «пакет чист»: `true` -- после
    /// отмены пакет не привязан и не стоит в очереди порта, его можно
    /// привязать снова (пул реактора, привязка Multi). `false` -- код
    /// отмены неожиданный: пакет закрывают, а не переиспользуют.
    pub(crate) fn cancel_for_reuse(&self) -> (CancelOutcome, bool) {
        classify_cancel(self.cancel_raw(false), || self.cancel_raw(true))
    }

    /// Годен ли пакет к переиспользованию после отказа привязки с кодом
    /// `status`. Код сам по себе не доказывает, чей дескриптор негоден:
    /// `STATUS_INVALID_HANDLE` и `STATUS_OBJECT_TYPE_MISMATCH` ядро отвечает
    /// и на негодный пакет (проверено на x64 и i686). Поэтому нужны оба
    /// условия: код допускает вину объекта ожидания
    /// ([`arm_failure_may_name_target`]) **и** проба
    /// `NtCancelWaitCompletionPacket(FALSE)` отвечает `STATUS_CANCELLED` --
    /// дескриптор пакета годен, пакет не привязан и не в очереди порта.
    /// Иначе (в т.ч. `STATUS_INVALID_PARAMETER_1` -- пакет уже привязан)
    /// пакет закрывают: «грязный» пакет в пуле отказывал бы каждой
    /// следующей регистрации.
    pub(crate) fn survives_arm_failure(&self, status: NTSTATUS) -> bool {
        arm_failure_may_name_target(status) && self.cancel_raw(false) == STATUS_CANCELLED
    }

    /// Один вызов `NtCancelWaitCompletionPacket`.
    fn cancel_raw(&self, remove_signaled: bool) -> NTSTATUS {
        // SAFETY: дескриптор пакета жив, пока жив `self`.
        unsafe { NtCancelWaitCompletionPacket(self.handle.raw(), u8::from(remove_signaled)) }
    }
}

/// Отказ `NtAssociateWaitCompletionPacket` с этим кодом может быть виной
/// объекта ожидания (закрытый/негодный handle, не тот тип, нет доступа
/// `SYNCHRONIZE`, объект не ожидаемый -- порт). Прочие коды (в т.ч.
/// `STATUS_INVALID_PARAMETER_1` -- пакет) объект не называют.
const fn arm_failure_may_name_target(status: NTSTATUS) -> bool {
    matches!(
        status,
        STATUS_INVALID_HANDLE
            | STATUS_OBJECT_TYPE_MISMATCH
            | STATUS_ACCESS_DENIED
            | STATUS_INVALID_PARAMETER_3
    )
}

/// Коды двухшаговой отмены -> (исход, пакет чист). `second` -- второй шаг
/// (`RemoveSignaledPacket = TRUE`), вызывается только после
/// `STATUS_PENDING` первого.
fn classify_cancel(first: NTSTATUS, second: impl FnOnce() -> NTSTATUS) -> (CancelOutcome, bool) {
    match first {
        // Ожидание снято до сигнала.
        STATUS_SUCCESS => (CancelOutcome::NotFired, true),
        STATUS_PENDING => match second() {
            // Вынут из очереди.
            STATUS_SUCCESS => (CancelOutcome::FiredNotDelivered, true),
            // Между шагами пакет успел извлечь владелец порта.
            STATUS_CANCELLED => (CancelOutcome::AlreadyDelivered, true),
            // Иной код невозможен при живом дескрипторе: сигнал считаем
            // потреблённым, доставку несостоявшейся (недоставленный пакет
            // придёт и будет отброшен по ключу), а пакет -- «грязным».
            _ => (CancelOutcome::FiredNotDelivered, false),
        },
        // Уже извлечён / не привязывался.
        STATUS_CANCELLED => (CancelOutcome::AlreadyDelivered, true),
        // Иной код невозможен при живом дескрипторе; состояние пакета
        // неизвестно -- не переиспользовать.
        _ => (CancelOutcome::AlreadyDelivered, false),
    }
}

impl Drop for WaitPacket {
    fn drop(&mut self) {
        // Исход не важен: несработавшая привязка снята, стоявший в очереди
        // пакет вынут, а уже извлечённый отбросит владелец ключа.
        let _ = self.cancel();
    }
}

// ============================================================================
// Реактор
// ============================================================================

/// Живая регистрация: кого будить и чей пакет отменять.
#[derive(Debug)]
struct Entry {
    waker: Waker,
    packet: WaitPacket,
}

/// Слот таблицы: поколение растёт при каждом снятии регистрации.
#[derive(Debug, Default)]
struct Slot {
    generation: usize,
    entry: Option<Entry>,
}

/// Таблица регистраций: слоты + список свободных + пул простаивающих
/// пакетов (см. «Пул пакетов»).
#[derive(Debug, Default)]
struct Table {
    slots: Vec<Slot>,
    free: Vec<usize>,
    /// Не привязанные и не стоящие в очереди порта пакеты, не больше
    /// [`POOL_LIMIT`].
    pool: Vec<WaitPacket>,
}

/// Снятое потоком реактора за пачку -- разбирается вне блокировки.
#[derive(Debug, Default)]
struct Spent {
    /// Кого будить.
    wakers: Vec<Waker>,
    /// Пакеты сверх [`POOL_LIMIT`] -- закрыть.
    closing: Vec<WaitPacket>,
}

impl Spent {
    /// Пустой разбор под пачку `BATCH_LEN`.
    fn with_batch_capacity() -> Self {
        Self {
            wakers: Vec::with_capacity(BATCH_LEN),
            closing: Vec::with_capacity(BATCH_LEN),
        }
    }
}

impl Table {
    /// Вернуть простаивающий пакет в пул; сверх [`POOL_LIMIT`] -- пакет
    /// возвращается вызывающему, чтобы тот закрыл его вне блокировки.
    fn recycle(&mut self, packet: WaitPacket) -> Option<WaitPacket> {
        if self.pool.len() < POOL_LIMIT {
            self.pool.push(packet);
            None
        } else {
            Some(packet)
        }
    }

    /// Живая запись слота `index` поколения `generation`.
    fn entry_mut(&mut self, index: usize, generation: usize) -> Option<&mut Entry> {
        let slot = self.slots.get_mut(index)?;
        if slot.generation != generation {
            return None;
        }
        slot.entry.as_mut()
    }

    /// Снять регистрацию (срабатывание или `Drop`): запись -- вызывающему
    /// (будить и закрывать -- вне блокировки), поколение +1, слот свободен.
    fn take(&mut self, index: usize, generation: usize) -> Option<Entry> {
        let slot = self.slots.get_mut(index)?;
        if slot.generation != generation {
            return None;
        }
        let entry = slot.entry.take()?;
        slot.generation = slot.generation.wrapping_add(1);
        self.free.push(index);
        Some(entry)
    }
}

/// Реактор: порт завершения, таблица регистраций и один поток `xsr`,
/// который извлекает пакеты и будит `Waker`-ы.
///
/// Экземпляр живёт до конца процесса (`&'static`): поток реактора
/// бесконечно ждёт порт и не останавливается. Поток держит только
/// `&'static Reactor` -- никаких сильных ссылок на объекты пользователя,
/// кроме `Waker`, который вынут из таблицы и сейчас будится. Регистрации
/// (`Waker` + пакет) живут в таблице, пока владелец не снимет их (отмена,
/// `Drop`) или они не сработают. В крейте доступен только общий экземпляр
/// [`Reactor::global`]: реактор на канал -- лишний вечный поток.
#[derive(Debug)]
pub(crate) struct Reactor {
    port: CompletionPort,
    table: Mutex<Table>,
    /// Отброшенные устаревшие пакеты (слот снят или переиспользован).
    #[cfg(test)]
    stale: AtomicU64,
    /// Созданные `register`-ом пакеты (доказательство пула).
    #[cfg(test)]
    packets_created: AtomicU64,
    /// Инъекция отказа создания пакета (`register`) -- у своего
    /// экземпляра, чтобы не задевать параллельные тесты.
    #[cfg(test)]
    fail_create: AtomicBool,
    /// Разовая инъекция кода отказа привязки (`register`); `STATUS_SUCCESS`
    /// -- нет инъекции.
    #[cfg(test)]
    arm_fault: AtomicI32,
}

/// Общий реактор процесса.
static GLOBAL: OnceLock<&'static Reactor> = OnceLock::new();
/// Сериализация первого создания: неудача (`Err`) не кэшируется, а
/// повторное создание лишнего потока исключено.
static GLOBAL_INIT: Mutex<()> = Mutex::new(());

impl Reactor {
    /// Общий реактор процесса: создаётся (вместе с потоком) при первом
    /// вызове и живёт до конца процесса. Ошибка создания не кэшируется --
    /// следующий вызов попробует снова.
    pub(crate) fn global() -> Result<&'static Self> {
        if let Some(reactor) = GLOBAL.get() {
            return Ok(reactor);
        }
        let _guard = GLOBAL_INIT.lock_clean();
        if let Some(reactor) = GLOBAL.get() {
            return Ok(reactor);
        }
        let reactor = Self::start()?;
        Ok(GLOBAL.get_or_init(|| reactor))
    }

    /// Реактор без потока: порт и пустая таблица.
    fn unstarted() -> Result<Self> {
        Ok(Self {
            port: CompletionPort::new()?,
            table: Mutex::new(Table::default()),
            #[cfg(test)]
            stale: AtomicU64::new(0),
            #[cfg(test)]
            packets_created: AtomicU64::new(0),
            #[cfg(test)]
            fail_create: AtomicBool::new(false),
            #[cfg(test)]
            arm_fault: AtomicI32::new(STATUS_SUCCESS),
        })
    }

    /// Реактор со своим потоком (до конца процесса). Экземпляр уходит в
    /// поток в `Box` и становится `&'static` уже там: если поток не
    /// запустился (отказ `spawn` или паника хука старта), замыкание с
    /// `Box` уничтожается и порт закрывается -- ничего не утекает.
    fn start() -> Result<&'static Self> {
        let reactor = Box::new(Self::unstarted()?);
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        // Имя потока -- только в debug (короткий непрозрачный тег).
        let thread_name = String::from("xsr");
        #[cfg_attr(
            not(debug_assertions),
            expect(unused_mut, reason = "имя -- только в debug")
        )]
        let mut builder = thread::Builder::new();
        #[cfg(debug_assertions)]
        {
            builder = builder.name(thread_name);
        }
        #[cfg(not(debug_assertions))]
        drop(thread_name);
        // `JoinHandle` отпускается: поток живёт до конца процесса.
        crate::thread_hook::spawn(builder, move || {
            let reactor: &'static Self = Box::leak(reactor);
            // Получатель ждёт в `start` -- отправка не отказывает; поток
            // работает в любом случае.
            let _ = started_tx.send(reactor);
            reactor.run();
        })
        .map_err(|err| map_spawn_error(err, "spawn reactor thread"))?;
        started_rx.recv().map_err(|_| ShmError::WorkerPanicked {
            message: "reactor thread exited before start (thread start hook panicked)",
        })
    }

    /// Зарегистрировать одноразовое ожидание объекта `handle`: при его
    /// сигнале `waker` будится один раз, регистрация снимается.
    ///
    /// Регистрация -- это ожидание: сигнал автосбросного события она
    /// потребляет (в т.ч. уже взведённого -- тогда `waker` будится сразу,
    /// ровно один раз). Закрытый или негодный `handle` -- `Err`
    /// (`WindowsError`, контекст `NtAssociateWaitCompletionPacket`).
    ///
    /// Пакет берётся из пула (см. «Пул пакетов»); пул пуст -- создаётся.
    /// Отказ создания (`NtCreateWaitCompletionPacket`) -- `Err`, таблица и
    /// сигнал объекта не тронуты. Отказ привязки, в котором не доказана
    /// вина объекта ([`WaitPacket::survives_arm_failure`]), закрывает пакет;
    /// если он был из пула -- один повтор с новым пакетом, а не следующим
    /// из пула: отказавшая привязка не усложняет перебор пула.
    pub(crate) fn register(&'static self, handle: isize, waker: Waker) -> Result<Registration> {
        // Пакет из пула -- под блокировкой; новый создаём вне её. Привязка
        // -- под блокировкой: пакет может прийти сразу, и поток реактора
        // обязан найти запись в таблице.
        let mut table = self.table.lock_clean();
        let mut pooled = table.pool.pop();
        loop {
            let from_pool = pooled.is_some();
            let mut packet = match pooled.take() {
                Some(packet) => packet,
                None => {
                    drop(table);
                    let packet = self.new_packet()?;
                    table = self.table.lock_clean();
                    packet
                }
            };
            let index = table.free.pop().unwrap_or(table.slots.len());
            if index == table.slots.len() {
                table.slots.push(Slot::default());
            }
            // Индексация не паникует: индекс -- из списка свободных (слоты
            // не удаляются) или только что добавленного слота.
            let generation = table.slots[index].generation;
            let status = self.associate(&mut packet, raw_handle(handle), index, generation);
            if status == STATUS_SUCCESS {
                table.slots[index].entry = Some(Entry { waker, packet });
                drop(table);
                return Ok(Registration {
                    reactor: self,
                    index,
                    generation,
                });
            }
            // Привязки не было -- пакета с этим ключом в порту нет,
            // поколение менять незачем.
            table.free.push(index);
            drop(table);
            let err = status_to_error(status, "NtAssociateWaitCompletionPacket");
            // Проба пакета -- системный вызов, вне блокировки.
            if packet.survives_arm_failure(status) {
                // Виноват объект ожидания, пакет чист -- в пул.
                let overflow = self.table.lock_clean().recycle(packet);
                drop(overflow);
                return Err(err);
            }
            // Пакет негоден (или его вина не исключена) -- закрыть.
            drop(packet);
            if !from_pool {
                return Err(err);
            }
            // Негодным оказался пакет из пула: ровно один повтор -- с новым
            // пакетом (`pooled` пуст), второй отказ возвращается как есть.
            table = self.table.lock_clean();
        }
    }

    /// Привязка пакета регистрации (в тестах -- с разовой инъекцией кода
    /// отказа, `arm_fault`).
    fn associate(
        &self,
        packet: &mut WaitPacket,
        target: HANDLE,
        key: usize,
        tag: usize,
    ) -> NTSTATUS {
        #[cfg(test)]
        {
            // Relaxed: тестовая инъекция, ничего не публикует.
            let fault = self.arm_fault.swap(STATUS_SUCCESS, Ordering::Relaxed);
            if fault != STATUS_SUCCESS {
                return fault;
            }
        }
        self.port.arm_status(packet, target, key, tag)
    }

    /// Новый пакет для регистрации (в тестах -- со счётчиком и инъекцией
    /// отказа).
    fn new_packet(&self) -> Result<WaitPacket> {
        #[cfg(test)]
        {
            /// `STATUS_INSUFFICIENT_RESOURCES` -- типичный отказ под давлением.
            const INJECTED: NTSTATUS = 0xC000_009A_u32.cast_signed();
            // Relaxed: тестовые флаг и статистика, ничего не публикуют.
            if self.fail_create.load(Ordering::Relaxed) {
                return Err(status_to_error(INJECTED, "NtCreateWaitCompletionPacket"));
            }
            self.packets_created.fetch_add(1, Ordering::Relaxed);
        }
        #[cfg(not(test))]
        let _ = self;
        WaitPacket::new()
    }

    /// Цикл потока реактора: бесконечное ожидание порта и обработка пачек.
    fn run(&self) {
        let mut batch = Vec::with_capacity(BATCH_LEN);
        let mut spent = Spent::with_batch_capacity();
        loop {
            if self.port.wait_batch(&mut batch, None).is_err() {
                thread::sleep(ERROR_BACKOFF);
                continue;
            }
            self.process(&batch, &mut spent);
        }
    }

    /// Пачка пакетов -> записи из таблицы и возврат их пакетов в пул (за
    /// один захват блокировки) -> пробуждение и закрытие лишних пакетов
    /// (без неё).
    fn process(&self, batch: &[Completion], spent: &mut Spent) {
        {
            let mut table = self.table.lock_clean();
            for completion in batch {
                match table.take(completion.key, completion.tag) {
                    Some(Entry { waker, packet }) => {
                        spent.wakers.push(waker);
                        // Завершение с живым ключом -- пакет этой записи
                        // извлечён из порта: он не привязан и не в очереди.
                        if let Some(packet) = table.recycle(packet) {
                            spent.closing.push(packet);
                        }
                    }
                    // Устаревший пакет: регистрация уже снята (и, возможно,
                    // слот занят новой) -- отбрасываем.
                    #[cfg(test)]
                    None => {
                        // Relaxed: тестовая статистика.
                        self.stale.fetch_add(1, Ordering::Relaxed);
                    }
                    #[cfg(not(test))]
                    None => {}
                }
            }
        }
        for waker in spent.wakers.drain(..) {
            // Паника чужого `Waker` не должна убить общий поток.
            drop(catch_unwind(AssertUnwindSafe(|| waker.wake())));
        }
        // Пакеты извлечены -- отмена в `Drop` ничего не найдёт, остаётся
        // закрыть дескриптор.
        spent.closing.clear();
    }

    /// Сколько пакетов создано `register`-ом.
    #[cfg(test)]
    fn packets_created(&self) -> u64 {
        self.packets_created.load(Ordering::Relaxed)
    }

    /// Сколько пакетов простаивает в пуле.
    #[cfg(test)]
    fn pooled(&self) -> usize {
        self.table.lock_clean().pool.len()
    }

    /// Сколько устаревших пакетов отброшено.
    #[cfg(test)]
    fn stale_completions(&self) -> u64 {
        self.stale.load(Ordering::Relaxed)
    }
}

/// Одноразовая регистрация ожидания в [`Reactor`]. Сработала -- снята
/// реактором (`Waker` будится ровно один раз). Несработавшую снимает
/// [`Registration::cancel`] (с исходом) или `Drop` (исход отбрасывается):
/// двухшаговая отмена пакета, затем его закрытие.
///
/// После возврата из отмены или `Drop` возможно не более одного уже
/// начатого пробуждения (реактор вынул запись и будит `Waker` вне
/// блокировки) -- ровно случай `CancelOutcome::AlreadyDelivered`; контракт
/// `Waker` допускает такое ложное пробуждение.
#[derive(Debug)]
pub(crate) struct Registration {
    reactor: &'static Reactor,
    index: usize,
    generation: usize,
}

impl Registration {
    /// Сработала ли (снята реактором).
    pub(crate) fn is_fired(&self) -> bool {
        let mut table = self.reactor.table.lock_clean();
        table.entry_mut(self.index, self.generation).is_none()
    }

    /// Заменить `Waker` несработавшей регистрации (пропуская замену на
    /// эквивалентный, `will_wake`). `false` -- уже сработала: будить не
    /// будут, результат готов.
    pub(crate) fn set_waker(&self, waker: &Waker) -> bool {
        {
            let mut table = self.reactor.table.lock_clean();
            match table.entry_mut(self.index, self.generation) {
                None => return false,
                Some(entry) if entry.waker.will_wake(waker) => return true,
                Some(_) => {}
            }
        }
        // Клонирование и `Drop` старого -- код пользователя: вне блокировки.
        let mut waker = waker.clone();
        {
            let mut table = self.reactor.table.lock_clean();
            match table.entry_mut(self.index, self.generation) {
                None => return false,
                Some(entry) => std::mem::swap(&mut entry.waker, &mut waker),
            }
        }
        drop(waker);
        true
    }

    /// Снять регистрацию и узнать, что стало с сигналом.
    ///
    /// - `NotFired` -- сигнал объекта цел, `Waker` не будится;
    /// - `FiredNotDelivered` -- сигнал потреблён, `Waker` не будится: у
    ///   автосбросного события владелец возвращает сигнал (`NtSetEvent`) или
    ///   перепроверяет состояние (кольцо) сам -- async API, Task 16;
    /// - `AlreadyDelivered` -- реактор уже снял регистрацию, `Waker` разбужен
    ///   или вот-вот будет (ровно один раз).
    pub(crate) fn cancel(self) -> CancelOutcome {
        let outcome = self.cancel_inner();
        // Снята -- повторный проход `Drop` не нужен (ресурсов у самой
        // регистрации нет: ссылка и два числа).
        std::mem::forget(self);
        outcome
    }

    /// Снятие записи из таблицы и отмена её пакета (вне блокировки).
    fn cancel_inner(&self) -> CancelOutcome {
        let entry = self
            .reactor
            .table
            .lock_clean()
            .take(self.index, self.generation);
        let Some(Entry { waker, packet }) = entry else {
            return CancelOutcome::AlreadyDelivered;
        };
        // Запись снята нами -- реактор её не найдёт и не разбудит. Пакет,
        // извлечённый им до снятия (`AlreadyDelivered` на уровне пакета),
        // будет отброшен как устаревший: сигнал потреблён, не доставлен.
        let (outcome, clean) = packet.cancel_for_reuse();
        let outcome = match outcome {
            CancelOutcome::NotFired => CancelOutcome::NotFired,
            CancelOutcome::FiredNotDelivered | CancelOutcome::AlreadyDelivered => {
                CancelOutcome::FiredNotDelivered
            }
        };
        // Чистый пакет (не привязан, не в очереди порта; завершение,
        // извлечённое реактором до снятия записи, несёт старое поколение и
        // будет отброшено) -- в пул. «Грязный» и лишний сверх предела --
        // закрыть. Закрытие и `Drop` `Waker` (код пользователя) -- вне
        // блокировки.
        let closing = if clean {
            self.reactor.table.lock_clean().recycle(packet)
        } else {
            Some(packet)
        };
        drop(closing);
        drop(waker);
        outcome
    }

    /// Ключ пакета регистрации (тесты гонки отмены).
    #[cfg(test)]
    const fn key(&self) -> Completion {
        Completion {
            key: self.index,
            tag: self.generation,
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let _ = self.cancel_inner();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::win::EventHandle;
    use std::future::Future;
    use std::pin::{Pin, pin};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Wake, Waker};
    use std::thread::{self, Thread, ThreadId};
    use std::time::{Duration, Instant};

    /// Негоден первый параметр (`NtAssociateWaitCompletionPacket` -- пакет,
    /// например уже привязанный); проверено на x64 и i686.
    const STATUS_INVALID_PARAMETER_1: NTSTATUS = 0xC000_00EF_u32.cast_signed();

    /// Щедрый дедлайн: шумный хост не должен ронять тесты.
    const DEADLINE: Duration = Duration::from_secs(10);

    /// Waker-счётчик: сколько раз и на каком потоке разбудили.
    struct Counter {
        count: AtomicUsize,
        waiter: Thread,
        woke_on: Mutex<Vec<(ThreadId, Option<String>)>>,
    }

    impl Counter {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                count: AtomicUsize::new(0),
                waiter: thread::current(),
                woke_on: Mutex::new(Vec::new()),
            })
        }

        fn get(&self) -> usize {
            self.count.load(Ordering::SeqCst)
        }

        fn waker(self: &Arc<Self>) -> Waker {
            Waker::from(Arc::clone(self))
        }
    }

    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            let current = thread::current();
            self.woke_on
                .lock()
                .unwrap()
                .push((current.id(), current.name().map(String::from)));
            self.count.fetch_add(1, Ordering::SeqCst);
            self.waiter.unpark();
        }
    }

    /// Ждать условия до дедлайна (парковка с таймаутом -- только чтобы не
    /// зависнуть, если пробуждение потеряно; условие проверяется каждый раз).
    fn wait_until(cond: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + DEADLINE;
        while !cond() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            thread::park_timeout(left.min(Duration::from_millis(50)));
        }
        true
    }

    /// Барьер реактора: всё, что попало в порт раньше, уже обработано.
    /// Пакеты IOCP извлекаются по FIFO, а поток реактора один.
    fn sync(reactor: &'static Reactor) {
        let marker = EventHandle::create_unnamed(false).unwrap();
        marker.set().unwrap();
        let counter = Counter::new();
        let registration = reactor
            .register(marker.raw_handle(), counter.waker())
            .unwrap();
        assert!(wait_until(|| counter.get() == 1), "барьер не сработал");
        drop(registration);
    }

    /// Минимальный исполнитель: `Wake` + `thread::park` + `pin!`.
    fn block_on<F: Future>(future: F) -> Option<F::Output> {
        struct Unpark(Thread);
        impl Wake for Unpark {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        let waker = Waker::from(Arc::new(Unpark(thread::current())));
        let mut cx = Context::from_waker(&waker);
        let mut future = pin!(future);
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
                return Some(value);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            thread::park_timeout(left);
        }
    }

    /// Future «событие сработало» поверх реактора (прообраз Task 16).
    struct EventWait<'a> {
        reactor: &'static Reactor,
        event: &'a EventHandle,
        registration: Option<Registration>,
        polls: usize,
    }

    impl Future for EventWait<'_> {
        type Output = usize;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<usize> {
            let this = self.get_mut();
            this.polls += 1;
            match &this.registration {
                None => {
                    let registration = this
                        .reactor
                        .register(this.event.raw_handle(), cx.waker().clone())
                        .unwrap();
                    this.registration = Some(registration);
                    Poll::Pending
                }
                Some(registration) => {
                    if registration.set_waker(cx.waker()) {
                        Poll::Pending
                    } else {
                        Poll::Ready(this.polls)
                    }
                }
            }
        }
    }

    #[test]
    fn wakes_on_event() {
        let reactor = Reactor::global().unwrap();
        let event = Arc::new(EventHandle::create_unnamed(false).unwrap());
        let setter = {
            let event = Arc::clone(&event);
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(20));
                event.set().unwrap();
            })
        };
        let polls = block_on(EventWait {
            reactor,
            event: &event,
            registration: None,
            polls: 0,
        });
        setter.join().unwrap();
        assert!(polls.is_some(), "future не разбужен до дедлайна");
        // Регистрация -- это ожидание: автосбросное событие погашено.
        assert!(!event.wait(Some(Duration::ZERO)).unwrap());
        // Тот же `&'static` при повторном вызове.
        assert!(ptr::eq(reactor, Reactor::global().unwrap()));
    }

    #[test]
    fn cancelled_registration_never_wakes() {
        let reactor = Reactor::start().unwrap();
        let event = EventHandle::create_unnamed(false).unwrap();
        let counter = Counter::new();
        let registration = reactor
            .register(event.raw_handle(), counter.waker())
            .unwrap();
        assert!(!registration.is_fired());
        drop(registration);
        event.set().unwrap();
        sync(reactor);
        assert_eq!(counter.get(), 0, "отменённая регистрация разбудила waker");
        // Отменённое до сигнала ожидание сигнал не потребляет.
        assert!(event.wait(Some(Duration::ZERO)).unwrap());
    }

    #[test]
    fn idle_reactor_does_not_wake_up() {
        let reactor = Reactor::start().unwrap();
        let events: Vec<_> = (0..4)
            .map(|_| EventHandle::create_unnamed(false).unwrap())
            .collect();
        let counter = Counter::new();
        let registrations: Vec<_> = events
            .iter()
            .map(|event| {
                reactor
                    .register(event.raw_handle(), counter.waker())
                    .unwrap()
            })
            .collect();
        let before = reactor.port.removals();
        thread::sleep(Duration::from_millis(200));
        assert_eq!(
            reactor.port.removals() - before,
            0,
            "реактор просыпался в простое"
        );
        assert_eq!(before, 0);
        assert_eq!(counter.get(), 0);
        drop(registrations);
    }

    #[test]
    fn thousand_registrations_one_thread() {
        const N: usize = 1000;
        let reactor = Reactor::start().unwrap();
        let events: Vec<_> = (0..N)
            .map(|_| EventHandle::create_unnamed(false).unwrap())
            .collect();
        let counter = Counter::new();
        let registrations: Vec<_> = events
            .iter()
            .map(|event| {
                reactor
                    .register(event.raw_handle(), counter.waker())
                    .unwrap()
            })
            .collect();
        for event in &events {
            event.set().unwrap();
        }
        assert!(
            wait_until(|| counter.get() == N),
            "разбужено {} из {N}",
            counter.get()
        );
        assert!(registrations.iter().all(Registration::is_fired));
        let woke_on = counter.woke_on.lock().unwrap();
        let first = &woke_on[0];
        assert!(
            woke_on.iter().all(|w| w.0 == first.0),
            "больше одного потока"
        );
        assert_ne!(first.0, thread::current().id());
        #[cfg(debug_assertions)]
        assert_eq!(first.1.as_deref(), Some("xsr"));
        #[cfg(not(debug_assertions))]
        assert_eq!(first.1, None);
        drop(woke_on);
        sync(reactor);
        assert_eq!(counter.get(), N, "лишние пробуждения");
    }

    /// Постановление 1: устаревший ключ (регистрация снята, слот занят
    /// новой) -- молча отбрасывается. Гонку отмены с уже стоящим в очереди
    /// пакетом воспроизводим детерминированно: ставим в порт пакет со
    /// старым ключом руками (`post`), ровно как его оставило бы ядро.
    #[test]
    fn stale_completion_is_dropped() {
        let reactor = Reactor::start().unwrap();
        let first_event = EventHandle::create_unnamed(false).unwrap();
        let second_event = EventHandle::create_unnamed(false).unwrap();
        let first = Counter::new();
        let second = Counter::new();
        let registration = reactor
            .register(first_event.raw_handle(), first.waker())
            .unwrap();
        let stale = registration.key();
        drop(registration);
        let registration = reactor
            .register(second_event.raw_handle(), second.waker())
            .unwrap();
        let fresh = registration.key();
        assert_eq!(fresh.key, stale.key, "слот переиспользован");
        assert_ne!(fresh.tag, stale.tag, "поколение сменилось");

        let stale_before = reactor.stale_completions();
        reactor.port.post(stale.key, stale.tag).unwrap();
        reactor.port.post(usize::MAX, 0).unwrap();
        sync(reactor);
        assert_eq!(reactor.stale_completions() - stale_before, 2);
        assert_eq!(first.get(), 0);
        assert_eq!(second.get(), 0, "устаревший пакет разбудил чужой waker");
        assert!(!registration.is_fired());

        second_event.set().unwrap();
        assert!(wait_until(|| second.get() == 1));
        sync(reactor);
        assert_eq!(second.get(), 1);
    }

    /// Постановление 1, живая гонка: сигнал и сразу `Drop`. Порядок не
    /// управляем (поток реактора может успеть или нет), поэтому проверяем
    /// честный инвариант: waker разбужен не более одного раза -- и до
    /// `Drop`, и после него (одно уже начатое пробуждение допустимо).
    #[test]
    fn drop_racing_completion_wakes_at_most_once() {
        let reactor = Reactor::start().unwrap();
        let event = EventHandle::create_unnamed(false).unwrap();
        let mut seen = Vec::new();
        for _ in 0..300 {
            let counter = Counter::new();
            let registration = reactor
                .register(event.raw_handle(), counter.waker())
                .unwrap();
            event.set().unwrap();
            drop(registration);
            seen.push(counter);
            // Сигнал, не доставленный из-за отмены, гасим к следующему кругу.
            let _ = event.reset();
        }
        sync(reactor);
        for counter in &seen {
            assert!(counter.get() <= 1, "больше одного пробуждения");
        }
    }

    /// Постановление 2: событие уже взведено до регистрации -- waker
    /// будится ровно один раз, сигнал автосбросного события потреблён.
    #[test]
    fn already_signaled_wakes_exactly_once() {
        let reactor = Reactor::start().unwrap();
        let event = EventHandle::create_unnamed(false).unwrap();
        event.set().unwrap();
        let counter = Counter::new();
        let registration = reactor
            .register(event.raw_handle(), counter.waker())
            .unwrap();
        assert!(wait_until(|| counter.get() >= 1));
        sync(reactor);
        assert_eq!(counter.get(), 1);
        assert!(registration.is_fired());
        assert!(!registration.set_waker(&counter.waker()));
        assert!(
            !event.wait(Some(Duration::ZERO)).unwrap(),
            "сигнал не потреблён"
        );
    }

    /// Постановление 2 на уровне ядра: что делает `AlreadySignaled`.
    #[test]
    fn port_already_signaled_packet_is_queued() {
        let port = CompletionPort::new().unwrap();
        let event = EventHandle::create_unnamed(false).unwrap();
        event.set().unwrap();
        let packet = port.associate_raw(event.raw_handle(), 7, 3).unwrap();
        assert!(packet.already_signaled());
        // Пакет всё равно стоит в очереди: ждать его можно без таймаута.
        let mut batch = Vec::new();
        assert_eq!(port.wait_batch(&mut batch, Some(DEADLINE)).unwrap(), 1);
        assert_eq!(batch, [Completion { key: 7, tag: 3 }]);
        assert!(!event.wait(Some(Duration::ZERO)).unwrap());
    }

    /// Коды `NtCancelWaitCompletionPacket` по состояниям пакета.
    #[test]
    fn port_cancel_outcomes() {
        let port = CompletionPort::new().unwrap();
        let event = EventHandle::create_unnamed(false).unwrap();

        // Сырые коды первого шага (`RemoveSignaledPacket = FALSE`) -- то, на
        // чём стоит двухшаговая отмена; проверяются и на i686.
        let packet = port.associate_raw(event.raw_handle(), 10, 0).unwrap();
        assert_eq!(packet.cancel_raw(false), STATUS_SUCCESS, "не сработало");
        let packet = port.associate_raw(event.raw_handle(), 11, 0).unwrap();
        event.set().unwrap();
        assert_eq!(packet.cancel_raw(false), STATUS_PENDING, "в очереди");
        assert_eq!(packet.cancel_raw(true), STATUS_SUCCESS, "вынут");
        assert_eq!(packet.cancel_raw(false), STATUS_CANCELLED, "снимать нечего");
        assert_eq!(port.wait(Some(Duration::from_millis(50))).unwrap(), None);

        // Не привязывался -- снимать нечего.
        let fresh = WaitPacket::new().unwrap();
        assert_eq!(fresh.cancel(), CancelOutcome::AlreadyDelivered);

        // Ожидание не сработало -- отмена снимает его, сигнал цел.
        let packet = port.associate_raw(event.raw_handle(), 1, 0).unwrap();
        assert!(!packet.already_signaled());
        assert_eq!(packet.cancel(), CancelOutcome::NotFired);
        assert_eq!(packet.cancel(), CancelOutcome::AlreadyDelivered);
        event.set().unwrap();
        assert!(
            event.wait(Some(Duration::ZERO)).unwrap(),
            "сигнал не потреблён"
        );

        // Сработало и стоит в очереди -- `PENDING`, затем `TRUE` вынимает
        // пакет; сигнал потреблён.
        let packet = port.associate_raw(event.raw_handle(), 2, 0).unwrap();
        event.set().unwrap();
        assert!(
            !event.wait(Some(Duration::ZERO)).unwrap(),
            "сигнал потреблён"
        );
        assert_eq!(packet.cancel(), CancelOutcome::FiredNotDelivered);
        assert_eq!(port.wait(Some(Duration::from_millis(50))).unwrap(), None);

        // Пакет уже извлечён -- отменять нечего; сработавший пакет можно
        // привязать снова, дважды привязанный -- нельзя.
        let mut packet = port.associate_raw(event.raw_handle(), 3, 0).unwrap();
        event.set().unwrap();
        assert_eq!(port.wait(Some(DEADLINE)).unwrap(), Some(3));
        assert_eq!(packet.cancel(), CancelOutcome::AlreadyDelivered);
        let target = raw_handle(event.raw_handle());
        port.arm(&mut packet, target, 4, 0).unwrap();
        assert!(port.arm(&mut packet, target, 5, 0).is_err());
        event.set().unwrap();
        assert_eq!(port.wait(Some(DEADLINE)).unwrap(), Some(4));
    }

    #[test]
    fn port_wait_times_out_and_returns_posted_key() {
        let port = CompletionPort::new().unwrap();
        assert_eq!(port.wait(Some(Duration::from_millis(10))).unwrap(), None);
        port.post(42, 0).unwrap();
        assert_eq!(port.wait(Some(DEADLINE)).unwrap(), Some(42));
        // Привязка по заимствованному handle (путь Multi, Task 17).
        let event = EventHandle::create_unnamed(false).unwrap();
        let packet = port
            .associate(std::os::windows::io::AsHandle::as_handle(&event), 9)
            .unwrap();
        event.set().unwrap();
        assert_eq!(port.wait(Some(DEADLINE)).unwrap(), Some(9));
        drop(packet);
        assert_eq!(port.removals(), 3);
    }

    /// Постановление 4: закрытый/негодный handle -- `Err`, не паника.
    ///
    /// Значение только что закрытого handle при параллельном прогоне может
    /// сразу занять объект другого теста (таблица handle раздаёт
    /// освобождённые значения LIFO), и регистрация законно удалась бы --
    /// а то и съела бы чужой сигнал. Поэтому проверяем значения, которые
    /// заведомо не заняты: NULL и записи таблицы далеко за её реальным
    /// размером (для ядра закрытый handle -- ровно такая свободная запись).
    /// Ядро на закрытом handle проверено отдельно: `STATUS_INVALID_HANDLE`
    /// (отчёт T15). Принимается любой `Err`.
    #[test]
    fn closed_handle_is_error() {
        let reactor = Reactor::start().unwrap();
        let counter = Counter::new();
        // Свой закрытый объект: его значение не используем (см. выше), но
        // проверяемые значения выбраны выше любого выданного.
        let closed = EventHandle::create_unnamed(false).unwrap();
        let ceiling = closed.raw_handle();
        drop(closed);
        for handle in [0, 0x00FF_FFF0, 0x03FF_FFF0] {
            assert!(handle == 0 || handle > ceiling);
            let result = reactor.register(handle, counter.waker());
            assert!(result.is_err(), "handle {handle:#x}: {result:?}");
        }
        sync(reactor);
        assert_eq!(counter.get(), 0);
        // Неудачные регистрации вернули слот: таблица не растёт.
        assert_eq!(reactor.table.lock_clean().slots.len(), 1);
    }

    /// Реактор без потока: тест сам решает, когда извлекать пакеты.
    fn manual_reactor() -> &'static Reactor {
        Box::leak(Box::new(Reactor::unstarted().unwrap()))
    }

    /// Один оборот цикла реактора вручную.
    fn turn(reactor: &Reactor) -> usize {
        let mut batch = Vec::new();
        let mut spent = Spent::default();
        let count = reactor
            .port
            .wait_batch(&mut batch, Some(Duration::from_millis(100)))
            .unwrap();
        reactor.process(&batch, &mut spent);
        count
    }

    /// Отмена несработавшей регистрации -- `NotFired`, сигнал цел.
    #[test]
    fn registration_cancel_not_fired() {
        let reactor = manual_reactor();
        let event = EventHandle::create_unnamed(false).unwrap();
        let counter = Counter::new();
        let registration = reactor
            .register(event.raw_handle(), counter.waker())
            .unwrap();
        assert_eq!(registration.cancel(), CancelOutcome::NotFired);
        event.set().unwrap();
        assert_eq!(turn(reactor), 0, "отменённый пакет попал в порт");
        assert_eq!(counter.get(), 0);
        assert!(event.wait(Some(Duration::ZERO)).unwrap(), "сигнал цел");
    }

    /// Регистрация сработала, пакет в очереди, реактор его ещё не извлёк --
    /// `FiredNotDelivered` (путь `STATUS_PENDING` -> `TRUE`), пакет вынут,
    /// waker не будится, сигнал потреблён.
    #[test]
    fn registration_cancel_fired_not_delivered() {
        let reactor = manual_reactor();
        let event = EventHandle::create_unnamed(false).unwrap();
        let counter = Counter::new();
        let registration = reactor
            .register(event.raw_handle(), counter.waker())
            .unwrap();
        event.set().unwrap();
        assert!(
            !event.wait(Some(Duration::ZERO)).unwrap(),
            "сигнал потреблён"
        );
        assert_eq!(registration.cancel(), CancelOutcome::FiredNotDelivered);
        assert_eq!(turn(reactor), 0, "пакет не вынут из очереди");
        assert_eq!(counter.get(), 0);
    }

    /// Пакет уже извлечён реактором, но запись снята отменой
    /// раньше, чем он дошёл до таблицы -- `FiredNotDelivered`, пакет
    /// отброшен как устаревший, waker не будится.
    #[test]
    fn registration_cancel_after_dequeue_before_lookup() {
        let reactor = manual_reactor();
        let event = EventHandle::create_unnamed(false).unwrap();
        let counter = Counter::new();
        let registration = reactor
            .register(event.raw_handle(), counter.waker())
            .unwrap();
        event.set().unwrap();
        let mut batch = Vec::new();
        assert_eq!(
            reactor.port.wait_batch(&mut batch, Some(DEADLINE)).unwrap(),
            1
        );
        assert_eq!(registration.cancel(), CancelOutcome::FiredNotDelivered);
        reactor.process(&batch, &mut Spent::default());
        assert_eq!(reactor.stale_completions(), 1);
        assert_eq!(counter.get(), 0);
    }

    /// Реактор уже доставил -- `AlreadyDelivered`, waker
    /// разбужен ровно один раз.
    #[test]
    fn registration_cancel_already_delivered() {
        let reactor = manual_reactor();
        let event = EventHandle::create_unnamed(false).unwrap();
        let counter = Counter::new();
        let registration = reactor
            .register(event.raw_handle(), counter.waker())
            .unwrap();
        event.set().unwrap();
        assert_eq!(turn(reactor), 1);
        assert_eq!(counter.get(), 1);
        assert_eq!(registration.cancel(), CancelOutcome::AlreadyDelivered);
        assert_eq!(counter.get(), 1);
    }

    /// Живая гонка отмены с потоком реактора: исход отмены точно говорит,
    /// будет ли пробуждение (`AlreadyDelivered` -- ровно одно, иначе --
    /// ни одного).
    #[test]
    fn cancel_outcome_matches_delivery() {
        let reactor = Reactor::start().unwrap();
        let event = EventHandle::create_unnamed(false).unwrap();
        let mut seen = Vec::new();
        for _ in 0..300 {
            let counter = Counter::new();
            let registration = reactor
                .register(event.raw_handle(), counter.waker())
                .unwrap();
            event.set().unwrap();
            seen.push((registration.cancel(), counter));
            let _ = event.reset();
        }
        sync(reactor);
        for (outcome, counter) in &seen {
            let expected = usize::from(*outcome == CancelOutcome::AlreadyDelivered);
            assert_eq!(counter.get(), expected, "{outcome:?}");
            assert_ne!(
                *outcome,
                CancelOutcome::NotFired,
                "событие взведено до отмены"
            );
        }
    }

    /// Оборот без ожидания: только то, что уже стоит в порту.
    fn turn_quick(reactor: &Reactor) -> usize {
        let mut batch = Vec::new();
        let count = reactor
            .port
            .wait_batch(&mut batch, Some(Duration::ZERO))
            .unwrap();
        reactor.process(&batch, &mut Spent::default());
        count
    }

    /// Пул: в установившемся режиме регистрации не создают пакетов --
    /// ни после отмены, ни после штатного срабатывания.
    #[test]
    fn registrations_reuse_pooled_packets() {
        let reactor = manual_reactor();
        let event = EventHandle::create_unnamed(false).unwrap();
        let counter = Counter::new();
        for _ in 0..1000 {
            let registration = reactor
                .register(event.raw_handle(), counter.waker())
                .unwrap();
            assert_eq!(registration.cancel(), CancelOutcome::NotFired);
        }
        assert_eq!(reactor.packets_created(), 1);
        for round in 1..=1000 {
            let registration = reactor
                .register(event.raw_handle(), counter.waker())
                .unwrap();
            event.set().unwrap();
            assert_eq!(turn(reactor), 1);
            assert_eq!(counter.get(), round, "ровно одно пробуждение");
            assert!(registration.is_fired());
            drop(registration);
        }
        assert_eq!(turn_quick(reactor), 0, "лишних пакетов в порту нет");
        assert_eq!(counter.get(), 1000);
        assert_eq!(reactor.packets_created(), 1);
        assert_eq!(reactor.stale_completions(), 0);
    }

    /// Пул ограничен `POOL_LIMIT`: лишние пакеты закрываются -- и при
    /// отмене, и при пачке срабатываний.
    #[test]
    fn pool_is_bounded() {
        const N: usize = 200;
        let reactor = manual_reactor();
        let events: Vec<_> = (0..N)
            .map(|_| EventHandle::create_unnamed(false).unwrap())
            .collect();
        let counter = Counter::new();
        let register_all = || -> Vec<Registration> {
            events
                .iter()
                .map(|event| {
                    reactor
                        .register(event.raw_handle(), counter.waker())
                        .unwrap()
                })
                .collect()
        };
        let registrations = register_all();
        assert_eq!(reactor.packets_created(), 200);
        for registration in registrations {
            assert_eq!(registration.cancel(), CancelOutcome::NotFired);
        }
        assert_eq!(reactor.pooled(), POOL_LIMIT);
        assert_eq!(reactor.packets_created(), 200);
        let registrations = register_all();
        assert_eq!(reactor.pooled(), 0);
        assert_eq!(reactor.packets_created(), 400 - POOL_LIMIT as u64);
        for event in &events {
            event.set().unwrap();
        }
        let mut delivered = 0;
        while delivered < N {
            let got = turn(reactor);
            assert_ne!(got, 0, "доставлено {delivered} из {N}");
            delivered += got;
        }
        assert_eq!(counter.get(), N);
        assert_eq!(reactor.pooled(), POOL_LIMIT);
        drop(registrations);
        assert_eq!(reactor.pooled(), POOL_LIMIT);
    }

    /// Исход прежней регистрации, после которого её пакет ушёл в пул.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Scenario {
        /// Отмена до сигнала (`STATUS_SUCCESS`).
        NotFired,
        /// Сработала, пакет вынут из очереди отменой (`PENDING` -> `TRUE`).
        FiredNotDelivered,
        /// Пакет извлечён реактором до поиска в таблице, отмена сняла
        /// запись раньше (`STATUS_CANCELLED`): завершение «в полёте».
        DequeuedBeforeLookup,
        /// Штатное срабатывание и доставка.
        Delivered,
    }

    fn run_scenario(scenario: Scenario) {
        let reactor = manual_reactor();
        let first = EventHandle::create_unnamed(false).unwrap();
        let second = EventHandle::create_unnamed(false).unwrap();
        let old = Counter::new();
        let new = Counter::new();
        let registration = reactor.register(first.raw_handle(), old.waker()).unwrap();
        let mut in_flight = Vec::new();
        match scenario {
            Scenario::NotFired => {
                assert_eq!(registration.cancel(), CancelOutcome::NotFired);
            }
            Scenario::FiredNotDelivered => {
                first.set().unwrap();
                assert_eq!(registration.cancel(), CancelOutcome::FiredNotDelivered);
            }
            Scenario::DequeuedBeforeLookup => {
                first.set().unwrap();
                let got = reactor
                    .port
                    .wait_batch(&mut in_flight, Some(DEADLINE))
                    .unwrap();
                assert_eq!(got, 1);
                assert_eq!(registration.cancel(), CancelOutcome::FiredNotDelivered);
            }
            Scenario::Delivered => {
                first.set().unwrap();
                assert_eq!(turn(reactor), 1);
                assert_eq!(old.get(), 1);
                assert_eq!(registration.cancel(), CancelOutcome::AlreadyDelivered);
            }
        }
        assert_eq!(reactor.pooled(), 1, "{scenario:?}: пакет не вернулся в пул");
        let registration = reactor.register(second.raw_handle(), new.waker()).unwrap();
        assert_eq!(reactor.pooled(), 0, "{scenario:?}");
        assert_eq!(
            reactor.packets_created(),
            1,
            "{scenario:?}: пакет не из пула"
        );
        // Устаревшее завершение прежней регистрации (тот же слот, старое
        // поколение) -- отбрасывается, новый waker не будится.
        reactor.process(&in_flight, &mut Spent::default());
        // Прежнее событие переиспользованный пакет не будит.
        first.set().unwrap();
        assert_eq!(turn_quick(reactor), 0, "{scenario:?}: чужое событие");
        assert_eq!(new.get(), 0, "{scenario:?}");
        assert!(!registration.is_fired(), "{scenario:?}");
        second.set().unwrap();
        assert_eq!(turn(reactor), 1, "{scenario:?}");
        assert_eq!(new.get(), 1, "{scenario:?}");
        assert_eq!(turn_quick(reactor), 0, "{scenario:?}");
        assert_eq!(new.get(), 1, "{scenario:?}: больше одного пробуждения");
        let expected_old = usize::from(scenario == Scenario::Delivered);
        assert_eq!(old.get(), expected_old, "{scenario:?}");
        let expected_stale = u64::from(scenario == Scenario::DequeuedBeforeLookup);
        assert_eq!(reactor.stale_completions(), expected_stale, "{scenario:?}");
    }

    /// Пакет из пула чист после любого исхода прежней регистрации.
    #[test]
    fn pooled_packet_is_clean_after_every_outcome() {
        for scenario in [
            Scenario::NotFired,
            Scenario::FiredNotDelivered,
            Scenario::DequeuedBeforeLookup,
            Scenario::Delivered,
        ] {
            run_scenario(scenario);
        }
    }

    /// Прогретый пул переживает отказ создания пакета; пустой -- отказ
    /// чистый: `Err`, слот таблицы возвращён, чужой сигнал цел.
    #[test]
    fn warm_pool_survives_create_failure() {
        let reactor = manual_reactor();
        let event = EventHandle::create_unnamed(false).unwrap();
        let other = EventHandle::create_unnamed(false).unwrap();
        let counter = Counter::new();
        let warm = reactor
            .register(event.raw_handle(), counter.waker())
            .unwrap();
        assert_eq!(warm.cancel(), CancelOutcome::NotFired);
        reactor.fail_create.store(true, Ordering::SeqCst);
        let registration = reactor
            .register(event.raw_handle(), counter.waker())
            .unwrap();
        other.set().unwrap();
        let failed = reactor.register(other.raw_handle(), counter.waker());
        assert!(failed.is_err(), "{failed:?}");
        reactor.fail_create.store(false, Ordering::SeqCst);
        assert_eq!(reactor.packets_created(), 1);
        assert_eq!(reactor.table.lock_clean().slots.len(), 1);
        event.set().unwrap();
        assert_eq!(turn(reactor), 1);
        assert_eq!(counter.get(), 1);
        assert!(registration.is_fired());
        assert!(other.wait(Some(Duration::ZERO)).unwrap(), "сигнал цел");
    }

    /// Классификация кодов отмены: «грязный» код -- пакет не в пул.
    #[test]
    fn unexpected_cancel_status_is_not_reused() {
        let invalid = 0xC000_0008_u32.cast_signed();
        let never = || -> NTSTATUS { unreachable!("второго шага нет") };
        assert_eq!(
            classify_cancel(STATUS_SUCCESS, never),
            (CancelOutcome::NotFired, true)
        );
        assert_eq!(
            classify_cancel(STATUS_CANCELLED, never),
            (CancelOutcome::AlreadyDelivered, true)
        );
        assert_eq!(
            classify_cancel(invalid, never),
            (CancelOutcome::AlreadyDelivered, false)
        );
        assert_eq!(
            classify_cancel(STATUS_PENDING, || STATUS_SUCCESS),
            (CancelOutcome::FiredNotDelivered, true)
        );
        assert_eq!(
            classify_cancel(STATUS_PENDING, || STATUS_CANCELLED),
            (CancelOutcome::AlreadyDelivered, true)
        );
        assert_eq!(
            classify_cancel(STATUS_PENDING, || invalid),
            (CancelOutcome::FiredNotDelivered, false)
        );
    }

    /// Прогреть пул ручного реактора одним пакетом.
    fn warm(reactor: &'static Reactor, event: &EventHandle) {
        let registration = reactor
            .register(event.raw_handle(), Counter::new().waker())
            .unwrap();
        assert_eq!(registration.cancel(), CancelOutcome::NotFired);
        assert_eq!(reactor.pooled(), 1);
    }

    /// Fix round 1, Minor 1: отказ привязки по вине объекта ожидания
    /// (настоящие коды ядра: `STATUS_INVALID_HANDLE`,
    /// `STATUS_INVALID_PARAMETER_3` -- порт вместо объекта) -- пакет
    /// доказанно чист и возвращается в пул; новых пакетов нет.
    #[test]
    fn target_failure_keeps_pooled_packet() {
        let reactor = manual_reactor();
        let event = EventHandle::create_unnamed(false).unwrap();
        warm(reactor, &event);
        let counter = Counter::new();
        let port_handle = reactor.port.handle.raw().addr().cast_signed();
        for handle in [0, 0x00FF_FFF0, port_handle] {
            let result = reactor.register(handle, counter.waker());
            assert!(result.is_err(), "handle {handle:#x}: {result:?}");
            assert_eq!(reactor.pooled(), 1, "handle {handle:#x}: пакет закрыт");
        }
        assert_eq!(reactor.packets_created(), 1);
        let registration = reactor
            .register(event.raw_handle(), counter.waker())
            .unwrap();
        assert_eq!(reactor.packets_created(), 1, "пакет не из пула");
        event.set().unwrap();
        assert_eq!(turn(reactor), 1);
        assert_eq!(counter.get(), 1);
        assert!(registration.is_fired());
    }

    /// Fix round 1, Minor 1: отказ «по вине пакета» (инъекция кода
    /// `STATUS_INVALID_PARAMETER_1`, и неоднозначного
    /// `STATUS_INVALID_HANDLE` при негодном пакете -- ниже) не отравляет
    /// пул: пакет из пула закрыт, регистрация повторена с новым и удалась;
    /// новый пакет с тем же отказом -- `Err`, в пул не попадает.
    #[test]
    fn packet_failure_does_not_poison_pool() {
        let reactor = manual_reactor();
        let event = EventHandle::create_unnamed(false).unwrap();
        warm(reactor, &event);
        let counter = Counter::new();
        reactor
            .arm_fault
            .store(STATUS_INVALID_PARAMETER_1, Ordering::SeqCst);
        let registration = reactor
            .register(event.raw_handle(), counter.waker())
            .unwrap();
        assert_eq!(reactor.packets_created(), 2, "повтор -- с новым пакетом");
        assert_eq!(reactor.pooled(), 0);
        event.set().unwrap();
        assert_eq!(turn(reactor), 1);
        assert_eq!(counter.get(), 1);
        assert!(registration.is_fired());
        assert_eq!(reactor.pooled(), 1, "новый пакет вернулся после доставки");

        // Пул пуст, новый пакет отказал: `Err`, пакет закрыт, слот свободен.
        let held = reactor
            .register(event.raw_handle(), counter.waker())
            .unwrap();
        reactor
            .arm_fault
            .store(STATUS_INVALID_PARAMETER_1, Ordering::SeqCst);
        let failed = reactor.register(event.raw_handle(), counter.waker());
        assert!(failed.is_err(), "{failed:?}");
        assert_eq!(reactor.packets_created(), 3);
        assert_eq!(reactor.pooled(), 0, "негодный пакет попал в пул");
        assert_eq!(reactor.table.lock_clean().free.len(), 1);
        drop(held);
        assert_eq!(reactor.pooled(), 1);
    }

    /// Fix round 1, Minor 1, настоящий «грязный» пакет: в пул подложен
    /// уже привязанный пакет (ядро отвечает `STATUS_INVALID_PARAMETER_1`).
    /// Регистрация его закрывает (привязка снимается, чужой сигнал цел) и
    /// удаётся с новым пакетом; следующие регистрации пул не отравляет.
    #[test]
    fn bound_packet_in_pool_is_replaced() {
        let reactor = manual_reactor();
        let event = EventHandle::create_unnamed(false).unwrap();
        let decoy = EventHandle::create_unnamed(false).unwrap();
        warm(reactor, &event);
        {
            let mut table = reactor.table.lock_clean();
            let packet = table.pool.last_mut().unwrap();
            reactor
                .port
                .arm(packet, raw_handle(decoy.raw_handle()), usize::MAX, 0)
                .unwrap();
        }
        let counter = Counter::new();
        for round in 1..=3 {
            let registration = reactor
                .register(event.raw_handle(), counter.waker())
                .unwrap();
            event.set().unwrap();
            assert_eq!(turn(reactor), 1);
            assert_eq!(counter.get(), round);
            assert!(registration.is_fired());
        }
        assert_eq!(reactor.packets_created(), 2, "ровно одна замена");
        decoy.set().unwrap();
        assert_eq!(turn_quick(reactor), 0, "привязка закрытого пакета жива");
        assert!(decoy.wait(Some(Duration::ZERO)).unwrap(), "сигнал цел");
        assert_eq!(reactor.stale_completions(), 0);
    }

    /// Проба пакета: код, допускающий вину объекта, не спасает негодный
    /// пакет (уже привязанный -- проба снимает привязку и отвечает не
    /// `STATUS_CANCELLED`).
    #[test]
    fn survives_arm_failure_requires_idle_packet() {
        let port = CompletionPort::new().unwrap();
        let event = EventHandle::create_unnamed(false).unwrap();
        let idle = WaitPacket::new().unwrap();
        assert!(idle.survives_arm_failure(STATUS_INVALID_HANDLE));
        assert!(idle.survives_arm_failure(STATUS_OBJECT_TYPE_MISMATCH));
        assert!(idle.survives_arm_failure(STATUS_ACCESS_DENIED));
        assert!(idle.survives_arm_failure(STATUS_INVALID_PARAMETER_3));
        assert!(!idle.survives_arm_failure(STATUS_INVALID_PARAMETER_1));
        assert!(!idle.survives_arm_failure(0xC000_009A_u32.cast_signed()));
        let bound = port.associate_raw(event.raw_handle(), 1, 0).unwrap();
        assert!(!bound.survives_arm_failure(STATUS_INVALID_HANDLE));
    }
}
