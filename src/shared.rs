use std::ptr::NonNull;

use crate::layout::{ControlBlock, RingHeader};

pub struct SharedView {
    base: NonNull<u8>,
}

unsafe impl Send for SharedView {}
unsafe impl Sync for SharedView {}

impl SharedView {
    pub unsafe fn new(base: *mut u8) -> Self {
        SharedView {
            base: NonNull::new(base).expect("shared mapping pointer must be valid"),
        }
    }

    pub fn control_block(&self) -> &ControlBlock {
        unsafe { &*(self.base.as_ptr() as *const ControlBlock) }
    }

    pub fn control_block_mut(&self) -> &mut ControlBlock {
        unsafe { &mut *(self.base.as_ptr() as *mut ControlBlock) }
    }

    pub fn ring_header_a(&self) -> *mut RingHeader {
        unsafe { self.base.as_ptr().add(std::mem::size_of::<ControlBlock>()) as *mut RingHeader }
    }

    pub fn ring_header_b(&self) -> *mut RingHeader {
        unsafe { self.ring_buffer_a().add(crate::constants::RING_CAPACITY) as *mut RingHeader }
    }

    pub fn ring_buffer_a(&self) -> *mut u8 {
        unsafe { (self.ring_header_a() as *mut u8).add(std::mem::size_of::<RingHeader>()) }
    }

    pub fn ring_buffer_b(&self) -> *mut u8 {
        unsafe { (self.ring_header_b() as *mut u8).add(std::mem::size_of::<RingHeader>()) }
    }
}
