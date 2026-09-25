//! Lock-free SPSC кольцевой буфер для shared memory IPC.
//!
//! ВАЖНО: Код оптимизирован для x86/x86_64 с TSO (Total Store Order).
//! На этих архитектурах stores видны в порядке программы, что упрощает
//! синхронизацию. НЕ портировать на ARM/RISC-V без доработки!

use std::ptr::NonNull;
use std::sync::atomic::{Ordering, compiler_fence, fence};
use std::time::{Duration, Instant};

use crate::constants::*;
use crate::error::{Result, ShmError};
use crate::layout::RingHeader;
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
}

/// Штатное отключение пира для `wait_for_space`: событие `DISCONNECT` (одно
/// на обе стороны, автосброс) и проверка по разделяемой памяти, что пир
/// действительно ушёл (его состояние handshake уже не `SERVER_READY`) --
/// отличает свежий сигнал от устаревшего.
pub(crate) struct DisconnectWatch<'a> {
    pub event: &'a EventHandle,
    pub peer_left: &'a dyn Fn() -> bool,
}

#[derive(Debug, Clone, Copy)]
pub struct WriteOutcome {
    pub overwritten: u32,
    pub was_empty: bool,
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
    /// Свободные байты кольца. Каждое сообщение занимает
    /// `MESSAGE_HEADER_SIZE` (4) + длина payload.
    pub bytes: usize,
    /// Сколько ещё сообщений можно положить до лимита `MAX_MESSAGES`.
    pub messages: u32,
}

impl FreeSpace {
    /// Нулевое место: канал не подключён или кольцо заполнено.
    pub const ZERO: Self = Self {
        bytes: 0,
        messages: 0,
    };

    /// Поместится ли сейчас payload длины `payload_len` без перезаписи.
    /// Для недопустимых длин (`< MIN_MESSAGE_SIZE` или `> MAX_MESSAGE_SIZE`)
    /// -- всегда `false`: такие сообщения не принимаются никогда.
    #[must_use]
    pub const fn fits(&self, payload_len: usize) -> bool {
        payload_len >= MIN_MESSAGE_SIZE
            && payload_len <= MAX_MESSAGE_SIZE
            && self.messages > 0
            && self.bytes >= MESSAGE_HEADER_SIZE + payload_len
    }

    /// Максимальная длина payload, которая поместится прямо сейчас
    /// (`0` -- не поместится ни одно сообщение).
    #[must_use]
    pub const fn max_payload(&self) -> usize {
        if self.messages == 0 || self.bytes < MESSAGE_HEADER_SIZE + MIN_MESSAGE_SIZE {
            return 0;
        }
        let room = self.bytes - MESSAGE_HEADER_SIZE;
        if room > MAX_MESSAGE_SIZE {
            MAX_MESSAGE_SIZE
        } else {
            room
        }
    }
}

/// Размер кадра (заголовок + payload) для валидной длины payload.
///
/// Единая проверка границ для `write_message`/`try_write_message`/
/// `wait_for_space`: `MIN_MESSAGE_SIZE..=MAX_MESSAGE_SIZE`. Кадр максимальной
/// длины (65 539 байт) заведомо меньше `RING_CAPACITY` (2 МиБ), поэтому
/// «сообщение больше кольца» отдельной ветки не требует.
pub(crate) const fn frame_len(payload_len: usize) -> Result<u32> {
    if payload_len < MIN_MESSAGE_SIZE {
        return Err(ShmError::MessageTooSmall);
    }
    if payload_len > MAX_MESSAGE_SIZE {
        return Err(ShmError::MessageTooLarge);
    }
    Ok((MESSAGE_HEADER_SIZE + payload_len) as u32)
}

// Кадр максимальной длины обязан помещаться в пустое кольцо -- иначе
// `try_write_message` мог бы вечно отвечать `QueueFull` на валидное сообщение.
const _: () = assert!(MESSAGE_HEADER_SIZE + MAX_MESSAGE_SIZE <= RING_CAPACITY);

#[derive(Debug)]
pub struct RingBuffer {
    header: NonNull<RingHeader>,
    storage: NonNull<u8>,
    capacity: u32,
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
    pub const unsafe fn new(header: *mut RingHeader, data: *mut u8) -> Self {
        RingBuffer {
            header: NonNull::new(header).expect("header pointer must be valid"),
            storage: NonNull::new(data).expect("ring buffer pointer must be valid"),
            capacity: RING_CAPACITY as u32,
        }
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

    const fn available_bytes(&self, write: u32, read: u32) -> i64 {
        let used = write.wrapping_sub(read);
        self.capacity as i64 - used as i64
    }

    const fn mask_index(&self, pos: u32) -> usize {
        (pos & RING_MASK) as usize
    }

    /// # Safety
    /// `index + data.len() <= capacity` (вызывающий код обязан гарантировать
    /// отсутствие выхода за пределы `storage`; сам `copy_into` этого не
    /// проверяет -- проверки границ выполняются в `copy_into_wrapped` через
    /// модульную арифметику до вызова).
    const unsafe fn copy_into(&self, index: usize, data: &[u8]) {
        // SAFETY: storage валиден на всё время жизни self (гарантия
        // конструктора RingBuffer::new); index+data.len() <= capacity --
        // инвариант вызывающей стороны (см. doc выше).
        unsafe {
            let ptr = self.data_ptr().add(index);
            ptr.copy_from_nonoverlapping(data.as_ptr(), data.len());
        }
    }

    /// # Safety
    /// `index + dst.len() <= capacity` (см. `copy_into`).
    const unsafe fn copy_from(&self, index: usize, dst: &mut [u8]) {
        // SAFETY: storage валиден на всё время жизни self; index+dst.len()
        // <= capacity -- инвариант вызывающей стороны (см. doc выше).
        unsafe {
            let ptr = self.data_ptr().add(index);
            dst.copy_from_slice(std::slice::from_raw_parts(ptr, dst.len()));
        }
    }

    /// # Safety
    /// `index < capacity` (читает 2 байта начиная с `index`, с wrap-around
    /// через `copy_from_wrapped`, поэтому сам `index` не обязан оставлять
    /// место под оба байта без переноса).
    unsafe fn read_u16(&self, index: usize) -> u16 {
        let mut buf = [0u8; 2];
        // SAFETY: copy_from_wrapped сам обеспечивает wrap-around в пределах
        // capacity -- единственное требование к index описано в doc выше.
        unsafe { self.copy_from_wrapped(index, &mut buf) };
        u16::from_le_bytes(buf)
    }

    /// # Safety
    /// `data.len() <= capacity` (иначе один и тот же байт будет записан
    /// дважды при переносе через границу кольца; вызывающий код -- ring.rs
    /// сам, всегда после проверки `total_required <= self.capacity` в
    /// `write_message`).
    unsafe fn copy_into_wrapped(&self, start: usize, data: &[u8]) {
        let capacity = self.capacity as usize;
        let start = start % capacity;
        let first = capacity - start;
        if data.len() <= first {
            // SAFETY: start+data.len() <= capacity -- проверено веткой if.
            unsafe { self.copy_into(start, data) };
        } else {
            // SAFETY: первая часть укладывается в [start, capacity) по построению
            // (start+first == capacity).
            unsafe { self.copy_into(start, &data[..first]) };
            // SAFETY: остаток укладывается в [0, capacity): его длина равна
            // data.len()-first, а data.len() <= capacity (контракт функции).
            unsafe { self.copy_into(0, &data[first..]) };
        }
    }

    /// # Safety
    /// `dst.len() <= capacity` (см. `copy_into_wrapped`).
    unsafe fn copy_from_wrapped(&self, start: usize, dst: &mut [u8]) {
        let capacity = self.capacity as usize;
        let start = start % capacity;
        let first = capacity - start;
        if dst.len() <= first {
            // SAFETY: start+dst.len() <= capacity -- проверено веткой if.
            unsafe { self.copy_from(start, dst) };
        } else {
            // SAFETY: первая часть укладывается в [start, capacity) по построению
            // (start+first == capacity), как и в copy_into_wrapped.
            unsafe { self.copy_from(start, &mut dst[..first]) };
            // SAFETY: остаток укладывается в [0, capacity): его длина равна
            // dst.len()-first, а dst.len() <= capacity (контракт функции).
            unsafe { self.copy_from(0, &mut dst[first..]) };
        }
    }

    fn discard_oldest(&self) -> Result<()> {
        let header = self.header();

        loop {
            let read = header.read_pos.load(Ordering::Acquire);
            let write = header.write_pos.load(Ordering::Acquire);
            if read == write {
                return Err(ShmError::QueueEmpty);
            }

            let idx = self.mask_index(read);
            // SAFETY: idx = read & RING_MASK всегда < capacity (mask_index).
            let msg_len = unsafe { self.read_u16(idx) } as usize;
            if !(MIN_MESSAGE_SIZE..=MAX_MESSAGE_SIZE).contains(&msg_len) {
                // Повреждённая длина в слоте. Не трогаем общий message_count
                // деструктивно (его двигает и reader). Сигналим Corrupted —
                // вызывающий код решает (auto-mode трактует как fatal -> reconnect,
                // что сбросит буферы через handshake/generation).
                return Err(ShmError::Corrupted);
            }
            let total = MESSAGE_HEADER_SIZE + msg_len;
            let new_read = read.wrapping_add(total as u32);

            // CAS to avoid racing with read_message on the reader side
            if header
                .read_pos
                .compare_exchange(read, new_read, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                header.message_count.fetch_sub(1, Ordering::AcqRel);
                header.drop_count.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
            // CAS failed — reader moved read_pos, retry with fresh values
        }
    }

    /// Запись с политикой «перезаписать старейшее»: при нехватке места
    /// (байт или слотов `MAX_MESSAGES`) вытесняет самые старые непрочитанные
    /// сообщения, число вытесненных -- в `WriteOutcome::overwritten`.
    pub fn write_message(&self, payload: &[u8]) -> Result<WriteOutcome> {
        let total_required = frame_len(payload.len())?;

        let header = self.header();
        let mut overwritten = 0u32;

        loop {
            let write = header.write_pos.load(Ordering::Acquire);
            let read = header.read_pos.load(Ordering::Acquire);
            let available = self.available_bytes(write, read);
            let count = header.message_count.load(Ordering::Acquire);

            if available < total_required as i64 || count >= MAX_MESSAGES {
                if count == 0 {
                    // нет сообщений, но не хватает места — значит сообщение больше буфера
                    return Err(ShmError::MessageTooLarge);
                }
                self.discard_oldest()?;
                overwritten += 1;
                continue;
            }

            let was_empty = self.commit(write, payload, total_required);
            return Ok(WriteOutcome {
                overwritten,
                was_empty,
            });
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
    /// - `QueueFull` -- не хватает байт (`4 + len`) или занято `MAX_MESSAGES`
    ///   слотов; проверка консервативна (см. `free_space`), ложных успехов
    ///   нет, ложный `QueueFull` возможен только если читатель освободил
    ///   место в тот же момент -- повтор увидит его;
    /// - длина вне `MIN_MESSAGE_SIZE..=MAX_MESSAGE_SIZE` --
    ///   `MessageTooSmall`/`MessageTooLarge` (кадр максимальной длины всегда
    ///   помещается в пустое кольцо, «больше кольца» не бывает);
    /// - писатель на направление ровно один (SPSC), как и у `write_message`.
    ///
    /// Ordering: `read_pos` читается с `Acquire`. Читатель двигает его CAS-ом
    /// (`AcqRel`) строго ПОСЛЕ того, как докопировал сообщение, поэтому всё,
    /// что писатель затем пишет в освобождённый диапазон, happens-after
    /// чтения этого диапазона читателем -- гонки данных «писатель затирает
    /// то, что читатель ещё копирует» нет (в отличие от overwrite-пути, где
    /// её разруливает seqlock-валидация в `read_message`).
    pub fn try_write_message(&self, payload: &[u8]) -> Result<WriteOutcome> {
        let total_required = frame_len(payload.len())?;
        let header = self.header();

        // write_pos меняет только этот (единственный) писатель; read_pos и
        // message_count читатель может лишь уменьшать «занятость», поэтому
        // значения ниже -- консервативная оценка свободного места.
        let write = header.write_pos.load(Ordering::Acquire);
        let read = header.read_pos.load(Ordering::Acquire);
        let count = header.message_count.load(Ordering::Acquire);

        if self.available_bytes(write, read) < total_required as i64 || count >= MAX_MESSAGES {
            return Err(ShmError::QueueFull);
        }

        let was_empty = self.commit(write, payload, total_required);
        Ok(WriteOutcome {
            overwritten: 0,
            was_empty,
        })
    }

    /// Копирует кадр в позицию `write` и публикует его.
    /// Возвращает `true`, если кольцо было пусто (переход empty -> non-empty).
    ///
    /// Вызывающий обязан предварительно убедиться, что `total_required`
    /// байт начиная с `write` свободны и `message_count < MAX_MESSAGES`.
    fn commit(&self, write: u32, payload: &[u8], total_required: u32) -> bool {
        let header = self.header();
        let idx = self.mask_index(write);
        let len_le = (payload.len() as u16).to_le_bytes();
        let flags = 0u16.to_le_bytes();
        // SAFETY: каждый вызов copy_into_wrapped пишет <= capacity байт
        // (len_le/flags -- по 2 байта, payload -- не более MAX_MESSAGE_SIZE,
        // `frame_len` + const-assert гарантируют total_required <= capacity).
        unsafe {
            self.copy_into_wrapped(idx, &len_le);
            self.copy_into_wrapped((idx + 2) & (RING_MASK as usize), &flags);
            self.copy_into_wrapped((idx + MESSAGE_HEADER_SIZE) & (RING_MASK as usize), payload);
        }

        // ВАЖНО: сначала увеличиваем message_count, потом обновляем write_pos
        // Это гарантирует, что reader увидит count > 0 когда видит новый write_pos
        // На x86/x64 TSO это безопасно, но порядок операций всё равно важен
        let prev_count = header.message_count.fetch_add(1, Ordering::AcqRel);

        let new_write = write.wrapping_add(total_required);
        header.write_pos.store(new_write, Ordering::Release);

        if prev_count == 0 {
            header.sequence.fetch_add(1, Ordering::Relaxed);
        }
        prev_count == 0
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
        let bytes = self.available_bytes(write, read);
        FreeSpace {
            // Отрицательное значение возможно только на повреждённом
            // заголовке (read > write) -- трактуем как «места нет».
            bytes: if bytes > 0 { bytes as usize } else { 0 },
            messages: MAX_MESSAGES.saturating_sub(count),
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

    pub fn read_message(&self, out: &mut Vec<u8>) -> Result<usize> {
        let header = self.header();

        loop {
            let count = header.message_count.load(Ordering::Acquire);
            if count == 0 {
                return Err(ShmError::QueueEmpty);
            }

            let read = header.read_pos.load(Ordering::Acquire);
            let idx = self.mask_index(read);
            // SAFETY: idx = read & RING_MASK всегда < capacity (mask_index).
            let msg_len = unsafe { self.read_u16(idx) } as usize;
            if !(MIN_MESSAGE_SIZE..=MAX_MESSAGE_SIZE).contains(&msg_len) {
                // Длина могла быть «порвана» перезаписью producer-а. Если read_pos
                // уже сдвинулся — это гонка перезаписи, повторяем. Иначе буфер
                // действительно повреждён.
                if header.read_pos.load(Ordering::Acquire) != read {
                    continue;
                }
                return Err(ShmError::Corrupted);
            }

            let total = MESSAGE_HEADER_SIZE + msg_len;
            let new_read = read.wrapping_add(total as u32);

            // ОПТИМИСТИЧНОЕ копирование ДО фиксации read_pos (seqlock-паттерн).
            // Если producer перезапишет слот во время копирования, CAS ниже
            // провалится, и мы отбросим эту (потенциально битую) копию.
            //
            // Заполняем именно неинициализированный хвост (`spare_capacity_mut`),
            // а `set_len` двигаем ПОСЛЕ записи: обратный порядок на мгновение
            // создавал `&mut [u8]` на неинициализированную память -- UB по букве
            // правил, даже если на x86 это работало (аудит 2026-07-28).
            out.clear();
            out.reserve(msg_len);
            let spare = &mut out.spare_capacity_mut()[..msg_len];
            // SAFETY: `spare` -- ровно msg_len байт выделенной (пусть и
            // неинициализированной) памяти вектора; copy_from_wrapped пишет
            // строго в пределах кольца (wrap по модулю capacity) и заполняет
            // весь срез целиком.
            unsafe {
                self.copy_from_wrapped(
                    (idx + MESSAGE_HEADER_SIZE) & (RING_MASK as usize),
                    spare.assume_init_mut(),
                );
            }
            // SAFETY: первые msg_len байт только что инициализированы выше.
            unsafe { out.set_len(msg_len) };

            // Барьер компилятора: копирование не должно «переехать» НИЖЕ CAS,
            // иначе валидация теряет смысл. На x86 успешный lock cmpxchg также
            // даёт аппаратный барьер.
            compiler_fence(Ordering::Release);

            // Фиксация: атомарно забираем слот. Провал => producer сдвинул read_pos
            // (перезапись/конкурентный discard) => скопированные байты невалидны,
            // повторяем с актуальными значениями.
            if header
                .read_pos
                .compare_exchange(read, new_read, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                continue;
            }

            let prev_count = header.message_count.fetch_sub(1, Ordering::AcqRel);
            if prev_count <= 1 {
                header.sequence.fetch_add(1, Ordering::Relaxed);
            }

            return Ok(msg_len);
        }
    }

    /// Писатель: дождаться, пока в кольце появится место под payload длины
    /// `payload_len`. `Ok(true)` -- место есть (следующий `try_write_message`
    /// этого писателя пройдёт), `Ok(false)` -- истёк `timeout`,
    /// `Err(PeerDied)` -- процесс читателя умер (место не освободится
    /// никогда), `Err(NotConnected)` -- читатель штатно отключился
    /// (`DISCONNECT`). `timeout = None` -- ждать без ограничения: безопасно
    /// только при наблюдаемом пире (`peer`), иначе упавший читатель старой
    /// версии подвесит навсегда -- единственный таймаут тогда тот, что задал
    /// вызывающий.
    ///
    /// Ожидание -- ОДНО `NtWaitForMultipleObjects` на весь остаток таймаута
    /// (0.9: без нарезки на срезы по 50 мс) по набору `[SPACE, DISCONNECT,
    /// процесс читателя]`. `SPACE` читатель сигналит, сняв заявку
    /// (`take_space_waiter`) или опустошив кольцо; читатель старой версии,
    /// не знающий про заявку, будит писателя только опустошив кольцо -- это
    /// задержка, но не зависание, пока он читает. Без событий вовсе
    /// (anonymous-сервер) -- опрос с шагом `SPACE_POLL_INTERVAL`.
    pub(crate) fn wait_for_space(
        &self,
        payload_len: usize,
        space_event: Option<&EventHandle>,
        peer: Option<&ProcessWatch>,
        disconnect: Option<&DisconnectWatch<'_>>,
        timeout: Option<Duration>,
    ) -> Result<bool> {
        let frame = frame_len(payload_len)?;
        let deadline = timeout.map(|t| Instant::now() + t);
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
            let mut handles = [space_event.raw_handle(), 0, 0];
            let mut count = 1;
            let disconnect_index = disconnect.map(|d| {
                handles[count] = d.event.raw_handle();
                count += 1;
                count - 1
            });
            let peer_index = peer.map(|p| {
                handles[count] = p.raw_handle();
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

    pub fn message_count(&self) -> u32 {
        self.header().message_count.load(Ordering::Acquire)
    }

    pub fn is_empty(&self) -> bool {
        self.message_count() == 0
    }
}

#[cfg(test)]
mod overflow_race_tests {
    use super::*;
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
        let header_size = size_of::<RingHeader>();
        let total = header_size + RING_CAPACITY;
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
        let ring = unsafe { RingBuffer::new(header, data) };
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
    use super::overflow_race_tests::make_ring;
    use super::*;
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
        let fs = |bytes, messages| FreeSpace { bytes, messages };
        assert_eq!(FreeSpace::ZERO.max_payload(), 0);
        assert!(!FreeSpace::ZERO.fits(MIN_MESSAGE_SIZE));
        // Хватает ровно на минимальный кадр.
        assert!(fs(6, 1).fits(2));
        assert_eq!(fs(6, 1).max_payload(), 2);
        assert!(!fs(5, 1).fits(2));
        assert_eq!(fs(5, 1).max_payload(), 0);
        // Байт много, слотов нет.
        assert!(!fs(RING_CAPACITY, 0).fits(10));
        assert_eq!(fs(RING_CAPACITY, 0).max_payload(), 0);
        // Недопустимые длины не «влезают» никогда.
        assert!(!fs(RING_CAPACITY, 10).fits(1));
        assert!(!fs(RING_CAPACITY, 10).fits(MAX_MESSAGE_SIZE + 1));
        assert_eq!(fs(1000, 1).max_payload(), 996);
    }

    #[test]
    fn try_write_rejects_bad_lengths_without_side_effects() {
        let (ring, _mem) = make_ring();
        assert_eq!(
            ring.try_write_message(&[1]).err(),
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
        assert_eq!(free.bytes, RING_CAPACITY - (4 + 100) - (4 + 10));
        assert_eq!(free.messages, MAX_MESSAGES - 2);
        ring.read_message(&mut out).unwrap();
        assert_eq!(ring.free_space().bytes, RING_CAPACITY - (4 + 10));
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
        assert_eq!(written as usize, RING_CAPACITY / (4 + LEN));
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
        assert_eq!(ring.message_count(), written);

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
                    used + 4 + len <= RING_CAPACITY && (model.len() as u32) < MAX_MESSAGES;
                assert_eq!(ring.free_space().fits(len), fits_model);
                match ring.try_write_message(&payload) {
                    Ok(o) => {
                        assert!(fits_model, "try_write прошёл, хотя места нет по модели");
                        assert_eq!(o.overwritten, 0);
                        assert_eq!(o.was_empty, model.is_empty());
                        used += 4 + len;
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
                        used -= 4 + n;
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
        let need = 2 * (4 + LEN) as u32;
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
        ring.arm_space_waiter(4 + LEN as u32);
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
            ring.wait_for_space(LEN, None, None, None, Some(Duration::from_millis(20))),
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
            ring.wait_for_space(LEN, None, None, None, Some(Duration::from_secs(5))),
            Ok(true)
        );
        reader.join().unwrap();
        ring.try_write_message(&[1u8; LEN]).unwrap();
        assert_eq!(
            ring.wait_for_space(1, None, None, None, None),
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
            ring.wait_for_space(LEN, Some(&space), None, None, None),
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
            ring.wait_for_space(LEN, Some(&space), Some(&watch), None, None),
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
            ring.wait_for_space(LEN, Some(&space), None, Some(&watch), None),
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
            ring.wait_for_space(LEN, Some(&space), None, Some(&watch), None),
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
