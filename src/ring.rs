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
        let read = header.read_pos.load(Ordering::Acquire);
        let write = header.write_pos.load(Ordering::Acquire);
        if read == write {
            return Err(ShmError::QueueEmpty);
        }

        let idx = self.mask_index(read);
        let msg_len = unsafe { self.read_u16(idx) } as usize;
        let total = MESSAGE_HEADER_SIZE + msg_len;
        let new_read = read.wrapping_add(total as u32);

        header.read_pos.store(new_read, Ordering::Release);
        header.message_count.fetch_sub(1, Ordering::Release);
        header.drop_count.fetch_add(1, Ordering::Relaxed);
        Ok(())
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
        let count = header.message_count.load(Ordering::Acquire);
        if count == 0 {
            return Err(ShmError::QueueEmpty);
        }

        let read = header.read_pos.load(Ordering::Acquire);
        let idx = self.mask_index(read);
        let msg_len = unsafe { self.read_u16(idx) } as usize;
        if msg_len < MIN_MESSAGE_SIZE || msg_len > MAX_MESSAGE_SIZE {
            return Err(ShmError::Corrupted);
        }

        if out.capacity() < msg_len {
            out.reserve(msg_len - out.capacity());
        }
        unsafe {
            out.set_len(msg_len);
            self.copy_from_wrapped(
                (idx + MESSAGE_HEADER_SIZE) & (RING_MASK as usize),
                out.as_mut_slice(),
            );
        }

        let total = MESSAGE_HEADER_SIZE + msg_len;
        let new_read = read.wrapping_add(total as u32);
        
        // ВАЖНО: сначала обновляем read_pos, потом уменьшаем message_count
        // Симметрично write_message для корректной синхронизации
        header.read_pos.store(new_read, Ordering::Release);
        let prev_count = header.message_count.fetch_sub(1, Ordering::AcqRel);
        
        if prev_count <= 1 {
            header.sequence.fetch_add(1, Ordering::Relaxed);
        }

        Ok(msg_len)
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
