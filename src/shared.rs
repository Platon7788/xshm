use std::ptr::NonNull;

use crate::layout::{ControlBlock, RingHeader};

pub struct SharedView {
    base: NonNull<u8>,
}

unsafe impl Send for SharedView {}
unsafe impl Sync for SharedView {}

impl SharedView {
    /// # Safety
    /// `base` обязан указывать на начало валидного маппинга размером не
    /// менее `shared_mapping_size()` байт (layout: `ControlBlock`
    /// `+RingHeader_A+RingBuffer_A+RingHeader_B+RingBuffer_B`), выровненного
    /// минимум на 64 байта, и оставаться валидным (не unmapped) на всё время
    /// жизни возвращаемого `SharedView` -- это гарантирует вызывающий код,
    /// держащий соответствующий `Mapping` живым.
    pub unsafe fn new(base: *mut u8) -> Self {
        SharedView {
            base: NonNull::new(base).expect("shared mapping pointer must be valid"),
        }
    }

    pub fn control_block(&self) -> &ControlBlock {
        // SAFETY: base указывает на начало маппинга (инвариант конструктора
        // new), ControlBlock -- первое поле layout'а; маппинг живёт не
        // меньше self (см. SharedView::new).
        unsafe { &*(self.base.as_ptr() as *const ControlBlock) }
    }

    pub fn control_block_ptr(&self) -> *mut ControlBlock {
        self.base.as_ptr() as *mut ControlBlock
    }

    pub fn ring_header_a(&self) -> *mut RingHeader {
        // SAFETY: смещение на size_of::<ControlBlock>() остаётся внутри
        // маппинга -- следующее поле layout'а сразу после ControlBlock.
        unsafe { self.base.as_ptr().add(std::mem::size_of::<ControlBlock>()) as *mut RingHeader }
    }

    pub fn ring_header_b(&self) -> *mut RingHeader {
        // SAFETY: ring_buffer_a() + RING_CAPACITY -- следующее поле layout'а
        // (RingHeader_B) сразу после RingBuffer_A, остаётся внутри маппинга.
        unsafe { self.ring_buffer_a().add(crate::constants::RING_CAPACITY) as *mut RingHeader }
    }

    pub fn ring_buffer_a(&self) -> *mut u8 {
        // SAFETY: смещение на size_of::<RingHeader>() от ring_header_a() --
        // следующее поле layout'а (RingBuffer_A), остаётся внутри маппинга.
        unsafe { (self.ring_header_a() as *mut u8).add(std::mem::size_of::<RingHeader>()) }
    }

    pub fn ring_buffer_b(&self) -> *mut u8 {
        // SAFETY: смещение на size_of::<RingHeader>() от ring_header_b() --
        // последнее поле layout'а (RingBuffer_B), остаётся внутри маппинга
        // (гарантировано размером, выделенным shared_mapping_size()).
        unsafe { (self.ring_header_b() as *mut u8).add(std::mem::size_of::<RingHeader>()) }
    }
}
