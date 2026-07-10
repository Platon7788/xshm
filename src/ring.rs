//! Lock-free SPSC кольцевой буфер для shared memory IPC.
//!
//! ВАЖНО: Код оптимизирован для x86/x86_64 с TSO (Total Store Order).
//! На этих архитектурах stores видны в порядке программы, что упрощает
//! синхронизацию. НЕ портировать на ARM/RISC-V без доработки!

use std::ptr::NonNull;
use std::sync::atomic::{compiler_fence, Ordering};

use crate::constants::*;
use crate::error::{Result, ShmError};
use crate::layout::RingHeader;

#[derive(Debug, Clone, Copy)]
pub struct WriteOutcome {
    pub overwritten: u32,
    pub was_empty: bool,
}

pub struct RingBuffer {
    header: NonNull<RingHeader>,
    storage: NonNull<u8>,
    capacity: u32,
}

unsafe impl Send for RingBuffer {}
unsafe impl Sync for RingBuffer {}

impl RingBuffer {
    pub unsafe fn new(header: *mut RingHeader, data: *mut u8) -> Self {
        RingBuffer {
            header: NonNull::new(header).expect("header pointer must be valid"),
            storage: NonNull::new(data).expect("ring buffer pointer must be valid"),
            capacity: RING_CAPACITY as u32,
        }
    }

    fn header(&self) -> &RingHeader {
        unsafe { self.header.as_ref() }
    }

    fn data_ptr(&self) -> *mut u8 {
        self.storage.as_ptr()
    }

    #[allow(dead_code)]
    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    #[allow(dead_code)]
    pub fn reset(&self, generation: u32) {
        self.header().reset(generation);
    }

    fn available_bytes(&self, write: u32, read: u32) -> i64 {
        let used = write.wrapping_sub(read);
        self.capacity as i64 - used as i64
    }

    fn mask_index(&self, pos: u32) -> usize {
        (pos & RING_MASK) as usize
    }

    /// # Safety
    /// `index + data.len() <= capacity` (вызывающий код обязан гарантировать
    /// отсутствие выхода за пределы `storage`; сам `copy_into` этого не
    /// проверяет -- проверки границ выполняются в `copy_into_wrapped` через
    /// модульную арифметику до вызова).
    unsafe fn copy_into(&self, index: usize, data: &[u8]) {
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
    unsafe fn copy_from(&self, index: usize, dst: &mut [u8]) {
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
            // SAFETY: обе части (`first` и остаток) укладываются в
            // [0, capacity) по построению (start+first == capacity).
            unsafe { self.copy_into(start, &data[..first]) };
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
            // SAFETY: обе части укладываются в [0, capacity) по построению
            // (start+first == capacity), как и в copy_into_wrapped.
            unsafe { self.copy_from(start, &mut dst[..first]) };
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

    pub fn write_message(&self, payload: &[u8]) -> Result<WriteOutcome> {
        if payload.len() < MIN_MESSAGE_SIZE {
            return Err(ShmError::MessageTooSmall);
        }
        if payload.len() > MAX_MESSAGE_SIZE {
            return Err(ShmError::MessageTooLarge);
        }

        let total_required = (MESSAGE_HEADER_SIZE + payload.len()) as u32;
        if total_required > self.capacity {
            return Err(ShmError::MessageTooLarge);
        }

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

            let idx = self.mask_index(write);
            let len_le = (payload.len() as u16).to_le_bytes();
            let flags = 0u16.to_le_bytes();
            // SAFETY: каждый вызов copy_into_wrapped пишет <= capacity байт
            // (len_le/flags -- по 2 байта, payload -- не более MAX_MESSAGE_SIZE,
            // и total_required = MESSAGE_HEADER_SIZE+payload.len() уже
            // проверен против self.capacity веткой availability-проверки выше).
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

            return Ok(WriteOutcome {
                overwritten,
                was_empty: prev_count == 0,
            });
        }
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
            // SAFETY: clear + reserve гарантируют capacity >= msg_len; copy_from_wrapped
            // читает строго в пределах кольца (wrap по модулю capacity).
            out.clear();
            out.reserve(msg_len);
            debug_assert!(out.capacity() >= msg_len);
            unsafe {
                out.set_len(msg_len);
                self.copy_from_wrapped(
                    (idx + MESSAGE_HEADER_SIZE) & (RING_MASK as usize),
                    out.as_mut_slice(),
                );
            }

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

    pub fn message_count(&self) -> u32 {
        self.header().message_count.load(Ordering::Acquire)
    }

    #[allow(dead_code)]
    pub fn drop_count(&self) -> u32 {
        self.header().drop_count.load(Ordering::Acquire)
    }

    pub fn is_empty(&self) -> bool {
        self.message_count() == 0
    }
}

#[cfg(test)]
mod overflow_race_tests {
    use super::*;
    use crate::layout::RingHeader;
    use std::alloc::{alloc_zeroed, dealloc, Layout};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as O};
    use std::sync::Arc;
    use std::thread;

    /// Владелец сырой выровненной памяти под один RingHeader + RING_CAPACITY.
    struct RingMem {
        ptr: *mut u8,
        layout: Layout,
    }
    unsafe impl Send for RingMem {}
    unsafe impl Sync for RingMem {}
    impl Drop for RingMem {
        fn drop(&mut self) {
            // SAFETY: ptr/layout получены из alloc_zeroed в make_ring.
            unsafe { dealloc(self.ptr, self.layout) };
        }
    }

    fn make_ring() -> (RingBuffer, RingMem) {
        let header_size = std::mem::size_of::<RingHeader>();
        let total = header_size + RING_CAPACITY;
        let layout = Layout::from_size_align(total, 64).unwrap();
        // SAFETY: ненулевой размер; зануление валидно для AtomicU32 полей.
        let ptr = unsafe { alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "alloc failed");
        let header = ptr as *mut RingHeader;
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
            let ring = ring.clone();
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

        thread::sleep(std::time::Duration::from_millis(800));
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
