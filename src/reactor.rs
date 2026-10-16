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
//! Отмена регистрации, проигравшая гонку с сигналом, сигнал тоже теряет
//! (пакет вынимается из очереди вместе с потреблённым сигналом).
//!
//! # Ключи и гонка отмены
//!
//! Ключ пакета -- не указатель, а пара (индекс слота таблицы, поколение
//! слота): `KeyContext` = индекс, `ApcContext` = поколение -- оба полного
//! размера указателя, на x86 тоже. Снятие регистрации (срабатывание или
//! `Drop`) увеличивает поколение слота. `NtCancelWaitCompletionPacket` может
//! проиграть гонку: поток реактора уже извлёк пакет, но ещё не дошёл до
//! таблицы. Тогда он находит слот пустым или с новым поколением и молча
//! отбрасывает пакет -- ни обращения к освобождённому, ни пробуждения чужого
//! `Waker`.

use std::mem::MaybeUninit;
use std::os::windows::io::{AsRawHandle, BorrowedHandle};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::task::Waker;
use std::thread;
use std::time::Duration;

use crate::auto::map_spawn_error;
use crate::error::Result;
use crate::ntapi::{
    FILE_IO_COMPLETION_INFORMATION, GENERIC_ALL, HANDLE, IO_COMPLETION_ALL_ACCESS, NTSTATUS,
    NtAssociateWaitCompletionPacket, NtCancelWaitCompletionPacket, NtCreateIoCompletion,
    NtCreateWaitCompletionPacket, NtRemoveIoCompletionEx, NtSetIoCompletion, STATUS_CANCELLED,
    STATUS_SUCCESS, STATUS_TIMEOUT, ULONG, duration_to_nt_timeout,
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

/// Исход `NtCancelWaitCompletionPacket(.., RemoveSignaledPacket = TRUE)`
/// (коды проверены тестом `port_cancel_outcomes`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CancelOutcome {
    /// `STATUS_SUCCESS`: привязка снята -- либо ожидание ещё не сработало
    /// (сигнал не потреблён), либо сработало и пакет вынут из очереди
    /// порта (сигнал потреблён и потерян). Ядро эти случаи не различает.
    Cancelled,
    /// `STATUS_CANCELLED`: снимать нечего -- пакет уже извлечён из порта
    /// или не привязывался.
    NotAssociated,
    /// Иной код (`STATUS_PENDING` бывает только при `RemoveSignaledPacket =
    /// FALSE`: пакет остался в очереди). Его извлечение отбросит владелец
    /// ключа.
    Other(NTSTATUS),
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
        if status != STATUS_SUCCESS {
            return Err(status_to_error(status, "NtAssociateWaitCompletionPacket"));
        }
        packet.already_signaled = signaled != 0;
        Ok(())
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

    /// Снять привязку; поставленный в очередь пакет вынимается
    /// (`RemoveSignaledPacket = TRUE`).
    pub(crate) fn cancel(&self) -> CancelOutcome {
        // SAFETY: дескриптор пакета жив, пока жив `self`.
        let status = unsafe { NtCancelWaitCompletionPacket(self.handle.raw(), 1) };
        match status {
            STATUS_SUCCESS => CancelOutcome::Cancelled,
            STATUS_CANCELLED => CancelOutcome::NotAssociated,
            other => CancelOutcome::Other(other),
        }
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

/// Таблица регистраций: слоты + список свободных.
#[derive(Debug, Default)]
struct Table {
    slots: Vec<Slot>,
    free: Vec<usize>,
}

impl Table {
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
/// (`Waker` + пакет) живут в таблице, пока владелец не снимет их `Drop`-ом
/// или они не сработают.
#[derive(Debug)]
pub(crate) struct Reactor {
    port: CompletionPort,
    table: Mutex<Table>,
    /// Отброшенные устаревшие пакеты (слот снят или переиспользован).
    #[cfg(test)]
    stale: AtomicU64,
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
        let reactor = Self::new()?;
        Ok(GLOBAL.get_or_init(|| reactor))
    }

    /// Отдельный реактор со своим потоком (до конца процесса; тесты
    /// изолируют им счётчики). При отказе запуска потока порт остаётся
    /// неосвобождённым -- редкий отказ ОС, не стоит `unsafe`-возврата
    /// утёкшей памяти.
    pub(crate) fn new() -> Result<&'static Self> {
        let port = CompletionPort::new()?;
        let reactor: &'static Self = Box::leak(Box::new(Self {
            port,
            table: Mutex::new(Table::default()),
            #[cfg(test)]
            stale: AtomicU64::new(0),
        }));
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
        crate::thread_hook::spawn(builder, move || reactor.run())
            .map_err(|err| map_spawn_error(err, "spawn reactor thread"))?;
        Ok(reactor)
    }

    /// Зарегистрировать одноразовое ожидание объекта `handle`: при его
    /// сигнале `waker` будится один раз, регистрация снимается.
    ///
    /// Регистрация -- это ожидание: сигнал автосбросного события она
    /// потребляет (в т.ч. уже взведённого -- тогда `waker` будится сразу,
    /// ровно один раз). Закрытый или негодный `handle` -- `Err`
    /// (`WindowsError`, контекст `NtAssociateWaitCompletionPacket`).
    pub(crate) fn register(&'static self, handle: isize, waker: Waker) -> Result<Registration> {
        // Пакет создаём вне блокировки; привязываем -- под ней: пакет может
        // прийти сразу, и поток реактора обязан найти запись в таблице.
        let mut packet = WaitPacket::new()?;
        let mut table = self.table.lock_clean();
        let index = table.free.pop().unwrap_or(table.slots.len());
        if index == table.slots.len() {
            table.slots.push(Slot::default());
        }
        // Индексация не паникует: индекс -- из списка свободных (слоты не
        // удаляются) или только что добавленного слота.
        let generation = table.slots[index].generation;
        if let Err(err) = self
            .port
            .arm(&mut packet, raw_handle(handle), index, generation)
        {
            // Привязки не было -- пакета с этим ключом в порту нет,
            // поколение менять незачем.
            table.free.push(index);
            drop(table);
            return Err(err);
        }
        table.slots[index].entry = Some(Entry { waker, packet });
        drop(table);
        Ok(Registration {
            reactor: self,
            index,
            generation,
        })
    }

    /// Цикл потока реактора: бесконечное ожидание порта, пачка пакетов ->
    /// записи из таблицы (под блокировкой) -> пробуждение (без неё).
    fn run(&self) {
        let mut batch = Vec::with_capacity(BATCH_LEN);
        let mut fired: Vec<Entry> = Vec::with_capacity(BATCH_LEN);
        loop {
            if self.port.wait_batch(&mut batch, None).is_err() {
                thread::sleep(ERROR_BACKOFF);
                continue;
            }
            {
                let mut table = self.table.lock_clean();
                for completion in &batch {
                    match table.take(completion.key, completion.tag) {
                        Some(entry) => fired.push(entry),
                        // Устаревший пакет: регистрация уже снята (и,
                        // возможно, слот занят новой) -- отбрасываем.
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
            for Entry { waker, packet } in fired.drain(..) {
                // Паника чужого `Waker` не должна убить общий поток.
                drop(catch_unwind(AssertUnwindSafe(|| waker.wake())));
                // Пакет извлечён -- отмена в `Drop` ничего не найдёт.
                drop(packet);
            }
        }
    }

    /// Сколько устаревших пакетов отброшено.
    #[cfg(test)]
    fn stale_completions(&self) -> u64 {
        self.stale.load(Ordering::Relaxed)
    }
}

/// Одноразовая регистрация ожидания в [`Reactor`]. Сработала -- снята
/// реактором; `Drop` несработавшей отменяет пакет
/// (`NtCancelWaitCompletionPacket`, `RemoveSignaledPacket = TRUE`) и
/// закрывает его. После возврата из `Drop` `Waker` не будится никогда.
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
        let entry = self
            .reactor
            .table
            .lock_clean()
            .take(self.index, self.generation);
        // Отмена пакета и `Drop` `Waker` -- вне блокировки.
        drop(entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ShmError;
    use crate::win::EventHandle;
    use std::future::Future;
    use std::pin::{Pin, pin};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Wake, Waker};
    use std::thread::{self, Thread, ThreadId};
    use std::time::{Duration, Instant};

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
        let reactor = Reactor::new().unwrap();
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
        let reactor = Reactor::new().unwrap();
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
        let reactor = Reactor::new().unwrap();
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
        let reactor = Reactor::new().unwrap();
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
    /// инвариант: после возврата из `Drop` waker больше не будится никогда.
    #[test]
    fn drop_racing_completion_never_wakes_after_drop() {
        let reactor = Reactor::new().unwrap();
        let event = EventHandle::create_unnamed(false).unwrap();
        let mut seen = Vec::new();
        for _ in 0..300 {
            let counter = Counter::new();
            let registration = reactor
                .register(event.raw_handle(), counter.waker())
                .unwrap();
            event.set().unwrap();
            drop(registration);
            seen.push((counter.get(), counter));
            // Сигнал, не доставленный из-за отмены, гасим к следующему кругу.
            let _ = event.reset();
        }
        sync(reactor);
        for (at_drop, counter) in &seen {
            assert!(*at_drop <= 1);
            assert_eq!(counter.get(), *at_drop, "пробуждение после Drop");
        }
    }

    /// Постановление 2: событие уже взведено до регистрации -- waker
    /// будится ровно один раз, сигнал автосбросного события потреблён.
    #[test]
    fn already_signaled_wakes_exactly_once() {
        let reactor = Reactor::new().unwrap();
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

        // Не привязывался -- снимать нечего.
        let fresh = WaitPacket::new().unwrap();
        assert_eq!(fresh.cancel(), CancelOutcome::NotAssociated);

        // Ожидание не сработало -- отмена снимает его; повторная -- нечего.
        let packet = port.associate_raw(event.raw_handle(), 1, 0).unwrap();
        assert!(!packet.already_signaled());
        assert_eq!(packet.cancel(), CancelOutcome::Cancelled);
        assert_eq!(packet.cancel(), CancelOutcome::NotAssociated);
        event.set().unwrap();
        assert!(
            event.wait(Some(Duration::ZERO)).unwrap(),
            "сигнал не потреблён"
        );

        // Сработало и стоит в очереди -- отмена вынимает пакет из очереди
        // (тот же `STATUS_SUCCESS`), сигнал потерян.
        let packet = port.associate_raw(event.raw_handle(), 2, 0).unwrap();
        event.set().unwrap();
        assert!(
            !event.wait(Some(Duration::ZERO)).unwrap(),
            "сигнал потреблён"
        );
        assert_eq!(packet.cancel(), CancelOutcome::Cancelled);
        assert_eq!(port.wait(Some(Duration::from_millis(50))).unwrap(), None);

        // Пакет уже извлечён -- отменять нечего; сработавший пакет можно
        // привязать снова, дважды привязанный -- нельзя.
        let mut packet = port.associate_raw(event.raw_handle(), 3, 0).unwrap();
        event.set().unwrap();
        assert_eq!(port.wait(Some(DEADLINE)).unwrap(), Some(3));
        assert_eq!(packet.cancel(), CancelOutcome::NotAssociated);
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
    #[test]
    fn closed_handle_is_error() {
        let reactor = Reactor::new().unwrap();
        let counter = Counter::new();
        // Таблица handle ядра раздаёт освобождённые значения LIFO: закрытый
        // последним достанется пакету самой `register`. Поэтому закрываем
        // проверяемый первым, а второй -- после: пакет займёт значение
        // второго, проверяемое останется закрытым. При параллельном прогоне
        // значение может успеть занять объект другого теста (регистрация
        // тогда законно удаётся) -- повторяем; последовательно хватает
        // первой попытки.
        let closed_is_error = (0..50).any(|_| {
            let closed = EventHandle::create_unnamed(false).unwrap();
            let decoy = EventHandle::create_unnamed(false).unwrap();
            let raw = closed.raw_handle();
            drop(closed);
            drop(decoy);
            match reactor.register(raw, Waker::noop().clone()) {
                Err(ShmError::WindowsError { code, .. }) => {
                    assert_eq!(code, 0xC000_0008, "STATUS_INVALID_HANDLE");
                    true
                }
                Err(other) => panic!("неожиданная ошибка {other:?}"),
                Ok(_reused) => false,
            }
        });
        assert!(closed_is_error, "закрытый handle ни разу не дал ошибку");
        // Пустой и несуществующий handle -- `STATUS_INVALID_HANDLE`.
        for handle in [0, 0x00FF_FFF0] {
            match reactor.register(handle, counter.waker()) {
                Err(ShmError::WindowsError { code, context }) => {
                    assert_eq!(code, 0xC000_0008, "handle {handle:#x}");
                    assert_eq!(context, "NtAssociateWaitCompletionPacket");
                }
                other => panic!("handle {handle:#x}: ожидалась ошибка, получено {other:?}"),
            }
        }
        sync(reactor);
        assert_eq!(counter.get(), 0);
        // Неудачные регистрации вернули слот: таблица не растёт.
        assert_eq!(reactor.table.lock_clean().slots.len(), 1);
    }
}
