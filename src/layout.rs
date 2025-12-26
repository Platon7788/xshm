use core::sync::atomic::{AtomicU32, Ordering};

use crate::constants::*;

#[repr(C, align(64))]
pub struct RingHeader {
    pub write_pos: AtomicU32,
    pub read_pos: AtomicU32,
    pub message_count: AtomicU32,
    pub drop_count: AtomicU32,
    pub sequence: AtomicU32,
    pub connection_gen: AtomicU32,
    pub handshake_state: AtomicU32,
    pub reserved: [u32; 8],
}

impl RingHeader {
    pub fn reset(&self, generation: u32) {
        self.write_pos.store(0, Ordering::Relaxed);
        self.read_pos.store(0, Ordering::Relaxed);
        self.message_count.store(0, Ordering::Relaxed);
        self.drop_count.store(0, Ordering::Relaxed);
        self.sequence.store(0, Ordering::Relaxed);
        self.connection_gen.store(generation, Ordering::Relaxed);
        self.handshake_state
            .store(HANDSHAKE_IDLE, Ordering::Relaxed);
    }
}

#[repr(C, align(64))]
pub struct ControlBlock {
    pub magic: u32,
    pub version: u32,
    pub generation: AtomicU32,
    pub server_state: AtomicU32,
    pub client_state: AtomicU32,
    pub reserved: [u32; 11],
}

impl ControlBlock {
    pub fn reset(&mut self) {
        self.magic = SHARED_MAGIC;
        self.version = SHARED_VERSION;
        self.generation.store(1, Ordering::Relaxed);
        self.server_state.store(HANDSHAKE_IDLE, Ordering::Relaxed);
        self.client_state.store(HANDSHAKE_IDLE, Ordering::Relaxed);
    }
}

impl Default for ControlBlock {
    fn default() -> Self {
        let block = ControlBlock {
            magic: SHARED_MAGIC,
            version: SHARED_VERSION,
            generation: AtomicU32::new(1),
            server_state: AtomicU32::new(HANDSHAKE_IDLE),
            client_state: AtomicU32::new(HANDSHAKE_IDLE),
            reserved: [0; 11],
        };
        block
    }
}

/// Общий размер сегмента (контрольный блок + 2 хэдера + 2 кольца).
pub const fn shared_mapping_size() -> usize {
    core::mem::size_of::<ControlBlock>()
        + core::mem::size_of::<RingHeader>() * 2
        + RING_CAPACITY * 2
}
