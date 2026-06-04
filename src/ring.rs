//! Lock-free SPSC кольцевой буфер для shared memory IPC.
//!
//! ВАЖНО: Код оптимизирован для x86/x86_64 с TSO (Total Store Order).
//! На этих архитектурах stores видны в порядке программы, что упрощает
//! синхронизацию. НЕ портировать на ARM/RISC-V без доработки!

use std::ptr::NonNull;
use std::sync::atomic::Ordering;

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

    unsafe fn copy_into(&self, index: usize, data: &[u8]) {
        unsafe {
            let ptr = self.data_ptr().add(index);
            ptr.copy_from_nonoverlapping(data.as_ptr(), data.len());
        }
    }

    unsafe fn copy_from(&self, index: usize, dst: &mut [u8]) {
        unsafe {
            let ptr = self.data_ptr().add(index);
            dst.copy_from_slice(std::slice::from_raw_parts(ptr, dst.len()));
        }
    }

    unsafe fn read_u16(&self, index: usize) -> u16 {
        let mut buf = [0u8; 2];
        unsafe { self.copy_from_wrapped(index, &mut buf) };
        u16::from_le_bytes(buf)
    }

    unsafe fn copy_into_wrapped(&self, start: usize, data: &[u8]) {
        let capacity = self.capacity as usize;
        let start = start % capacity;
        let first = capacity - start;
        if data.len() <= first {
            unsafe { self.copy_into(start, data) };
        } else {
            unsafe { self.copy_into(start, &data[..first]) };
            unsafe { self.copy_into(0, &data[first..]) };
        }
    }

    unsafe fn copy_from_wrapped(&self, start: usize, dst: &mut [u8]) {
        let capacity = self.capacity as usize;
        let start = start % capacity;
        let first = capacity - start;
        if dst.len() <= first {
            unsafe { self.copy_from(start, dst) };
        } else {
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
            let msg_len = unsafe { self.read_u16(idx) } as usize;
            if !(MIN_MESSAGE_SIZE..=MAX_MESSAGE_SIZE).contains(&msg_len) {
                // Corrupted ring buffer -- reset read_pos to write_pos to recover
                header.read_pos.store(write, Ordering::Release);
                header.message_count.store(0, Ordering::Release);
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
            let msg_len = unsafe { self.read_u16(idx) } as usize;
            if !(MIN_MESSAGE_SIZE..=MAX_MESSAGE_SIZE).contains(&msg_len) {
                return Err(ShmError::Corrupted);
            }

            let total = MESSAGE_HEADER_SIZE + msg_len;
            let new_read = read.wrapping_add(total as u32);

            // CAS to avoid racing with discard_oldest on the writer side
            if header
                .read_pos
                .compare_exchange(read, new_read, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                // discard_oldest moved read_pos, retry
                continue;
            }

            // We own this message slot now — copy data out
            // SAFETY: clear + reserve guarantees capacity >= msg_len
            out.clear();
            out.reserve(msg_len);
            debug_assert!(
                out.capacity() >= msg_len,
                "reserve failed: cap {} < msg_len {}",
                out.capacity(),
                msg_len
            );
            unsafe {
                out.set_len(msg_len);
                self.copy_from_wrapped(
                    (idx + MESSAGE_HEADER_SIZE) & (RING_MASK as usize),
                    out.as_mut_slice(),
                );
            }

            // read_pos already updated via CAS above
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

    const PAYLOAD: usize = 256;

    fn fill(seq: u32) -> Vec<u8> {
        let mut buf = vec![0u8; PAYLOAD];
        buf[0..4].copy_from_slice(&seq.to_le_bytes());
        for (k, b) in buf[4..].iter_mut().enumerate() {
            *b = (seq as usize).wrapping_add(k) as u8;
        }
        buf
    }

    /// Err, если сообщение «порвано» (смешаны байты двух разных записей).
    // Примечание: `use super::*` втягивает crate-овый `Result<T>` (ошибка = ShmError),
    // поэтому здесь используем полностью квалифицированный std-Result для String-ошибки.
    fn check(msg: &[u8]) -> std::result::Result<(), String> {
        if msg.len() != PAYLOAD {
            return Err(format!("bad len {}", msg.len()));
        }
        let seq = u32::from_le_bytes([msg[0], msg[1], msg[2], msg[3]]);
        for (k, &b) in msg[4..].iter().enumerate() {
            let exp = (seq as usize).wrapping_add(k) as u8;
            if b != exp {
                return Err(format!("torn seq={seq} at {k}: {b}!={exp}"));
            }
        }
        Ok(())
    }

    #[test]
    fn overflow_does_not_tear_messages() {
        let (ring, _mem) = make_ring();
        let ring = Arc::new(ring);
        let stop = Arc::new(AtomicBool::new(false));
        let torn = Arc::new(AtomicU64::new(0));

        let producer = {
            let ring = ring.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                let mut seq: u32 = 0;
                while !stop.load(O::Acquire) {
                    let msg = fill(seq);
                    let _ = ring.write_message(&msg); // overwrite разрешён
                    seq = seq.wrapping_add(1);
                }
            })
        };

        let consumer = {
            let ring = ring.clone();
            let stop = stop.clone();
            let torn = torn.clone();
            thread::spawn(move || {
                let mut out = Vec::with_capacity(PAYLOAD);
                let mut reads: u64 = 0;
                while !stop.load(O::Acquire) {
                    match ring.read_message(&mut out) {
                        Ok(_) => {
                            if check(&out).is_err() {
                                torn.fetch_add(1, O::AcqRel);
                            }
                            reads += 1;
                            if reads % 64 == 0 {
                                // периодически отстаём -> провоцируем overflow
                                thread::yield_now();
                            }
                        }
                        Err(_) => thread::yield_now(),
                    }
                }
                reads
            })
        };

        thread::sleep(std::time::Duration::from_millis(500));
        stop.store(true, O::Release);
        producer.join().unwrap();
        let _reads = consumer.join().unwrap();

        let torn = torn.load(O::Acquire);
        assert_eq!(torn, 0, "обнаружены порванные сообщения: {torn}");
    }
}
