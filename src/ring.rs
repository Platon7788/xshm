//! Lock-free SPSC кольцевой буфер для shared memory IPC.
//!
//! ВАЖНО: Код оптимизирован для x86/x86_64 с TSO (Total Store Order).
//! На этих архитектурах stores видны в порядке программы, что упрощает
//! синхронизацию. НЕ портировать на ARM/RISC-V без доработки!
//!
//! Кадр (формат `0x0002_0000`): `u32 len LE` + `u32 flags` (0) + payload,
//! дополненный до кратного `FRAME_ALIGN` (8). Позиции кадров кратны 8, поэтому
//! заголовок кадра не разрывается границей кольца; payload может.
//!
//! Граница доверия: содержимое кольца и `RingHeader` пишет другой процесс.
//! Читатель сверяет каждый кадр с `write_pos` (кадр не выходит за
//! опубликованные данные), с лимитом размера сообщения кольца и с
//! выравниванием -- мусор пира даёт `Corrupted`, а не зависание, панику или
//! выход за пределы кольца.

use std::marker::PhantomData;
use std::os::windows::io::AsHandle;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering, compiler_fence, fence};
use std::time::{Duration, Instant};

use crate::constants::*;
use crate::error::{Result, ShmError};
use crate::layout::{RingConfig, RingHeader};
use crate::win::{EventHandle, ProcessWatch, wait_any};

/// Шаг опроса, когда событий нет вовсе (anonymous-сервер: секция без имени,
/// ни одного объекта ядра, кроме самой секции). Это единственный путь
/// `wait_for_space` с опросом -- и он действует только пока вызывающий сам
/// блокируется в `wait_for_space`, а не в простое. Именованный канал (0.9+)
/// ждёт события без нарезки на срезы.
const SPACE_POLL_INTERVAL: Duration = Duration::from_millis(1);

#[cfg(test)]
thread_local! {
    /// Число ожиданий в ядре внутри `wait_for_space` на этом потоке: тесты
    /// доказывают, что ожидание не нарезано на срезы (одно событие -- одно
    /// ожидание).
    pub(crate) static SPACE_KERNEL_WAITS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// Число ожиданий в ядре внутри `wait_for_data` (`poll_*`) на этом
    /// потоке: тесты спина доказывают, что данные, пришедшие во время
    /// спина, читаются без сна в ядре.
    pub(crate) static DATA_KERNEL_WAITS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Политика спина перед ожиданием в ядре (0.10).
///
/// Перед тем как уснуть на событии (`DATA` у читателя, `SPACE` у писателя)
/// поток до `iterations` раз перепроверяет кольцо по позициям, с
/// `std::hint::spin_loop` между проверками. Данные (место), появившиеся за
/// это время, забираются без системного вызова и без переключения
/// контекста: задержка падает с единиц микросекунд до сотен наносекунд
/// ценой занятого ядра CPU на время спина.
///
/// Бюджет -- один на каждое засыпание, а не на каждую проверку: поток,
/// которому нечего делать, отспинивает бюджет один раз и спит в ядре без
/// таймаута, как и без спина (в простое -- ноль пробуждений). По умолчанию
/// -- [`SpinPolicy::NONE`]: спина нет, поведение 0.9.
///
/// Только локальная политика ожидания: формат памяти и протокол сигналов
/// не меняются, пиру спин не виден.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpinPolicy {
    /// Число проверок кольца до ожидания в ядре (`0` -- без спина).
    pub iterations: u32,
}

impl SpinPolicy {
    /// Без спина -- сразу ожидание в ядре (по умолчанию).
    pub const NONE: Self = Self { iterations: 0 };

    /// Спин на `iterations` проверок.
    #[must_use]
    pub const fn new(iterations: u32) -> Self {
        Self { iterations }
    }

    /// Спин включён (`iterations > 0`).
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        self.iterations > 0
    }
}

/// Спин: до `spin.iterations` раз вызвать `ready`, пока он не вернёт
/// `true` (`std::hint::spin_loop` между вызовами). `true` -- условие
/// выполнено до исчерпания бюджета, `false` -- бюджет исчерпан (или спин
/// выключен) и вызывающий уходит в обычное ожидание в ядре.
///
/// `ready` обязан быть дешёвым (атомарные чтения без блокировок) и сам
/// включать проверку остановки, если поток её ждёт: так задержка `stop()`
/// ограничена одной итерацией, а не всем бюджетом.
///
/// `deadline` -- дедлайн вызывающего (`poll_*_spin`, `wait_for_space_spin`):
/// проверяется раз в `SPIN_DEADLINE_STRIDE` итераций (`Instant::now` -- не
/// на каждой), наступил -- спин прекращается с `false`. Перебег таймаута
/// поэтому не больше `SPIN_DEADLINE_STRIDE` итераций (единицы микросекунд),
/// а не весь бюджет. Worker-ы без таймаута передают `None`.
pub(crate) fn spin_until(
    spin: SpinPolicy,
    deadline: Option<Instant>,
    mut ready: impl FnMut() -> bool,
) -> bool {
    for i in 0..spin.iterations {
        if ready() {
            return true;
        }
        if let Some(deadline) = deadline
            && i % SPIN_DEADLINE_STRIDE == SPIN_DEADLINE_STRIDE - 1
            && Instant::now() >= deadline
        {
            return false;
        }
        std::hint::spin_loop();
    }
    false
}

/// Раз во сколько итераций спин сверяется с дедлайном вызывающего.
const SPIN_DEADLINE_STRIDE: u32 = 64;

/// Дедлайн из таймаута (`None` -- без таймаута или он непредставим как
/// `Instant`: ждать «вечно» и так).
pub(crate) fn deadline_of(timeout: Option<Duration>) -> Option<Instant> {
    timeout.and_then(|t| Instant::now().checked_add(t))
}

/// Проба «есть непрочитанные данные» по позициям одного кольца -- для
/// спина worker-а Multi, который перебирает кольца нескольких слотов, не
/// беря их блокировок. Хранит только адрес `RingHeader` в отображении.
#[derive(Debug, Clone, Copy)]
pub(crate) struct UnreadProbe<'a> {
    header: NonNull<RingHeader>,
    _ring: PhantomData<&'a RingHeader>,
}

impl<'a> UnreadProbe<'a> {
    /// Та же проба с другим временем жизни.
    ///
    /// # Safety
    /// Отображение, в котором лежит заголовок кольца, живо всё время `'b`.
    pub(crate) const unsafe fn extend<'b>(self) -> UnreadProbe<'b> {
        UnreadProbe {
            header: self.header,
            _ring: PhantomData,
        }
    }

    /// В кольце есть опубликованный непрочитанный кадр
    /// (`write_pos != read_pos`, Acquire). Без барьера: «пусто» здесь --
    /// лишь повод спинить дальше, а перед сном читатель всё равно
    /// подтверждает пустоту через `confirm_empty`.
    pub(crate) fn has_unread(&self) -> bool {
        // SAFETY: заголовок валиден всё время `'a` (инвариант конструктора
        // `RingBuffer::unread_probe` / контракт `extend`); поля атомарные.
        let header = unsafe { self.header.as_ref() };
        header.write_pos.load(Ordering::Acquire) != header.read_pos.load(Ordering::Acquire)
    }
}

/// Штатное отключение пира для `wait_for_space`: событие `DISCONNECT` (одно
/// на обе стороны, автосброс) и проверка по разделяемой памяти, что пир
/// действительно ушёл (его состояние handshake уже не `SERVER_READY`) --
/// отличает свежий сигнал от устаревшего.
pub(crate) struct DisconnectWatch<'a> {
    pub event: &'a EventHandle,
    pub peer_left: &'a dyn Fn() -> bool,
}

/// Результат записи в кольцо.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteOutcome {
    /// Сколько старейших непрочитанных сообщений вытеснено (только запись
    /// с перезаписью; у `try_send*` всегда `0`).
    pub overwritten: u32,
    /// Сигналить `DATA`: после публикации кадра и полного барьера писатель
    /// увидел `read_pos ==` прежнему `write_pos` -- читатель уже выбрал всё,
    /// что было до этого кадра, и мог уснуть (протокол «публикация ->
    /// перепроверка», см. `RingBuffer::commit`). Не «счётчик был 0»:
    /// `message_count` читатель уменьшает ПОСЛЕ сдвига `read_pos`, и решение по
    /// нему теряло пробуждение. Лишний `true` безвреден, ложного `false` при
    /// спящем читателе нет.
    pub was_empty: bool,
}

/// Размер кадра в кольце для payload длины `payload_len`: заголовок
/// (`FRAME_HEADER_SIZE`) + payload, дополненные до кратного `FRAME_ALIGN`.
#[must_use]
pub const fn frame_size(payload_len: usize) -> usize {
    (FRAME_HEADER_SIZE + payload_len).next_multiple_of(FRAME_ALIGN)
}

/// Свободное место в исходящем кольце.
///
/// Снимок, снятый на стороне писателя, -- **нижняя граница**: единственный,
/// кто уменьшает свободное место, это сам писатель, а читатель параллельно
/// может только освобождать. Поэтому если `fits(n)` вернул `true`, следующий
/// `try_send*` того же писателя с payload длины `n` гарантированно пройдёт
/// (при одном писателе на направление -- см. SPSC-контракт).
///
/// Снимок, снятый из другого потока, чем писатель, -- просто наблюдение:
/// писатель мог уже занять часть места.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FreeSpace {
    /// Свободные байты кольца. Сообщение занимает `frame_size(len)` байт.
    pub bytes: usize,
    /// Сколько ещё сообщений можно положить до лимита `max_messages` кольца.
    pub messages: u32,
    /// Лимит размера payload этого кольца (`RingConfig::max_message_size`).
    pub max_message_size: usize,
}

impl FreeSpace {
    /// Нулевое место: канал не подключён или кольцо заполнено.
    pub const ZERO: Self = Self {
        bytes: 0,
        messages: 0,
        max_message_size: 0,
    };

    /// Поместится ли сейчас payload длины `payload_len` без перезаписи.
    /// Для недопустимых длин (`< MIN_MESSAGE_SIZE` или больше лимита кольца)
    /// -- всегда `false`: такие сообщения не принимаются никогда.
    #[must_use]
    pub const fn fits(&self, payload_len: usize) -> bool {
        payload_len >= MIN_MESSAGE_SIZE
            && payload_len <= self.max_message_size
            && self.messages > 0
            && self.bytes >= frame_size(payload_len)
    }

    /// Максимальная длина payload, которая поместится прямо сейчас
    /// (`0` -- не поместится ни одно сообщение).
    #[must_use]
    pub const fn max_payload(&self) -> usize {
        if self.messages == 0 || self.bytes < frame_size(MIN_MESSAGE_SIZE) {
            return 0;
        }
        // Наибольший len с frame_size(len) <= bytes: выравнивание поглощает
        // до FRAME_ALIGN - 1 байт хвоста.
        let room = (self.bytes & !(FRAME_ALIGN - 1)) - FRAME_HEADER_SIZE;
        if room > self.max_message_size {
            self.max_message_size
        } else {
            room
        }
    }
}

/// Кадр, найденный читателем по позиции `read`.
#[derive(Debug, Clone, Copy)]
struct Frame {
    len: usize,
    total: u32,
}

#[derive(Debug)]
pub struct RingBuffer {
    header: NonNull<RingHeader>,
    storage: NonNull<u8>,
    capacity: u32,
    mask: u32,
    max_messages: u32,
    max_message_size: u32,
}

// SAFETY: тип хранит только адреса внутри shared-маппинга, который живёт не
// меньше самого кольца (инвариант `RingBuffer::new`). Всё состояние кольца --
// атомарные поля `RingHeader`, доступ к данным идёт по позициям, опубликованным
// через Acquire/Release, поэтому передача между потоками безопасна.
unsafe impl Send for RingBuffer {}
// SAFETY: см. выше; SPSC-контракт (один писатель, один читатель) обеспечивает
// вызывающий код, а не система типов.
unsafe impl Sync for RingBuffer {}

impl RingBuffer {
    /// # Safety
    /// `header` -- выровненный на 64 байта `RingHeader`, `data` -- начало
    /// `config.capacity` байт данных кольца; обе области не пересекаются и
    /// валидны всё время жизни кольца. `config` проверен
    /// (`RingConfig::validate`).
    ///
    /// Нулевой указатель -- `Err(Corrupted)`, а не паника. Библиотека
    /// строит кольца из адресов отображения (`from_parts`); из сырых
    /// указателей -- только тестовые фикстуры.
    #[cfg(test)]
    pub unsafe fn new(header: *mut RingHeader, data: *mut u8, config: &RingConfig) -> Result<Self> {
        let (Some(header), Some(data)) = (NonNull::new(header), NonNull::new(data)) else {
            return Err(ShmError::Corrupted);
        };
        // SAFETY: контракт `new` совпадает с контрактом `from_parts`.
        Ok(unsafe { Self::from_parts(header, data, config) })
    }

    /// Кольцо из заведомо ненулевых указателей (`SharedView`: адреса внутри
    /// отображения).
    ///
    /// # Safety
    /// То же, что у [`RingBuffer::new`].
    pub(crate) unsafe fn from_parts(
        header: NonNull<RingHeader>,
        storage: NonNull<u8>,
        config: &RingConfig,
    ) -> Self {
        debug_assert!(config.validate().is_ok());
        RingBuffer {
            header,
            storage,
            capacity: config.capacity as u32,
            mask: config.capacity as u32 - 1,
            max_messages: config.max_messages,
            max_message_size: config.max_message_size as u32,
        }
    }

    /// Лимит размера payload.
    #[cfg(test)]
    pub const fn max_message_size(&self) -> usize {
        self.max_message_size as usize
    }

    const fn header(&self) -> &RingHeader {
        // SAFETY: указатель на заголовок валиден и выровнен на всё время жизни
        // `self` (инвариант `RingBuffer::new`); все поля атомарные, поэтому
        // shared-ссылки достаточно и для записи.
        unsafe { self.header.as_ref() }
    }

    const fn data_ptr(&self) -> *mut u8 {
        self.storage.as_ptr()
    }

    const fn mask_index(&self, pos: u32) -> usize {
        (pos & self.mask) as usize
    }

    /// Проверка длины payload для записи; возвращает размер кадра.
    pub(crate) const fn frame_len(&self, payload_len: usize) -> Result<u32> {
        if payload_len < MIN_MESSAGE_SIZE {
            return Err(ShmError::MessageTooSmall);
        }
        if payload_len > self.max_message_size as usize {
            return Err(ShmError::MessageTooLarge);
        }
        // max_message_size <= capacity - FRAME_HEADER_SIZE (RingConfig::validate),
        // поэтому кадр всегда помещается в пустое кольцо и в u32.
        Ok(frame_size(payload_len) as u32)
    }

    /// Слово заголовка кадра по индексу `index` (кратен 4, внутри кольца).
    fn header_word(&self, index: usize) -> &AtomicU32 {
        // Инвариант вызывающих (позиции проверены `frame_at`/`writer_used`);
        // нарушение -- ошибка в самом крейте, проверка дешёвая и остаётся в
        // release: запись за пределы кольца хуже паники.
        assert!(
            index.is_multiple_of(4) && index + 4 <= self.capacity as usize,
            "ring frame header index out of bounds"
        );
        // SAFETY: index + 4 <= capacity (проверено выше) -- адрес внутри
        // данных кольца.
        let word = unsafe { self.data_ptr().add(index) };
        // SAFETY: контракт `AtomicU32::from_ptr`: адрес выровнен на 4 (index
        // кратен 4, storage выровнен на 64 -- идёт сразу за RingHeader) и
        // валиден всё время жизни `&self` (инвариант `new`); к слову заголовка
        // кадра обе стороны обращаются только атомарно, поэтому гонка с
        // перезаписью писателем -- не data race.
        unsafe { AtomicU32::from_ptr(word.cast()) }
    }

    /// Скопировать `src` в кольцо с позиции `start` с переносом через
    /// границу.
    ///
    /// # Safety
    /// `start < capacity`, `src.len() <= capacity`.
    unsafe fn copy_in(&self, start: usize, src: &[u8]) {
        let capacity = self.capacity as usize;
        let first = (capacity - start).min(src.len());
        let (head, tail) = src.split_at(first);
        // SAFETY: start < capacity (контракт функции) -- адрес внутри кольца.
        let dst = unsafe { self.data_ptr().add(start) };
        // SAFETY: [start, start+first) лежит внутри кольца (first <=
        // capacity - start); источник -- локальный срез, не пересекается с
        // разделяемой памятью.
        unsafe { std::ptr::copy_nonoverlapping(head.as_ptr(), dst, head.len()) };
        // SAFETY: [0, src.len()-first) лежит внутри кольца (src.len() <=
        // capacity); источник -- локальный срез.
        unsafe { std::ptr::copy_nonoverlapping(tail.as_ptr(), self.data_ptr(), tail.len()) };
    }

    /// Скопировать `len` байт из кольца с позиции `start` в `dst`.
    ///
    /// Копирование идёт сырыми указателями без промежуточного `&[u8]` на
    /// разделяемую память: при перезаписи писатель может менять эти байты
    /// одновременно (seqlock-валидация в `read_message` отбросит копию).
    ///
    /// # Safety
    /// `start < capacity`, `len <= capacity`, `dst` -- `len` байт записи.
    unsafe fn copy_out(&self, start: usize, dst: *mut u8, len: usize) {
        let capacity = self.capacity as usize;
        let first = (capacity - start).min(len);
        // SAFETY: start < capacity (контракт функции) -- адрес внутри кольца.
        let src = unsafe { self.data_ptr().add(start) };
        // SAFETY: [start, start+first) внутри кольца, `dst` -- `len >= first`
        // байт записи (контракт функции).
        unsafe { std::ptr::copy_nonoverlapping(src, dst, first) };
        // SAFETY: first <= len -- адрес внутри буфера `dst`.
        let dst_tail = unsafe { dst.add(first) };
        // SAFETY: [0, len-first) внутри кольца (len <= capacity), `dst_tail`
        // -- оставшиеся `len - first` байт записи.
        unsafe { std::ptr::copy_nonoverlapping(self.data_ptr(), dst_tail, len - first) };
    }

    /// Кадр в позиции `read` при опубликованном `write`.
    ///
    /// `Ok(None)` -- кольцо пусто; `Err(Corrupted)` -- позиции или заголовок
    /// кадра не согласуются (порча пиром или гонка с перезаписью -- решает
    /// вызывающий, перечитав `read_pos`).
    fn frame_at(&self, read: u32, write: u32) -> Result<Option<Frame>> {
        let used = write.wrapping_sub(read);
        if used == 0 {
            return Ok(None);
        }
        if used > self.capacity || !(read as usize).is_multiple_of(FRAME_ALIGN) {
            return Err(ShmError::Corrupted);
        }
        // Relaxed: видимость байтов кадра дало Acquire-чтение write_pos.
        let len = self
            .header_word(self.mask_index(read))
            .load(Ordering::Relaxed) as usize;
        if !(MIN_MESSAGE_SIZE..=self.max_message_size as usize).contains(&len) {
            return Err(ShmError::Corrupted);
        }
        let total = frame_size(len) as u32;
        if total > used {
            return Err(ShmError::Corrupted);
        }
        Ok(Some(Frame { len, total }))
    }

    /// Вытеснить старейшее сообщение (запись с перезаписью). `Ok(true)` --
    /// вытеснено, `Ok(false)` -- читатель успел сдвинуть `read_pos`
    /// (перепроверить место).
    fn discard_oldest(&self, read: u32, write: u32) -> Result<bool> {
        let header = self.header();
        let Some(frame) = self.frame_at(read, write)? else {
            return Ok(false);
        };
        // CAS: читатель может параллельно забрать это же сообщение.
        if header
            .read_pos
            .compare_exchange(
                read,
                read.wrapping_add(frame.total),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return Ok(false);
        }
        self.decrement_count();
        // Relaxed: счётчик статистики, ничего не публикует.
        header.drop_count.fetch_add(1, Ordering::Relaxed);
        Ok(true)
    }

    /// `message_count - 1` без ухода ниже нуля: пир может испортить
    /// счётчик, и обычный `fetch_sub` превратил бы `0` в `u32::MAX` (кольцо
    /// навсегда «полно по счётчику»). Возвращает прежнее значение.
    fn decrement_count(&self) -> u32 {
        self.header()
            .message_count
            .try_update(Ordering::AcqRel, Ordering::Acquire, |c| c.checked_sub(1))
            .unwrap_or(0)
    }

    /// Занятость кольца для писателя с проверкой позиций из разделяемой
    /// памяти. Читатель-пир может испортить и `read_pos`, и (через тот же
    /// заголовок) `write_pos`: невыровненная позиция записи дала бы индекс
    /// заголовка кадра у самого конца кольца, и `commit` писал бы ЗА его
    /// пределы. Позиции обязаны быть кратны `FRAME_ALIGN`, занятость -- не
    /// больше ёмкости; иначе `Corrupted` и ни одного байта в кольцо.
    const fn writer_used(&self, write: u32, read: u32) -> Result<u32> {
        let used = write.wrapping_sub(read);
        if used > self.capacity
            || !(write as usize).is_multiple_of(FRAME_ALIGN)
            || !(read as usize).is_multiple_of(FRAME_ALIGN)
        {
            return Err(ShmError::Corrupted);
        }
        Ok(used)
    }

    /// Запись с политикой «перезаписать старейшее»: при нехватке места
    /// (байт или слотов `max_messages`) вытесняет самые старые непрочитанные
    /// сообщения, число вытесненных -- в `WriteOutcome::overwritten`.
    ///
    /// Число итераций ограничено: вытеснение идёт только при `used > 0`
    /// (см. `admits`), каждая итерация либо сдвигает `read_pos` к
    /// `write_pos` (наше вытеснение или чтение читателя), либо застаёт
    /// кольцо пустым -- а пустое кольцо кадр принимает всегда. `Corrupted`
    /// -- только по позициям и заголовкам кадров, не по счётчику.
    pub fn write_message(&self, payload: &[u8]) -> Result<WriteOutcome> {
        let total = self.frame_len(payload.len())?;
        let header = self.header();
        let mut overwritten = 0u32;

        loop {
            let write = header.write_pos.load(Ordering::Acquire);
            let read = header.read_pos.load(Ordering::Acquire);
            let used = self.writer_used(write, read)?;
            let count = header.message_count.load(Ordering::Acquire);
            if self.admits(used, total, count) {
                let was_empty = self.commit(write, payload, total);
                return Ok(WriteOutcome {
                    overwritten,
                    was_empty,
                });
            }
            if used == 0 {
                // Недостижимо: пустое кольцо принимает любой допустимый кадр
                // (`admits`, `RingConfig::validate`). Ветка держит цикл
                // конечным, если инвариант конфигурации когда-нибудь
                // нарушат: вытеснять из пустого кольца нечего.
                return Err(ShmError::Corrupted);
            }
            if self.discard_oldest(read, write)? {
                overwritten += 1;
            }
        }
    }

    /// Запись без перезаписи: либо сообщение целиком ложится в свободное
    /// место кольца, либо `Err(QueueFull)` и кольцо не меняется вовсе.
    ///
    /// Семантика:
    /// - **никогда** не трогает `read_pos` и непрочитанные данные
    ///   (`overwritten` в результате всегда `0`, `drop_count` не растёт);
    /// - атомарность сообщения та же, что у `write_message`: данные
    ///   копируются до публикации `write_pos`, читатель видит сообщение
    ///   целиком или не видит вовсе;
    /// - `QueueFull` -- не хватает байт (`frame_size(len)`) или занято
    ///   `max_messages` слотов; проверка консервативна (см. `free_space`),
    ///   ложных успехов нет, ложный `QueueFull` возможен только если читатель
    ///   освободил место в тот же момент -- повтор увидит его;
    /// - длина вне `MIN_MESSAGE_SIZE..=max_message_size` --
    ///   `MessageTooSmall`/`MessageTooLarge` (кадр максимальной длины всегда
    ///   помещается в пустое кольцо, «больше кольца» не бывает);
    /// - писатель на направление ровно один (SPSC), как и у `write_message`.
    ///
    /// Ordering: `read_pos` читается с `Acquire`. Читатель двигает его CAS-ом
    /// (`AcqRel`) строго ПОСЛЕ того, как докопировал сообщение, поэтому всё,
    /// что писатель затем пишет в освобождённый диапазон, happens-after
    /// чтения этого диапазона читателем.
    pub fn try_write_message(&self, payload: &[u8]) -> Result<WriteOutcome> {
        let total = self.frame_len(payload.len())?;
        let header = self.header();

        // write_pos меняет только этот (единственный) писатель; read_pos и
        // message_count читатель может лишь уменьшать «занятость», поэтому
        // значения ниже -- консервативная оценка свободного места.
        let write = header.write_pos.load(Ordering::Acquire);
        let read = header.read_pos.load(Ordering::Acquire);
        let count = header.message_count.load(Ordering::Acquire);
        let used = self.writer_used(write, read)?;
        if !self.admits(used, total, count) {
            return Err(ShmError::QueueFull);
        }

        let was_empty = self.commit(write, payload, total);
        Ok(WriteOutcome {
            overwritten: 0,
            was_empty,
        })
    }

    /// Принять ли кадр `total` байт при занятости `used` (позиции уже
    /// проверены `writer_used`) и значении `message_count == count`.
    ///
    /// `message_count` -- подсказка, а не истина: его пишет и пир
    /// (недоверенный), и законно отстаёт на 1 -- читатель уменьшает его
    /// ПОСЛЕ сдвига `read_pos` (CAS), так что пустое по позициям кольцо
    /// может показывать `count == max_messages` (при `max_messages = 1` --
    /// обычная гонка). Поэтому лимит сообщений вытесняет/отказывает только
    /// при `used > 0`; пустое кольцо принимает любой кадр, помещающийся по
    /// байтам (а помещается любой допустимый: `max_message_size <=
    /// capacity - FRAME_HEADER_SIZE`). Счётчик здесь не правим -- его
    /// сводит насыщающий `decrement_count` читателя.
    const fn admits(&self, used: u32, total: u32, count: u32) -> bool {
        self.capacity - used >= total && (used == 0 || count < self.max_messages)
    }

    /// Копирует кадр в позицию `write` и публикует его.
    /// Возвращает `true`, если читатель мог уснуть, не увидев кадр, --
    /// вызывающий сигналит `DATA` (`WriteOutcome::was_empty`).
    ///
    /// Протокол «публикация -> перепроверка» (Dekker, пара к
    /// `confirm_empty` читателя; зеркало `arm_space_waiter`/
    /// `take_space_waiter`): писатель публикует `write_pos`, ставит
    /// `fence(SeqCst)` и ЗАТЕМ читает `read_pos`; читатель, увидев пустоту
    /// (`read_pos == write_pos`), ставит `fence(SeqCst)` и ЗАТЕМ перечитывает
    /// `write_pos`. Сдвиг `read_pos` читателем (его CAS) предшествует его
    /// фенсу. Два SeqCst-фенса гарантируют, что хотя бы одна сторона увидит
    /// запись другой: либо читатель при перепроверке увидит новый
    /// `write_pos` и прочитает кадр, либо писатель увидит `read_pos ==
    /// write` (всё до кадра выбрано) и просигналит событие -- потерянного
    /// пробуждения нет. `message_count` и `sequence` -- лимит и статистика,
    /// на решение о сигнале не влияют.
    ///
    /// Вызывающий обязан предварительно убедиться, что `total` байт начиная
    /// с `write` свободны и `message_count < max_messages`.
    fn commit(&self, write: u32, payload: &[u8], total: u32) -> bool {
        let header = self.header();
        let idx = self.mask_index(write);
        // Relaxed: заголовок кадра публикует Release-запись write_pos ниже.
        self.header_word(idx)
            .store(payload.len() as u32, Ordering::Relaxed);
        self.header_word(idx + 4).store(0, Ordering::Relaxed);
        // SAFETY: idx + FRAME_HEADER_SIZE < capacity по модулю mask; payload
        // не длиннее max_message_size <= capacity (frame_len).
        unsafe {
            self.copy_in((idx + FRAME_HEADER_SIZE) & self.mask as usize, payload);
        }

        // ВАЖНО: сначала увеличиваем message_count, потом обновляем write_pos:
        // писатель, проверяющий лимит, не увидит «лишнего» места. Прежнее
        // значение счётчика на сигнал НЕ влияет (см. протокол выше).
        header.message_count.fetch_add(1, Ordering::AcqRel);
        header
            .write_pos
            .store(write.wrapping_add(total), Ordering::Release);

        // Публикация write_pos упорядочена перед чтением read_pos -- первая
        // половина Dekker-пары с `confirm_empty` (на x86 -- mfence: TSO сам
        // по себе разрешает обгон store -> load).
        fence(Ordering::SeqCst);
        // Acquire -- по конвенции; порядок с перепроверкой читателя дал фенс.
        // `read_pos == write`: читатель (или наше же вытеснение в
        // `write_message`) выбрал всё до этого кадра. Больше `write` -- кадр
        // уже прочитан, будить некого; меньше -- читатель ещё не дочитал
        // старые кадры и не уснёт, не дойдя до этого.
        let was_empty = header.read_pos.load(Ordering::Acquire) == write;
        if was_empty {
            // Relaxed: счётчик статистики переходов empty -> non-empty.
            header.sequence.fetch_add(1, Ordering::Relaxed);
        }
        was_empty
    }

    /// Читатель: перед тем как признать кольцо пустым (и, возможно,
    /// уснуть на `DATA`), убедиться, что писатель не опубликовал кадр после
    /// снимка `write_seen` (при котором `read_pos == write_seen`). `true` --
    /// пусто и спать безопасно: писатель, публикующий кадр позже, увидит
    /// `read_pos == write_seen` и просигналит `DATA` (Dekker-пара с
    /// `commit`, см. там). `false` -- кадр появился, читать снова.
    fn confirm_empty(&self, write_seen: u32) -> bool {
        // Парный фенс к `commit`: сдвиг read_pos этим читателем (CAS в
        // `read_message`) упорядочен перед повторным чтением write_pos.
        fence(Ordering::SeqCst);
        self.header().write_pos.load(Ordering::Acquire) == write_seen
    }

    /// Свободное место кольца (байты и слоты сообщений).
    ///
    /// Для писателя -- нижняя граница (см. `FreeSpace`): порядок загрузок
    /// тот же, что в `try_write_message`, и каждое из значений может только
    /// устареть в сторону «занято больше, чем на самом деле».
    pub fn free_space(&self) -> FreeSpace {
        let header = self.header();
        let write = header.write_pos.load(Ordering::Acquire);
        let read = header.read_pos.load(Ordering::Acquire);
        let count = header.message_count.load(Ordering::Acquire);
        let used = write.wrapping_sub(read);
        let messages = self.max_messages.saturating_sub(count);
        FreeSpace {
            // Занятость больше ёмкости -- только на испорченном заголовке:
            // трактуем как «места нет».
            bytes: self.capacity.saturating_sub(used) as usize,
            // Пустое кольцо принимает кадр при любом (отстающем) счётчике --
            // то же правило, что у записи (`admits`).
            messages: if used == 0 { messages.max(1) } else { messages },
            max_message_size: self.max_message_size as usize,
        }
    }

    /// Писатель: заявить, что ждём места под кадр `frame` байт.
    ///
    /// Протокол «заявка -> перепроверка -> сон» (Dekker): писатель пишет
    /// заявку, ставит `fence(SeqCst)` и ЗАТЕМ перепроверяет место; читатель
    /// двигает `read_pos`, ставит `fence(SeqCst)` и ЗАТЕМ читает заявку
    /// (`take_space_waiter`). Два SeqCst-фенса гарантируют, что хотя бы одна
    /// сторона увидит запись другой: либо писатель при перепроверке увидит
    /// освобождённое место, либо читатель увидит заявку и просигналит
    /// событие -- потерянного пробуждения нет.
    pub(crate) fn arm_space_waiter(&self, frame: u32) {
        // Relaxed: упорядочивание с последующей перепроверкой даёт fence ниже.
        self.header().space_waiter.store(frame, Ordering::Relaxed);
        fence(Ordering::SeqCst);
    }

    /// Писатель: снять свою заявку (место нашлось без ожидания).
    pub(crate) fn disarm_space_waiter(&self) {
        // Relaxed: заявка -- только подсказка для пробуждения; лишний сигнал
        // события после снятия безвреден (писатель перепроверяет место).
        self.header().space_waiter.store(0, Ordering::Relaxed);
    }

    /// Читатель (после успешного `read_message`): если писатель ждёт места и
    /// его теперь достаточно -- атомарно снимает заявку и возвращает `true`
    /// (вызывающий сигналит событие `SPACE`).
    pub(crate) fn take_space_waiter(&self) -> bool {
        let header = self.header();
        // Парный фенс к `arm_space_waiter` (см. там): сдвиг read_pos в
        // read_message упорядочен перед чтением заявки.
        fence(Ordering::SeqCst);
        // Relaxed: упорядочивание с read_pos обеспечил фенс выше.
        let frame = header.space_waiter.load(Ordering::Relaxed);
        if frame == 0 {
            return false;
        }
        let free = self.free_space();
        if free.messages == 0 || free.bytes < frame as usize {
            // Места пока мало -- не будим писателя зря, заявка остаётся.
            return false;
        }
        // CAS, а не store: писатель мог уже снять/переподать заявку; сигналим,
        // только если сняли именно ту, которую проверяли. AcqRel -- по
        // конвенции для CAS, сам фенс выше уже дал нужный порядок.
        header
            .space_waiter
            .compare_exchange(frame, 0, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    }

    /// Прочитать старейшее сообщение в `out` (содержимое заменяется).
    /// Возвращает длину payload; `Err(QueueEmpty)` -- сообщений нет,
    /// `Err(Corrupted)` -- пир испортил кольцо (позиции/заголовок кадра).
    pub fn read_message(&self, out: &mut Vec<u8>) -> Result<usize> {
        let header = self.header();

        loop {
            let read = header.read_pos.load(Ordering::Acquire);
            // Acquire: всё, что писатель записал до публикации write_pos
            // (заголовок и payload кадра), видно ниже.
            let write = header.write_pos.load(Ordering::Acquire);
            let frame = match self.frame_at(read, write) {
                Ok(Some(frame)) => frame,
                // Пусто по снимку -- перепроверка после полного барьера
                // (вторая половина Dekker-пары с `commit`): кадр, опубликованный
                // после снимка, читаем сейчас, а не ждём `DATA`, которого
                // писатель тогда может не дать.
                Ok(None) if self.confirm_empty(write) => return Err(ShmError::QueueEmpty),
                Ok(None) => continue,
                Err(err) => {
                    // Заголовок мог быть «порван» перезаписью писателя: если
                    // read_pos уже сдвинулся -- это гонка, повторяем.
                    if header.read_pos.load(Ordering::Acquire) != read {
                        continue;
                    }
                    return Err(err);
                }
            };

            // ОПТИМИСТИЧНОЕ копирование ДО фиксации read_pos (seqlock-паттерн).
            // Если producer перезапишет слот во время копирования, CAS ниже
            // провалится, и мы отбросим эту (потенциально битую) копию.
            out.clear();
            out.reserve(frame.len);
            // SAFETY: в `out` зарезервировано не меньше frame.len байт;
            // frame.len <= max_message_size <= capacity, старт -- внутри
            // кольца по маске.
            unsafe {
                self.copy_out(
                    self.mask_index(read.wrapping_add(FRAME_HEADER_SIZE as u32)),
                    out.as_mut_ptr(),
                    frame.len,
                );
            }
            // SAFETY: первые frame.len байт только что инициализированы
            // (`copy_out` выше), ёмкость зарезервирована.
            unsafe { out.set_len(frame.len) };

            // Барьер компилятора: копирование не должно «переехать» НИЖЕ CAS,
            // иначе валидация теряет смысл. На x86 успешный lock cmpxchg также
            // даёт аппаратный барьер.
            compiler_fence(Ordering::Release);

            // Фиксация: атомарно забираем кадр. Провал => producer сдвинул
            // read_pos (перезапись) => скопированные байты невалидны.
            if header
                .read_pos
                .compare_exchange(
                    read,
                    read.wrapping_add(frame.total),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                continue;
            }

            if self.decrement_count() <= 1 {
                // Relaxed: счётчик статистики переходов non-empty -> empty.
                header.sequence.fetch_add(1, Ordering::Relaxed);
            }

            return Ok(frame.len);
        }
    }

    /// Писатель: дождаться, пока в кольце появится место под payload длины
    /// `payload_len`. `Ok(true)` -- место есть (следующий `try_write_message`
    /// этого писателя пройдёт), `Ok(false)` -- истёк `timeout`,
    /// `Err(PeerDied)` -- процесс читателя умер (место не освободится
    /// никогда), `Err(NotConnected)` -- читатель штатно отключился
    /// (`DISCONNECT`). `timeout = None` -- ждать без ограничения: безопасно
    /// только при наблюдаемом пире (`peer`), иначе упавший читатель подвесит
    /// навсегда -- единственный таймаут тогда тот, что задал вызывающий.
    ///
    /// Ожидание -- ОДНО `NtWaitForMultipleObjects` на весь остаток таймаута
    /// по набору `[SPACE, DISCONNECT, процесс читателя]`. `SPACE` читатель
    /// сигналит, сняв заявку (`take_space_waiter`) или опустошив кольцо. Без
    /// событий вовсе (anonymous-сервер) -- опрос с шагом `SPACE_POLL_INTERVAL`.
    ///
    /// `spin` (0.10): до заявки -- один бюджет спина на вызов с
    /// перепроверкой места по позициям; заявка `space_waiter` во время спина
    /// НЕ ставится (читатель не тратит на нас сигнал). При нулевом таймауте
    /// спина нет: вызов не блокирующий. Спин уважает дедлайн `timeout`:
    /// перебег не больше `SPIN_DEADLINE_STRIDE` итераций (см. `spin_until`).
    pub(crate) fn wait_for_space(
        &self,
        payload_len: usize,
        space_event: Option<&EventHandle>,
        peer: Option<&ProcessWatch>,
        disconnect: Option<&DisconnectWatch<'_>>,
        timeout: Option<Duration>,
        spin: SpinPolicy,
    ) -> Result<bool> {
        let frame = self.frame_len(payload_len)?;
        let deadline = timeout.map(|t| Instant::now() + t);
        if timeout != Some(Duration::ZERO)
            && spin_until(spin, deadline, || self.free_space().fits(payload_len))
        {
            return Ok(true);
        }
        loop {
            if self.free_space().fits(payload_len) {
                return Ok(true);
            }
            self.arm_space_waiter(frame);
            // Перепроверка ПОСЛЕ заявки -- вторая половина Dekker-протокола.
            if self.free_space().fits(payload_len) {
                self.disarm_space_waiter();
                return Ok(true);
            }
            let remaining = match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        self.disarm_space_waiter();
                        return Ok(false);
                    }
                    Some(remaining)
                }
                None => None,
            };

            let Some(space_event) = space_event else {
                // Anonymous: объектов ядра нет -- ждать нечего, кроме опроса.
                std::thread::sleep(
                    remaining.map_or(SPACE_POLL_INTERVAL, |r| r.min(SPACE_POLL_INTERVAL)),
                );
                if peer.is_some_and(ProcessWatch::has_exited)
                    && !self.free_space().fits(payload_len)
                {
                    self.disarm_space_waiter();
                    return Err(ShmError::PeerDied);
                }
                continue;
            };

            // Индекс 0 -- место (приоритет: при одновременном сигнале NT
            // отдаёт наименьший индекс), затем отключение, затем смерть.
            // Хвост массива -- заполнитель, в ожидание уходит `[..count]`.
            let mut handles = [space_event.as_handle(); 3];
            let mut count = 1;
            let disconnect_index = disconnect.map(|d| {
                handles[count] = d.event.as_handle();
                count += 1;
                count - 1
            });
            let peer_index = peer.map(|p| {
                handles[count] = p.as_handle();
                count += 1;
                count - 1
            });
            #[cfg(test)]
            SPACE_KERNEL_WAITS.with(|c| c.set(c.get() + 1));
            match wait_any(&handles[..count], remaining) {
                Err(err) => {
                    self.disarm_space_waiter();
                    return Err(err);
                }
                // SPACE или конец таймаута: верх цикла перепроверит место и
                // дедлайн.
                Ok(Some(0) | None) => {}
                Ok(Some(i)) if Some(i) == disconnect_index => {
                    if let Some(d) = disconnect
                        && (d.peer_left)()
                    {
                        // Событие одно на обе стороны и автосбросное: мы его
                        // поглотили -- возвращаем взведённым для остальных
                        // ждущих (worker, `poll_*` другого потока).
                        let _ = d.event.set();
                        self.disarm_space_waiter();
                        return Err(ShmError::NotConnected);
                    }
                    // Устаревший сигнал прошлой сессии -- поглощён, ждём дальше.
                }
                Ok(Some(i)) => {
                    debug_assert_eq!(Some(i), peer_index);
                    // Читатель мог успеть освободить место перед смертью --
                    // тогда честно отвечаем «место есть».
                    if !self.free_space().fits(payload_len) {
                        self.disarm_space_waiter();
                        return Err(ShmError::PeerDied);
                    }
                }
            }
        }
    }

    /// Нет опубликованных непрочитанных данных (`write_pos == read_pos`).
    ///
    /// Для читателя «пусто» подтверждается после полного барьера
    /// (`confirm_empty`): ответ `true` перед сном на `DATA` (`poll_*`)
    /// безопасен -- кадр, опубликованный позже, писатель просигналит.
    pub fn is_empty(&self) -> bool {
        let header = self.header();
        let read = header.read_pos.load(Ordering::Acquire);
        let write = header.write_pos.load(Ordering::Acquire);
        read == write && self.confirm_empty(write)
    }

    /// Проба непрочитанных данных этого кольца для спина (см.
    /// `UnreadProbe`).
    pub(crate) const fn unread_probe(&self) -> UnreadProbe<'_> {
        UnreadProbe {
            header: self.header,
            _ring: PhantomData,
        }
    }

    /// Дешёвая проверка «есть непрочитанное» по позициям, без барьера --
    /// для спина (`UnreadProbe::has_unread`). Решение «пусто, можно спать»
    /// принимает только `is_empty`.
    pub(crate) fn has_unread(&self) -> bool {
        self.unread_probe().has_unread()
    }

    /// Читатель: дождаться данных (`poll_*`). `Ok(true)` -- в кольце есть
    /// кадр ИЛИ сигнален `DATA` (возможно, устаревший: вызывающий читает
    /// до `QueueEmpty`), `Ok(false)` -- истёк `timeout`, `Err(PeerDied)` --
    /// процесс писателя завершился и всё, что он записал, прочитано.
    ///
    /// Порядок: `is_empty` (с барьером, Dekker-пара с `commit`), затем
    /// `spin` -- один бюджет на вызов с проверкой по позициям, затем ОДНО
    /// ожидание в ядре по `[DATA, процесс писателя]` (индекс 0 -- данные:
    /// при одновременном сигнале последние данные мёртвого пира не
    /// теряются). Спин, увидевший кадр, возвращает `Ok(true)` без ядра;
    /// `DATA`, который писатель мог при этом взвести, остаётся взведённым и
    /// даст позже одно лишнее пробуждение -- сбрасывать его нельзя (можно
    /// потерять сигнал следующего кадра). При нулевом таймауте спина нет;
    /// дедлайн `timeout` спин сверяет раз в `SPIN_DEADLINE_STRIDE` итераций.
    pub(crate) fn wait_for_data(
        &self,
        data: &EventHandle,
        peer: Option<&ProcessWatch>,
        timeout: Option<Duration>,
        spin: SpinPolicy,
    ) -> Result<bool> {
        if !self.is_empty() {
            return Ok(true);
        }
        if timeout != Some(Duration::ZERO)
            && spin_until(spin, deadline_of(timeout), || self.has_unread())
        {
            return Ok(true);
        }
        #[cfg(test)]
        DATA_KERNEL_WAITS.with(|c| c.set(c.get() + 1));
        let Some(peer) = peer else {
            return data.wait(timeout);
        };
        match wait_any(&[data.as_handle(), peer.as_handle()], timeout)? {
            Some(0) => Ok(true),
            Some(_) if !self.is_empty() => Ok(true),
            Some(_) => Err(ShmError::PeerDied),
            None => Ok(false),
        }
    }
}

#[cfg(test)]
mod overflow_race_tests {
    use super::*;

    pub(super) const RING_CAPACITY: usize = DEFAULT_RING_CAPACITY;
    use crate::layout::RingHeader;
    use std::alloc::{Layout, alloc_zeroed, dealloc};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as O};
    use std::thread;

    /// Владелец сырой выровненной памяти под один RingHeader + RING_CAPACITY.
    pub(super) struct RingMem {
        ptr: *mut u8,
        layout: Layout,
    }
    // SAFETY: владеет единственным блоком памяти, освобождает его ровно один раз
    // в Drop; доступ к содержимому идёт только через `RingBuffer` (атомарные
    // операции), поэтому передача владения между потоками безопасна.
    unsafe impl Send for RingMem {}
    // SAFETY: см. выше -- сам `RingMem` не даёт доступа к памяти, он только
    // владеет ей и освобождает.
    unsafe impl Sync for RingMem {}
    impl Drop for RingMem {
        fn drop(&mut self) {
            // SAFETY: ptr/layout получены из alloc_zeroed в make_ring.
            unsafe { dealloc(self.ptr, self.layout) };
        }
    }

    pub(super) fn make_ring() -> (RingBuffer, RingMem) {
        make_ring_with(&RingConfig::DEFAULT)
    }

    /// Кольцо с произвольной (проверенной) конфигурацией.
    pub(super) fn make_ring_with(config: &RingConfig) -> (RingBuffer, RingMem) {
        config.validate().unwrap();
        let header_size = size_of::<RingHeader>();
        let total = header_size + config.capacity;
        let layout = Layout::from_size_align(total, 64).unwrap();
        // SAFETY: ненулевой размер; зануление валидно для AtomicU32 полей.
        let ptr = unsafe { alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "alloc failed");
        let header = ptr.cast::<RingHeader>();
        // SAFETY: ptr выровнен на 64 и указывает на зануленный RingHeader.
        unsafe { (*header).reset(1) };
        // SAFETY: data сразу за заголовком, в пределах выделения.
        let data = unsafe { ptr.add(header_size) };
        // SAFETY: header и data валидны, не пересекаются, живут пока жив RingMem.
        let ring = unsafe { RingBuffer::new(header, data, config) }.unwrap();
        (ring, RingMem { ptr, layout })
    }

    // Большие сообщения: ~34 сообщения заполняют 2 МБ кольца ПО БАЙТАМ
    // (а не по счётчику MAX_MESSAGES=500). Torn-read возможен только в
    // байт-заполненном режиме, где write_pos & MASK == read_pos & MASK и
    // producer физически перезаписывает слот, который читает consumer.
    // С маленькими сообщениями переполнение наступает по счётчику задолго
    // до байтового, write далеко впереди read, и гонка не открывается.
    const PAYLOAD: usize = 60_000;
    // seq-маркеры в трёх точках сообщения. Если producer перезапишет слот
    // в середине копирования, маркеры начала/середины/конца разойдутся.
    const MARK0: usize = 0;
    const MARK1: usize = PAYLOAD / 2;
    const MARK2: usize = PAYLOAD - 4;

    /// Проставить seq в три маркера переиспользуемого буфера (без аллокаций
    /// в горячем цикле — producer должен быть быстрым, чтобы успевать
    /// перезаписывать слот во время копирования consumer-ом).
    fn stamp(buf: &mut [u8], seq: u32) {
        let s = seq.to_le_bytes();
        buf[MARK0..MARK0 + 4].copy_from_slice(&s);
        buf[MARK1..MARK1 + 4].copy_from_slice(&s);
        buf[MARK2..MARK2 + 4].copy_from_slice(&s);
    }

    /// Err, если сообщение «порвано»: маркеры начала/середины/конца не совпали.
    // Примечание: `use super::*` втягивает crate-овый `Result<T>` (ошибка = ShmError),
    // поэтому здесь используем полностью квалифицированный std-Result для String-ошибки.
    fn check(msg: &[u8]) -> std::result::Result<(), String> {
        if msg.len() != PAYLOAD {
            return Err(format!("bad len {}", msg.len()));
        }
        let m0 = u32::from_le_bytes([msg[MARK0], msg[MARK0 + 1], msg[MARK0 + 2], msg[MARK0 + 3]]);
        let m1 = u32::from_le_bytes([msg[MARK1], msg[MARK1 + 1], msg[MARK1 + 2], msg[MARK1 + 3]]);
        let m2 = u32::from_le_bytes([msg[MARK2], msg[MARK2 + 1], msg[MARK2 + 2], msg[MARK2 + 3]]);
        if m0 != m1 || m0 != m2 {
            return Err(format!("torn: m0={m0} m1={m1} m2={m2}"));
        }
        Ok(())
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "seqlock-чтение при перезаписи -- гонка по модели Rust by design"
    )]
    fn overflow_does_not_tear_messages() {
        let (ring, _mem) = make_ring();
        let ring = Arc::new(ring);
        let stop = Arc::new(AtomicBool::new(false));
        let torn = Arc::new(AtomicU64::new(0));
        let reads = Arc::new(AtomicU64::new(0));

        let producer = {
            let ring = ring.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                // Переиспользуемый буфер: producer на полной скорости держит
                // кольцо байт-заполненным, постоянно перезаписывая старое.
                let mut buf = vec![0u8; PAYLOAD];
                let mut seq: u32 = 1;
                while !stop.load(O::Acquire) {
                    stamp(&mut buf, seq);
                    let _ = ring.write_message(&buf); // overwrite разрешён
                    seq = seq.wrapping_add(1);
                }
            })
        };

        let consumer = {
            let stop = stop.clone();
            let torn = torn.clone();
            let reads = reads.clone();
            thread::spawn(move || {
                let mut out = Vec::with_capacity(PAYLOAD);
                while !stop.load(O::Acquire) {
                    match ring.read_message(&mut out) {
                        Ok(_) => {
                            if check(&out).is_err() {
                                torn.fetch_add(1, O::AcqRel);
                            }
                            reads.fetch_add(1, O::AcqRel);
                            // Лёгкая задержка: consumer чуть медленнее producer-а
                            // -> кольцо остаётся заполненным -> producer пишет
                            // ровно в слот, который мы читаем.
                            for _ in 0..400 {
                                std::hint::spin_loop();
                            }
                        }
                        Err(_) => thread::yield_now(),
                    }
                }
            })
        };

        thread::sleep(Duration::from_millis(800));
        stop.store(true, O::Release);
        producer.join().unwrap();
        consumer.join().unwrap();

        let reads = reads.load(O::Acquire);
        let torn = torn.load(O::Acquire);
        assert!(reads > 0, "consumer не прочитал ни одного сообщения");
        assert_eq!(
            torn, 0,
            "обнаружены порванные сообщения: {torn} (из {reads} прочитанных)"
        );
    }
}

#[cfg(test)]
mod try_write_tests {
    use super::overflow_race_tests::{RING_CAPACITY, make_ring, make_ring_with};
    use super::*;

    const MAX_MESSAGES: u32 = DEFAULT_MAX_MESSAGES;
    const MAX_MESSAGE_SIZE: usize = DEFAULT_MAX_MESSAGE_SIZE;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as O};
    use std::thread;

    /// Детерминированный xorshift64* -- без внешних зависимостей (proptest
    /// в крейт не тянем: правило «никаких зависимостей без крайней нужды»).
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn drop_count(ring: &RingBuffer) -> u32 {
        ring.header().drop_count.load(O::Acquire)
    }

    /// Payload с seq в начале/середине/конце и детерминированным телом --
    /// по нему видно и потерю/перестановку, и порванное сообщение.
    fn make_payload(seq: u32, len: usize) -> Vec<u8> {
        let mut v: Vec<u8> = (0..len)
            .map(|i| (i as u32).wrapping_mul(31).wrapping_add(seq) as u8)
            .collect();
        if len >= 12 {
            let s = seq.to_le_bytes();
            v[..4].copy_from_slice(&s);
            let mid = len / 2 - 2;
            v[mid..mid + 4].copy_from_slice(&s);
            v[len - 4..].copy_from_slice(&s);
        }
        v
    }

    #[test]
    fn free_space_of_empty_ring() {
        let (ring, _mem) = make_ring();
        let free = ring.free_space();
        assert_eq!(free.bytes, RING_CAPACITY);
        assert_eq!(free.messages, MAX_MESSAGES);
        assert_eq!(free.max_payload(), MAX_MESSAGE_SIZE);
        assert!(free.fits(MAX_MESSAGE_SIZE));
    }

    #[test]
    fn free_space_helpers_edge_cases() {
        let fs = |bytes, messages| FreeSpace {
            bytes,
            messages,
            max_message_size: MAX_MESSAGE_SIZE,
        };
        assert_eq!(FreeSpace::ZERO.max_payload(), 0);
        assert!(!FreeSpace::ZERO.fits(MIN_MESSAGE_SIZE));
        // Хватает ровно на минимальный кадр (8 + 1 -> 16 с выравниванием).
        assert!(fs(16, 1).fits(8));
        assert_eq!(fs(16, 1).max_payload(), 8);
        assert!(!fs(15, 1).fits(1));
        assert_eq!(fs(15, 1).max_payload(), 0);
        // Байт много, слотов нет.
        assert!(!fs(RING_CAPACITY, 0).fits(10));
        assert_eq!(fs(RING_CAPACITY, 0).max_payload(), 0);
        // Недопустимые длины не «влезают» никогда.
        assert!(!fs(RING_CAPACITY, 10).fits(0));
        assert!(!fs(RING_CAPACITY, 10).fits(MAX_MESSAGE_SIZE + 1));
        assert_eq!(fs(1000, 1).max_payload(), 992);
    }

    #[test]
    fn try_write_rejects_bad_lengths_without_side_effects() {
        let (ring, _mem) = make_ring();
        assert_eq!(
            ring.try_write_message(&[]).err(),
            Some(ShmError::MessageTooSmall)
        );
        assert_eq!(
            ring.try_write_message(&vec![0u8; MAX_MESSAGE_SIZE + 1])
                .err(),
            Some(ShmError::MessageTooLarge)
        );
        assert_eq!(ring.free_space().bytes, RING_CAPACITY);
        assert!(ring.try_write_message(&vec![7u8; MAX_MESSAGE_SIZE]).is_ok());
    }

    #[test]
    fn free_space_tracks_writes_and_reads_exactly() {
        let (ring, _mem) = make_ring();
        let mut out = Vec::new();
        ring.try_write_message(&[1u8; 100]).unwrap();
        ring.try_write_message(&[2u8; 10]).unwrap();
        let free = ring.free_space();
        assert_eq!(free.bytes, RING_CAPACITY - frame_size(100) - frame_size(10));
        assert_eq!(free.messages, MAX_MESSAGES - 2);
        ring.read_message(&mut out).unwrap();
        assert_eq!(ring.free_space().bytes, RING_CAPACITY - frame_size(10));
        assert_eq!(ring.free_space().messages, MAX_MESSAGES - 1);
    }

    /// Кольцо, заполненное по БАЙТАМ: try_write отвечает QueueFull, ничего
    /// не вытесняет; после чтения одного сообщения запись снова проходит.
    #[test]
    fn try_write_full_by_bytes_never_overwrites_and_resumes() {
        let (ring, _mem) = make_ring();
        const LEN: usize = 60_000;
        let mut written = 0u32;
        loop {
            match ring.try_write_message(&make_payload(written, LEN)) {
                Ok(outcome) => {
                    assert_eq!(outcome.overwritten, 0);
                    written += 1;
                }
                Err(ShmError::QueueFull) => break,
                Err(err) => panic!("unexpected {err:?}"),
            }
        }
        assert_eq!(written as usize, RING_CAPACITY / frame_size(LEN));
        let before = ring.free_space();
        assert!(!before.fits(LEN));
        assert_eq!(drop_count(&ring), 0);
        // Повторные попытки ничего не меняют.
        for _ in 0..3 {
            assert_eq!(
                ring.try_write_message(&make_payload(999, LEN)).err(),
                Some(ShmError::QueueFull)
            );
        }
        assert_eq!(ring.free_space(), before);
        assert_eq!(ring.header().message_count.load(O::Acquire), written);

        let mut out = Vec::new();
        ring.read_message(&mut out).unwrap();
        assert_eq!(
            out,
            make_payload(0, LEN),
            "первое сообщение не должно быть затёрто"
        );
        assert!(ring.free_space().fits(LEN));
        ring.try_write_message(&make_payload(written, LEN)).unwrap();
        // Всё прочитанное -- строго по порядку, без пропусков.
        for seq in 1..=written {
            ring.read_message(&mut out).unwrap();
            assert_eq!(out, make_payload(seq, LEN));
        }
        assert!(ring.is_empty());
        assert_eq!(drop_count(&ring), 0);
    }

    /// Граница доверия (аудит 2026-10-16): пир пишет мусор в заголовок
    /// кольца или кадра -- читатель отвечает `Corrupted`, не выходя за
    /// опубликованные данные, не зацикливаясь и не паникуя.
    #[test]
    fn hostile_reader_side_corruption_is_detected() {
        let (ring, _mem) = make_ring();
        let header = ring.header();
        let mut out = Vec::new();

        // Длина кадра больше лимита кольца.
        ring.try_write_message(b"payload").unwrap();
        ring.header_word(0).store(u32::MAX, O::Relaxed);
        assert_eq!(ring.read_message(&mut out), Err(ShmError::Corrupted));

        // Кадр длиннее опубликованных данных (write_pos указывает внутрь).
        ring.header_word(0).store(64, O::Relaxed);
        assert_eq!(ring.read_message(&mut out), Err(ShmError::Corrupted));

        // Занятость больше ёмкости.
        header
            .write_pos
            .store(RING_CAPACITY as u32 + 64, O::Relaxed);
        assert_eq!(ring.read_message(&mut out), Err(ShmError::Corrupted));
        assert_eq!(ring.free_space().bytes, 0);
        assert_eq!(ring.try_write_message(b"xx"), Err(ShmError::Corrupted));
        assert_eq!(ring.write_message(b"xx"), Err(ShmError::Corrupted));

        // Невыровненная позиция чтения.
        header.write_pos.store(64, O::Relaxed);
        header.read_pos.store(3, O::Relaxed);
        assert_eq!(ring.read_message(&mut out), Err(ShmError::Corrupted));
    }

    /// Регрессия (security review 2026-10-16): читатель-пир ставит
    /// невыровненную позицию записи у конца кольца -- раньше `commit`
    /// писал второе слово заголовка кадра за пределы кольца.
    #[test]
    fn hostile_writer_position_cannot_write_out_of_bounds() {
        let (ring, _mem) = make_ring();
        let header = ring.header();
        let edge = RING_CAPACITY as u32 - 4;
        header.write_pos.store(edge, O::Relaxed);
        header.read_pos.store(edge, O::Relaxed);
        assert_eq!(ring.try_write_message(b"xx"), Err(ShmError::Corrupted));
        assert_eq!(ring.write_message(b"xx"), Err(ShmError::Corrupted));
        // Выровненная позиция записи при невыровненной позиции чтения.
        header.write_pos.store(64, O::Relaxed);
        header.read_pos.store(60, O::Relaxed);
        assert_eq!(ring.try_write_message(b"xx"), Err(ShmError::Corrupted));
        assert_eq!(ring.write_message(b"xx"), Err(ShmError::Corrupted));
    }

    /// Испорченный счётчик сообщений: пустое кольцо со «счётчиком» на
    /// пределе не зацикливает запись с перезаписью, а нулевой счётчик при
    /// чтении не уходит в `u32::MAX` (кольцо навсегда «полно»).
    #[test]
    fn hostile_message_count_is_contained() {
        let (ring, _mem) = make_ring();
        let header = ring.header();
        let mut out = Vec::new();

        // Счётчик -- подсказка: пустое по позициям кольцо принимает кадр при
        // любом его значении (ни `Corrupted`, ни `QueueFull`).
        header.message_count.store(u32::MAX, O::Relaxed);
        assert!(ring.free_space().fits(2));
        let outcome = ring.write_message(b"xx").unwrap();
        assert_eq!(outcome.overwritten, 0);
        assert_eq!(ring.read_message(&mut out), Ok(2));
        header.message_count.store(u32::MAX, O::Relaxed);
        ring.try_write_message(b"yy").unwrap();
        assert_eq!(ring.read_message(&mut out), Ok(2));
        assert_eq!(ring.read_message(&mut out), Err(ShmError::QueueEmpty));

        // Непустое кольцо при «исчерпанном» счётчике: try_write отказывает,
        // write вытесняет до пустого кольца и пишет -- цикл конечен.
        ring.try_write_message(b"a1").unwrap();
        ring.try_write_message(b"a2").unwrap();
        header.message_count.store(u32::MAX, O::Relaxed);
        assert_eq!(ring.try_write_message(b"zz"), Err(ShmError::QueueFull));
        let outcome = ring.write_message(b"zz").unwrap();
        assert_eq!(outcome.overwritten, 2);
        assert!(outcome.was_empty, "вытеснение опустошило кольцо");
        assert_eq!(ring.read_message(&mut out), Ok(2));
        assert_eq!(out, b"zz");
        assert_eq!(ring.read_message(&mut out), Err(ShmError::QueueEmpty));

        header.message_count.store(0, O::Relaxed);
        ring.try_write_message(b"one").unwrap();
        header.message_count.store(0, O::Relaxed);
        assert_eq!(ring.read_message(&mut out), Ok(3));
        assert_eq!(
            header.message_count.load(O::Relaxed),
            0,
            "без ухода ниже нуля"
        );
        assert!(ring.free_space().fits(8));
    }

    /// Кольцо, заполненное по СЧЁТЧИКУ (`MAX_MESSAGES`), при почти пустых байтах.
    #[test]
    fn try_write_full_by_count() {
        let (ring, _mem) = make_ring();
        for i in 0..MAX_MESSAGES {
            ring.try_write_message(&i.to_le_bytes()).unwrap();
        }
        let free = ring.free_space();
        assert_eq!(free.messages, 0);
        assert!(free.bytes > RING_CAPACITY / 2);
        assert_eq!(
            ring.try_write_message(b"xx").err(),
            Some(ShmError::QueueFull)
        );
        let mut out = Vec::new();
        ring.read_message(&mut out).unwrap();
        assert_eq!(out, 0u32.to_le_bytes());
        ring.try_write_message(b"xx").unwrap();
        assert_eq!(drop_count(&ring), 0);
    }

    /// Рандомизированная проверка кадрирования против модели (VecDeque):
    /// случайные длины (с упором на границы 2/65535 и перенос через конец
    /// кольца), случайное чередование записи и чтения, многократный wrap
    /// позиций. Однопоточно предсказание QueueFull точное, поэтому модель
    /// сверяет и сам факт отказа, и содержимое каждого прочитанного сообщения.
    #[test]
    fn randomized_framing_matches_model() {
        let (ring, _mem) = make_ring();
        let steps = if cfg!(miri) { 300 } else { 60_000 };
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut model: VecDeque<Vec<u8>> = VecDeque::new();
        let mut used = 0usize;
        let mut out = Vec::new();
        let mut seq = 0u32;
        let mut fulls = 0u32;
        for _ in 0..steps {
            if rng.below(100) < 55 {
                let len = match rng.below(10) {
                    0 => MIN_MESSAGE_SIZE,
                    1 => MAX_MESSAGE_SIZE,
                    2..=5 => 2 + rng.below(64) as usize,
                    _ => 2 + rng.below((MAX_MESSAGE_SIZE - 1) as u64) as usize,
                };
                let payload = make_payload(seq, len);
                let fits_model =
                    used + frame_size(len) <= RING_CAPACITY && (model.len() as u32) < MAX_MESSAGES;
                assert_eq!(ring.free_space().fits(len), fits_model);
                match ring.try_write_message(&payload) {
                    Ok(o) => {
                        assert!(fits_model, "try_write прошёл, хотя места нет по модели");
                        assert_eq!(o.overwritten, 0);
                        assert_eq!(o.was_empty, model.is_empty());
                        used += frame_size(len);
                        model.push_back(payload);
                        seq += 1;
                    }
                    Err(ShmError::QueueFull) => {
                        assert!(!fits_model, "ложный QueueFull");
                        fulls += 1;
                    }
                    Err(err) => panic!("unexpected {err:?}"),
                }
            } else {
                match ring.read_message(&mut out) {
                    Ok(n) => {
                        let expect = model.pop_front().expect("кольцо отдало лишнее");
                        assert_eq!(n, expect.len());
                        assert_eq!(out, expect);
                        used -= frame_size(n);
                    }
                    Err(ShmError::QueueEmpty) => assert!(model.is_empty()),
                    Err(err) => panic!("unexpected {err:?}"),
                }
            }
            let free = ring.free_space();
            assert_eq!(free.bytes, RING_CAPACITY - used);
            assert_eq!(free.messages, MAX_MESSAGES - model.len() as u32);
        }
        assert_eq!(drop_count(&ring), 0);
        if !cfg!(miri) {
            assert!(fulls > 0, "тест ни разу не упёрся в полное кольцо");
            let total_written = ring.header().write_pos.load(O::Acquire) as usize;
            assert!(
                total_written / RING_CAPACITY >= 10,
                "позиции обернулись всего {} раз",
                total_written / RING_CAPACITY
            );
        }
    }

    /// Детерминированный интерливинг потерянного сигнала `DATA` (Task 21):
    /// читатель CAS-ом забрал последний кадр и «замер» до уменьшения
    /// `message_count`, его снимок позиций -- «пусто»; писатель кладёт
    /// следующий кадр; читатель досчитывает счётчик. Если читатель уснёт по
    /// своему снимку, разбудить его может только сигнал писателя: запись
    /// обязана вернуть `was_empty`. Оба пути записи: без перезаписи и с ней.
    #[test]
    fn data_signal_survives_reader_draining_concurrently() {
        type Write = fn(&RingBuffer, &[u8]) -> Result<WriteOutcome>;
        let paths: [(&str, Write); 2] = [
            ("try_write_message", RingBuffer::try_write_message),
            ("write_message", RingBuffer::write_message),
        ];
        for (path, write) in paths {
            let (ring, _mem) = make_ring();
            let header = ring.header();
            let mut out = Vec::new();
            write(&ring, &[1u8; 16]).unwrap();

            // Читатель: шаги `read_message` до `decrement_count`.
            let read0 = header.read_pos.load(O::Acquire);
            let write0 = header.write_pos.load(O::Acquire);
            let frame = ring.frame_at(read0, write0).unwrap().unwrap();
            header
                .read_pos
                .compare_exchange(read0, read0 + frame.total, O::AcqRel, O::Acquire)
                .unwrap();
            // Снимок читателя ДО публикации нового кадра: пусто по позициям.
            let snapshot_read = header.read_pos.load(O::Acquire);
            let snapshot_write = header.write_pos.load(O::Acquire);
            assert!(
                ring.frame_at(snapshot_read, snapshot_write)
                    .unwrap()
                    .is_none()
            );

            // Писатель кладёт следующий кадр, пока счётчик ещё 1.
            let outcome = write(&ring, &[2u8; 16]).unwrap();
            // Перепроверка читателя по устаревшему снимку после барьера
            // обязана провалиться: кадр опубликован.
            assert!(
                !ring.confirm_empty(snapshot_write),
                "{path}: перепроверка не увидела новый write_pos"
            );
            // Читатель досчитывает message_count.
            ring.decrement_count();

            assert!(
                outcome.was_empty,
                "{path}: читатель уже выбрал всё до этого кадра -- без сигнала \
                 DATA он уснёт навсегда"
            );
            assert_eq!(outcome.overwritten, 0);
            assert_eq!(ring.read_message(&mut out).unwrap(), 16, "{path}");
            assert_eq!(out, [2u8; 16]);
            assert_eq!(
                ring.read_message(&mut out).err(),
                Some(ShmError::QueueEmpty)
            );
        }
    }

    /// Ветка вытеснения `write_message` (лимит сообщений), пока читатель
    /// «замер» между CAS `read_pos` и `decrement_count`: писатель вытесняет
    /// следующий за забранным кадр, кольцо НЕ пусто -- сигнал не нужен
    /// (`was_empty == false`), а устаревший снимок читателя перепроверкой
    /// не подтверждается; читатель дочитывает всё, новый кадр -- последним.
    #[test]
    fn data_signal_on_eviction_by_count_with_reader_mid_step() {
        let (ring, _mem) = make_ring();
        let header = ring.header();
        let mut out = Vec::new();
        for i in 0..MAX_MESSAGES {
            ring.write_message(&i.to_le_bytes()).unwrap();
        }
        // Читатель забрал кадр 0 (CAS), счётчик ещё MAX_MESSAGES.
        let read0 = header.read_pos.load(O::Acquire);
        let write0 = header.write_pos.load(O::Acquire);
        let frame = ring.frame_at(read0, write0).unwrap().unwrap();
        header
            .read_pos
            .compare_exchange(read0, read0 + frame.total, O::AcqRel, O::Acquire)
            .unwrap();

        let outcome = ring.write_message(&u32::MAX.to_le_bytes()).unwrap();
        assert_eq!(outcome.overwritten, 1, "лимит сообщений -- вытеснение");
        assert!(
            !outcome.was_empty,
            "читатель не выбрал всё до кадра -- он не уснёт, сигнал не нужен"
        );
        assert!(!ring.confirm_empty(write0), "снимок устарел -- кадр виден");
        ring.decrement_count();

        // Кадр 1 вытеснен; читатель получает 2..MAX_MESSAGES и новый кадр.
        for i in 2..MAX_MESSAGES {
            assert_eq!(ring.read_message(&mut out).unwrap(), 4);
            assert_eq!(out, i.to_le_bytes());
        }
        assert_eq!(ring.read_message(&mut out).unwrap(), 4);
        assert_eq!(out, u32::MAX.to_le_bytes());
        assert_eq!(
            ring.read_message(&mut out).err(),
            Some(ShmError::QueueEmpty)
        );
    }

    /// Ветка вытеснения по байтам, опустошающая кольцо: писатель сам выбрал
    /// всё до своего кадра (`read_pos ==` прежнему `write_pos`) -- решение
    /// «сигналить» верно (читатель с прежним снимком «пусто» иначе уснул
    /// бы), снимок до записи перепроверкой не подтверждается.
    #[test]
    fn data_signal_on_eviction_by_bytes_that_empties_ring() {
        let config = RingConfig::new(4096);
        let (ring, _mem) = make_ring_with(&config);
        let header = ring.header();
        let mut out = Vec::new();
        const LEN: usize = 3000;
        ring.write_message(&[1u8; LEN]).unwrap();
        let write0 = header.write_pos.load(O::Acquire);

        let outcome = ring.write_message(&[2u8; LEN]).unwrap();
        assert_eq!(
            outcome.overwritten, 1,
            "два кадра по 3000 в 4 КиБ не входят"
        );
        assert!(
            outcome.was_empty,
            "вытеснение выбрало всё до кадра -- read_pos == прежний write_pos"
        );
        assert_eq!(header.read_pos.load(O::Acquire), write0);
        assert!(!ring.confirm_empty(write0), "снимок устарел -- кадр виден");
        assert_eq!(ring.read_message(&mut out).unwrap(), LEN);
        assert_eq!(out, [2u8; LEN]);
        assert_eq!(
            ring.read_message(&mut out).err(),
            Some(ShmError::QueueEmpty)
        );
    }

    /// Счётчик отстаёт от `read_pos` (`max_messages = 1`): читатель CAS-ом
    /// забрал единственный кадр и «замер» до `decrement_count` -- кольцо
    /// пусто по позициям, а `message_count == max_messages`. Это законная
    /// гонка, а не порча: оба пути записи обязаны принять кадр (пустое
    /// кольцо принимает любой кадр, помещающийся по байтам), без вытеснения
    /// и без `QueueFull`; счётчик потом сходится с насыщением.
    #[test]
    fn lagging_count_does_not_reject_write_into_empty_ring() {
        type Write = fn(&RingBuffer, &[u8]) -> Result<WriteOutcome>;
        let paths: [(&str, Write); 2] = [
            ("write_message", RingBuffer::write_message),
            ("try_write_message", RingBuffer::try_write_message),
        ];
        for (path, write) in paths {
            let config = RingConfig::new(4096).max_messages(1);
            let (ring, _mem) = make_ring_with(&config);
            let header = ring.header();
            let mut out = Vec::new();
            write(&ring, &[1u8; 16]).unwrap();

            // Читатель: CAS read_pos за кадр, decrement_count ещё не сделан.
            let read0 = header.read_pos.load(O::Acquire);
            let write0 = header.write_pos.load(O::Acquire);
            let frame = ring.frame_at(read0, write0).unwrap().unwrap();
            header
                .read_pos
                .compare_exchange(read0, read0 + frame.total, O::AcqRel, O::Acquire)
                .unwrap();
            assert_eq!(header.message_count.load(O::Acquire), 1);

            let outcome = write(&ring, &[2u8; 16])
                .unwrap_or_else(|e| panic!("{path}: пустое кольцо отвергло кадр: {e:?}"));
            assert_eq!(outcome.overwritten, 0, "{path}: вытеснять нечего");
            assert!(outcome.was_empty, "{path}: читатель выбрал всё до кадра");

            // Читатель досчитывает; счётчик -- 1 (новый кадр), не ниже нуля.
            ring.decrement_count();
            assert_eq!(header.message_count.load(O::Acquire), 1, "{path}");
            assert_eq!(ring.read_message(&mut out).unwrap(), 16, "{path}");
            assert_eq!(out, [2u8; 16]);
            assert_eq!(header.message_count.load(O::Acquire), 0, "{path}");
            assert_eq!(
                ring.read_message(&mut out).err(),
                Some(ShmError::QueueEmpty)
            );
            assert_eq!(header.message_count.load(O::Acquire), 0, "{path}");
            assert_eq!(drop_count(&ring), 0, "{path}");
        }
    }

    /// Вторая половина протокола: снимок читателя «пусто» снят ДО
    /// публикации, перепроверка после барьера -- ПОСЛЕ. Читатель обязан
    /// увидеть кадр (не уснуть), а писатель в этом порядке тоже сигналит:
    /// хотя бы одна сторона видит запись другой, здесь -- обе.
    #[test]
    fn reader_recheck_sees_frame_published_after_empty_snapshot() {
        let (ring, _mem) = make_ring();
        let header = ring.header();
        let mut out = Vec::new();
        let snapshot_write = header.write_pos.load(O::Acquire);
        assert_eq!(header.read_pos.load(O::Acquire), snapshot_write);
        assert!(ring.confirm_empty(snapshot_write), "пустое кольцо -- пусто");

        let outcome = ring.try_write_message(&[3u8; 16]).unwrap();
        assert!(outcome.was_empty);
        assert!(
            !ring.confirm_empty(snapshot_write),
            "перепроверка после барьера обязана увидеть новый write_pos"
        );
        assert!(!ring.is_empty());
        assert_eq!(ring.read_message(&mut out).unwrap(), 16);
        assert!(ring.is_empty());

        // Читатель отстаёт (в кольце два кадра): сигнал не нужен -- он не
        // уснёт, не дочитав до нового кадра.
        ring.try_write_message(&[4u8; 16]).unwrap();
        let outcome = ring.try_write_message(&[5u8; 16]).unwrap();
        assert!(!outcome.was_empty);
    }

    /// Заявка писателя: читатель снимает её и «будит» только когда места
    /// реально хватает под заявленный кадр.
    #[test]
    fn space_waiter_is_taken_only_when_enough_space() {
        let (ring, _mem) = make_ring();
        const LEN: usize = 60_000;
        while ring.try_write_message(&[0u8; LEN]).is_ok() {}
        let mut out = Vec::new();

        // Никто не ждёт -- сигналить нечего.
        ring.read_message(&mut out).unwrap();
        assert!(!ring.take_space_waiter());
        while ring.try_write_message(&[0u8; LEN]).is_ok() {}

        // Ждём место под 2 кадра: после первого чтения его ещё мало.
        let need = 2 * frame_size(LEN) as u32;
        ring.arm_space_waiter(need);
        ring.read_message(&mut out).unwrap();
        assert!(!ring.take_space_waiter(), "места под заявку ещё нет");
        ring.read_message(&mut out).unwrap();
        assert!(
            ring.take_space_waiter(),
            "места хватает -- заявку надо снять"
        );
        assert!(!ring.take_space_waiter(), "заявка снимается ровно один раз");
        assert_eq!(ring.header().space_waiter.load(O::Acquire), 0);

        // Снятая самим писателем заявка не будит.
        ring.arm_space_waiter(frame_size(LEN) as u32);
        ring.disarm_space_waiter();
        assert!(!ring.take_space_waiter());
    }

    /// `wait_for_space` без события (anonymous-путь или читатель старой
    /// версии, не знающий про заявку): заявка + опрос.
    #[test]
    fn wait_for_space_polls_without_event() {
        let (ring, _mem) = make_ring();
        let ring = Arc::new(ring);
        const LEN: usize = 60_000;
        while ring.try_write_message(&[0u8; LEN]).is_ok() {}
        assert_eq!(
            ring.wait_for_space(
                LEN,
                None,
                None,
                None,
                Some(Duration::from_millis(20)),
                SpinPolicy::NONE
            ),
            Ok(false)
        );
        assert_eq!(
            ring.header().space_waiter.load(O::Acquire),
            0,
            "таймаут снимает заявку"
        );

        let reader = {
            let ring = ring.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(30));
                let mut out = Vec::new();
                ring.read_message(&mut out).unwrap();
                // «Старый» читатель: заявку не смотрит.
            })
        };
        assert_eq!(
            ring.wait_for_space(
                LEN,
                None,
                None,
                None,
                Some(Duration::from_secs(5)),
                SpinPolicy::NONE
            ),
            Ok(true)
        );
        reader.join().unwrap();
        ring.try_write_message(&[1u8; LEN]).unwrap();
        assert_eq!(
            ring.wait_for_space(0, None, None, None, None, SpinPolicy::NONE),
            Err(ShmError::MessageTooSmall),
            "длина проверяется до ожидания"
        );
    }

    fn kernel_waits() -> u32 {
        SPACE_KERNEL_WAITS.with(std::cell::Cell::get)
    }

    /// Заполнить кольцо кадрами по `len` байт.
    fn fill(ring: &RingBuffer, len: usize) {
        while ring.try_write_message(&vec![0u8; len]).is_ok() {}
    }

    /// 0.9: пробуждение по месту -- ОДНО ожидание в ядре на всё время
    /// ожидания (раньше -- срезы по 50 мс: за 300 мс было бы ~6 пробуждений).
    #[test]
    fn wait_for_space_wakes_on_space_without_slicing() {
        let (ring, _mem) = make_ring();
        let ring = Arc::new(ring);
        const LEN: usize = 60_000;
        fill(&ring, LEN);
        let space = Arc::new(EventHandle::create_unnamed(false).unwrap());
        let reader = {
            let (ring, space) = (ring.clone(), space.clone());
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(300));
                let mut out = Vec::new();
                ring.read_message(&mut out).unwrap();
                // Читатель 0.8+: снимает заявку и сигналит SPACE.
                assert!(
                    ring.take_space_waiter(),
                    "заявка писателя должна быть видна"
                );
                space.set().unwrap();
            })
        };
        let before = kernel_waits();
        let t0 = Instant::now();
        assert_eq!(
            ring.wait_for_space(LEN, Some(&space), None, None, None, SpinPolicy::NONE),
            Ok(true)
        );
        let waited = t0.elapsed();
        reader.join().unwrap();
        assert_eq!(kernel_waits() - before, 1, "ожидание нарезано на срезы");
        assert!(
            waited >= Duration::from_millis(250),
            "проснулись раньше места"
        );
    }

    /// 0.9: смерть читателя будит писателя сразу и без срезов, даже при
    /// `timeout = None` (единственное ожидание -- до сигнала процесса).
    #[test]
    fn wait_for_space_wakes_on_peer_death_without_slicing() {
        let (ring, _mem) = make_ring();
        const LEN: usize = 60_000;
        fill(&ring, LEN);
        let space = EventHandle::create_unnamed(false).unwrap();
        // «Читатель» -- процесс, живущий ~1 с.
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "ping -n 2 127.0.0.1 >NUL"])
            .spawn()
            .expect("spawn child");
        let watch = ProcessWatch::open(child.id()).expect("watch child");
        let before = kernel_waits();
        let t0 = Instant::now();
        assert_eq!(
            ring.wait_for_space(
                LEN,
                Some(&space),
                Some(&watch),
                None,
                None,
                SpinPolicy::NONE
            ),
            Err(ShmError::PeerDied)
        );
        let waited = t0.elapsed();
        child.wait().unwrap();
        assert_eq!(kernel_waits() - before, 1, "ожидание нарезано на срезы");
        assert!(
            waited >= Duration::from_millis(300),
            "PeerDied до смерти: {waited:?}"
        );
        assert_eq!(
            ring.header().space_waiter.load(O::Acquire),
            0,
            "заявка снята"
        );
    }

    /// 0.9: штатное отключение читателя (`DISCONNECT` + состояние handshake)
    /// будит писателя (`NotConnected`) и возвращает событие взведённым для
    /// других ждущих; устаревший сигнал (пир на месте) поглощается.
    #[test]
    fn wait_for_space_wakes_on_disconnect_and_ignores_stale_signal() {
        let (ring, _mem) = make_ring();
        let ring = Arc::new(ring);
        const LEN: usize = 60_000;
        fill(&ring, LEN);
        let space = Arc::new(EventHandle::create_unnamed(false).unwrap());
        let disconnect = Arc::new(EventHandle::create_unnamed(false).unwrap());
        let left = Arc::new(AtomicBool::new(false));
        let peer_left = {
            let left = left.clone();
            move || left.load(O::Acquire)
        };
        let watch = DisconnectWatch {
            event: &disconnect,
            peer_left: &peer_left,
        };

        // Устаревший DISCONNECT (пир на месте), затем настоящее место.
        disconnect.set().unwrap();
        let reader = {
            let (ring, space) = (ring.clone(), space.clone());
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(150));
                let mut out = Vec::new();
                ring.read_message(&mut out).unwrap();
                assert!(ring.take_space_waiter());
                space.set().unwrap();
            })
        };
        let before = kernel_waits();
        assert_eq!(
            ring.wait_for_space(
                LEN,
                Some(&space),
                None,
                Some(&watch),
                None,
                SpinPolicy::NONE
            ),
            Ok(true)
        );
        reader.join().unwrap();
        assert_eq!(kernel_waits() - before, 2, "устаревший сигнал + место");
        assert!(
            !disconnect.wait(Some(Duration::ZERO)).unwrap(),
            "устаревший сигнал поглощён"
        );

        // Настоящее отключение.
        fill(&ring, LEN);
        let leaver = {
            let disconnect = disconnect.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(150));
                left.store(true, O::Release);
                disconnect.set().unwrap();
            })
        };
        let before = kernel_waits();
        assert_eq!(
            ring.wait_for_space(
                LEN,
                Some(&space),
                None,
                Some(&watch),
                None,
                SpinPolicy::NONE
            ),
            Err(ShmError::NotConnected)
        );
        leaver.join().unwrap();
        assert_eq!(kernel_waits() - before, 1);
        assert!(
            disconnect.wait(Some(Duration::ZERO)).unwrap(),
            "сигнал отключения возвращён остальным ждущим"
        );
    }

    /// Писатель быстрее читателя: try_write упирается в QueueFull, но ни одно
    /// сообщение не теряется, не переставляется и не рвётся; после того как
    /// читатель разгребает кольцо, запись продолжается.
    #[test]
    fn lossless_producer_faster_than_consumer() {
        let (ring, _mem) = make_ring();
        let ring = Arc::new(ring);
        let total: u32 = if cfg!(miri) { 60 } else { 20_000 };
        let fulls = Arc::new(AtomicU64::new(0));
        let done = Arc::new(AtomicBool::new(false));

        let producer = {
            let ring = ring.clone();
            let fulls = fulls.clone();
            thread::spawn(move || {
                let mut rng = Rng(42);
                for seq in 0..total {
                    let len = if cfg!(miri) {
                        16 + rng.below(64) as usize
                    } else {
                        12 + rng.below(40_000) as usize
                    };
                    let payload = make_payload(seq, len);
                    loop {
                        match ring.try_write_message(&payload) {
                            Ok(o) => {
                                assert_eq!(o.overwritten, 0);
                                break;
                            }
                            Err(ShmError::QueueFull) => {
                                fulls.fetch_add(1, O::Relaxed);
                                thread::yield_now();
                            }
                            Err(err) => panic!("unexpected {err:?}"),
                        }
                    }
                }
            })
        };

        let consumer = {
            let ring = ring.clone();
            let done = done.clone();
            thread::spawn(move || {
                let mut out = Vec::new();
                let mut expected = 0u32;
                while expected < total {
                    match ring.read_message(&mut out) {
                        Ok(len) => {
                            let seq = u32::from_le_bytes(out[..4].try_into().unwrap());
                            assert_eq!(seq, expected, "потеря или перестановка");
                            assert_eq!(out, make_payload(seq, len), "порванное сообщение");
                            expected += 1;
                            // Читатель заметно медленнее писателя.
                            if !cfg!(miri) && expected.is_multiple_of(8) {
                                thread::sleep(Duration::from_micros(200));
                            }
                        }
                        Err(ShmError::QueueEmpty) => thread::yield_now(),
                        Err(err) => panic!("unexpected {err:?}"),
                    }
                }
                done.store(true, O::Release);
            })
        };

        producer.join().unwrap();
        consumer.join().unwrap();
        assert!(done.load(O::Acquire));
        assert_eq!(drop_count(&ring), 0);
        assert!(ring.is_empty());
        if !cfg!(miri) {
            assert!(
                fulls.load(O::Relaxed) > 0,
                "писатель ни разу не упёрся в полное кольцо"
            );
        }
    }

    /// Микробенчмарк: `cargo test --release --lib -- --ignored --nocapture bench_`
    #[test]
    #[ignore = "бенчмарк, запускать вручную в --release"]
    fn bench_try_write_vs_write() {
        use std::hint::black_box;
        let (ring, _mem) = make_ring();
        let mut out = Vec::with_capacity(MAX_MESSAGE_SIZE);
        const ITERS: u32 = 2_000_000;
        for len in [16usize, 256, 4096] {
            let payload = vec![0xA5u8; len];
            // Прогрев.
            for _ in 0..10_000 {
                ring.write_message(&payload).unwrap();
                ring.read_message(&mut out).unwrap();
            }
            let t = Instant::now();
            for _ in 0..ITERS {
                black_box(ring.write_message(black_box(&payload)).unwrap());
                ring.read_message(&mut out).unwrap();
            }
            let write_ns = t.elapsed().as_nanos() as f64 / f64::from(ITERS);
            let t = Instant::now();
            for _ in 0..ITERS {
                black_box(ring.try_write_message(black_box(&payload)).unwrap());
                ring.read_message(&mut out).unwrap();
            }
            let try_ns = t.elapsed().as_nanos() as f64 / f64::from(ITERS);
            let t = Instant::now();
            for _ in 0..ITERS {
                black_box(ring.try_write_message(black_box(&payload)).unwrap());
                ring.read_message(&mut out).unwrap();
                black_box(ring.take_space_waiter());
            }
            let try_waiter_ns = t.elapsed().as_nanos() as f64 / f64::from(ITERS);
            let t = Instant::now();
            for _ in 0..ITERS {
                black_box(ring.free_space());
            }
            let free_ns = t.elapsed().as_nanos() as f64 / f64::from(ITERS);
            println!(
                "len={len:5}: write+read {write_ns:6.1} ns | try_write+read {try_ns:6.1} ns | \
                 try_write+read+take_space_waiter {try_waiter_ns:6.1} ns | free_space {free_ns:5.1} ns"
            );
        }
        // Отказ на полном кольце -- стоимость «холостой» попытки.
        let big = vec![0u8; 60_000];
        while ring.try_write_message(&big).is_ok() {}
        let t = Instant::now();
        for _ in 0..ITERS {
            black_box(ring.try_write_message(black_box(&big)).is_err());
        }
        let full_ns = t.elapsed().as_nanos() as f64 / f64::from(ITERS);
        println!("try_write on full ring (QueueFull): {full_ns:.1} ns");
    }
}

#[cfg(test)]
mod tests {
    //! Спин перед ожиданием в ядре (0.10, `SpinPolicy`).
    use super::overflow_race_tests::make_ring;
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::Ordering as O;
    use std::thread;

    /// Бюджет, заведомо перекрывающий задержку писателя ниже (десятки мс)
    /// на любом x86: даже при ~3 нс на итерацию -- больше 0,5 с.
    const LONG_SPIN: SpinPolicy = SpinPolicy::new(200_000_000);
    /// Задержка писателя/читателя: спин уже идёт, когда данные (место)
    /// появляются.
    const PEER_DELAY: Duration = Duration::from_millis(20);
    const LEN: usize = 60_000;

    fn data_waits() -> u32 {
        DATA_KERNEL_WAITS.with(std::cell::Cell::get)
    }

    fn space_waits() -> u32 {
        SPACE_KERNEL_WAITS.with(std::cell::Cell::get)
    }

    /// Писатель в другом потоке: через `PEER_DELAY` кладёт кадр и, как
    /// требует протокол, сигналит `DATA` при `was_empty`.
    fn spawn_writer(ring: &Arc<RingBuffer>, data: &Arc<EventHandle>) -> thread::JoinHandle<()> {
        let (ring, data) = (Arc::clone(ring), Arc::clone(data));
        thread::spawn(move || {
            thread::sleep(PEER_DELAY);
            if ring.try_write_message(&[7u8; 16]).unwrap().was_empty {
                data.set().unwrap();
            }
        })
    }

    #[test]
    fn spin_policy_defaults_to_none() {
        assert_eq!(SpinPolicy::default(), SpinPolicy::NONE);
        assert!(!SpinPolicy::NONE.is_enabled());
        assert!(SpinPolicy::new(1).is_enabled());
        assert!(
            !spin_until(SpinPolicy::NONE, None, || true),
            "без бюджета -- ни одной проверки"
        );
        let mut calls = 0;
        assert!(!spin_until(SpinPolicy::new(5), None, || {
            calls += 1;
            false
        }));
        assert_eq!(calls, 5, "бюджет -- ровно `iterations` проверок");
    }

    /// Данные, появившиеся во время спина, читаются без ожидания в ядре; без
    /// спина тот же сценарий -- ровно одно ожидание (счётчик честный).
    #[test]
    fn spin_sees_data_without_kernel_wait() {
        let (ring, _mem) = make_ring();
        let ring = Arc::new(ring);
        let data = Arc::new(EventHandle::create_unnamed(false).unwrap());
        let mut out = Vec::new();

        let writer = spawn_writer(&ring, &data);
        let before = data_waits();
        assert_eq!(
            ring.wait_for_data(&data, None, Some(Duration::from_secs(5)), LONG_SPIN),
            Ok(true)
        );
        writer.join().unwrap();
        assert_eq!(data_waits() - before, 0, "спин ушёл в ядро");
        assert_eq!(ring.read_message(&mut out), Ok(16));
        // Сигнал писателя спин не трогает: остался взведённым (одно лишнее
        // пробуждение потом), а не сброшен -- сброс мог бы потерять сигнал.
        assert!(data.wait(Some(Duration::ZERO)).unwrap());

        // Контроль: без спина -- одно ожидание в ядре до сигнала писателя.
        let writer = spawn_writer(&ring, &data);
        let before = data_waits();
        assert_eq!(
            ring.wait_for_data(&data, None, Some(Duration::from_secs(5)), SpinPolicy::NONE),
            Ok(true)
        );
        writer.join().unwrap();
        assert_eq!(data_waits() - before, 1);
        assert_eq!(ring.read_message(&mut out), Ok(16));
    }

    /// Бюджет исчерпан без данных -- одно обычное ожидание в ядре (не
    /// повторный спин); нулевой таймаут не спинит вовсе.
    #[test]
    fn exhausted_spin_falls_back_to_single_kernel_wait() {
        let (ring, _mem) = make_ring();
        let data = EventHandle::create_unnamed(false).unwrap();
        let before = data_waits();
        assert_eq!(
            ring.wait_for_data(
                &data,
                None,
                Some(Duration::from_millis(30)),
                SpinPolicy::new(1_000)
            ),
            Ok(false)
        );
        assert_eq!(data_waits() - before, 1);

        let t0 = Instant::now();
        assert_eq!(
            ring.wait_for_data(&data, None, Some(Duration::ZERO), LONG_SPIN),
            Ok(false)
        );
        assert!(
            t0.elapsed() < Duration::from_millis(200),
            "нулевой таймаут спинил: {:?}",
            t0.elapsed()
        );
    }

    /// Спин уважает дедлайн вызывающего: бюджет `u32::MAX` (минуты) не
    /// растягивает короткий таймаут ни ожидания данных, ни ожидания места.
    #[test]
    fn spin_respects_caller_deadline() {
        let unbounded = SpinPolicy::new(u32::MAX);
        let (ring, _mem) = make_ring();
        let data = EventHandle::create_unnamed(false).unwrap();
        let t0 = Instant::now();
        assert_eq!(
            ring.wait_for_data(&data, None, Some(Duration::from_millis(30)), unbounded),
            Ok(false)
        );
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "спин данных перебрал таймаут: {:?}",
            t0.elapsed()
        );

        while ring.try_write_message(&[0u8; LEN]).is_ok() {}
        let space = EventHandle::create_unnamed(false).unwrap();
        let t0 = Instant::now();
        assert_eq!(
            ring.wait_for_space(
                LEN,
                Some(&space),
                None,
                None,
                Some(Duration::from_millis(30)),
                unbounded
            ),
            Ok(false)
        );
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "спин места перебрал таймаут: {:?}",
            t0.elapsed()
        );

        // Дедлайн проверяется раз в `SPIN_DEADLINE_STRIDE` итераций, а не
        // на каждой: уже наступивший дедлайн обрывает спин ровно на первой
        // проверке, после `SPIN_DEADLINE_STRIDE` вызовов `ready`.
        let mut calls = 0;
        assert!(!spin_until(
            SpinPolicy::new(SPIN_DEADLINE_STRIDE * 3),
            Some(Instant::now()),
            || {
                calls += 1;
                false
            }
        ));
        assert_eq!(calls, SPIN_DEADLINE_STRIDE, "дедлайн сверяется на шаге");
    }

    /// Место, освобождённое читателем во время спина (без заявки и без
    /// `SPACE`), видно без ожидания в ядре; заявка на время спина не
    /// ставится.
    #[test]
    fn spin_sees_space_without_kernel_wait() {
        let (ring, _mem) = make_ring();
        let ring = Arc::new(ring);
        while ring.try_write_message(&[0u8; LEN]).is_ok() {}
        let space = EventHandle::create_unnamed(false).unwrap();
        let reader = {
            let ring = Arc::clone(&ring);
            thread::spawn(move || {
                thread::sleep(PEER_DELAY);
                let mut out = Vec::new();
                ring.read_message(&mut out).unwrap();
                // Заявки нет -- будить некого.
                assert!(!ring.take_space_waiter());
            })
        };
        let before = space_waits();
        assert_eq!(
            ring.wait_for_space(
                LEN,
                Some(&space),
                None,
                None,
                Some(Duration::from_secs(5)),
                LONG_SPIN
            ),
            Ok(true)
        );
        reader.join().unwrap();
        assert_eq!(space_waits() - before, 0, "спин ушёл в ядро");
        assert_eq!(ring.header().space_waiter.load(O::Acquire), 0);
        ring.try_write_message(&[1u8; LEN]).unwrap();
    }
}
