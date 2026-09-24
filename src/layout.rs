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
    /// Заявка писателя, ждущего места (`wait_for_space`, lossless-отправка
    /// auto-режима): размер кадра (заголовок + payload) в байтах, `0` --
    /// никто не ждёт. Читатель, освободивший достаточно места, снимает
    /// заявку и сигналит событие `SPACE` (см. `RingBuffer::take_space_waiter`).
    ///
    /// Бывший `reserved[0]`: во всех прежних версиях поле всегда было нулём
    /// и никем не читалось, поэтому размер структуры и `SHARED_VERSION` не
    /// меняются. Старый читатель заявку просто игнорирует -- писатель тогда
    /// просыпается по прежнему сигналу «кольцо опустело» или по таймауту.
    pub space_waiter: AtomicU32,
    pub reserved: [u32; 7],
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
        self.space_waiter.store(0, Ordering::Relaxed);
    }
}

#[repr(C, align(64))]
pub struct ControlBlock {
    pub magic: u32,
    pub version: u32,
    pub generation: AtomicU32,
    pub server_state: AtomicU32,
    pub client_state: AtomicU32,
    /// Reserved поля для расширения протокола.
    /// reserved[0] используется для передачи slot_id в multi-client режиме.
    pub reserved: [AtomicU32; 11],
}

impl ControlBlock {
    pub fn reset(&mut self) {
        self.magic = SHARED_MAGIC;
        self.version = SHARED_VERSION;
        self.generation.store(1, Ordering::Relaxed);
        self.server_state.store(HANDSHAKE_IDLE, Ordering::Relaxed);
        self.client_state.store(HANDSHAKE_IDLE, Ordering::Relaxed);
        for r in &self.reserved {
            r.store(0, Ordering::Relaxed);
        }
    }
}

impl Default for ControlBlock {
    fn default() -> Self {
        ControlBlock {
            magic: SHARED_MAGIC,
            version: SHARED_VERSION,
            generation: AtomicU32::new(1),
            server_state: AtomicU32::new(HANDSHAKE_IDLE),
            client_state: AtomicU32::new(HANDSHAKE_IDLE),
            reserved: [
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
            ],
        }
    }
}

/// Общий размер сегмента (контрольный блок + 2 хэдера + 2 кольца).
pub const fn shared_mapping_size() -> usize {
    size_of::<ControlBlock>() + size_of::<RingHeader>() * 2 + RING_CAPACITY * 2
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Размеры структур -- часть wire-формата: любое изменение ломает
    /// совместимость с уже собранными пирами и с kernel-драйвером.
    #[test]
    fn layout_sizes_are_exact() {
        assert_eq!(size_of::<ControlBlock>(), 64);
        assert_eq!(align_of::<ControlBlock>(), 64);
        assert_eq!(size_of::<RingHeader>(), 64);
        assert_eq!(align_of::<RingHeader>(), 64);
        assert_eq!(shared_mapping_size(), 64 + 2 * 64 + 2 * RING_CAPACITY);
        assert_eq!(shared_mapping_size(), 4_194_496);
    }

    /// `space_waiter` занял бывший `reserved[0]`: смещения всех прежних полей
    /// и хвост `reserved` обязаны остаться на месте (wire-совместимость с
    /// пирами 0.7.0 без смены `SHARED_VERSION`).
    #[test]
    fn ring_header_offsets_are_stable() {
        use std::mem::offset_of;
        assert_eq!(offset_of!(RingHeader, write_pos), 0);
        assert_eq!(offset_of!(RingHeader, read_pos), 4);
        assert_eq!(offset_of!(RingHeader, message_count), 8);
        assert_eq!(offset_of!(RingHeader, drop_count), 12);
        assert_eq!(offset_of!(RingHeader, sequence), 16);
        assert_eq!(offset_of!(RingHeader, connection_gen), 20);
        assert_eq!(offset_of!(RingHeader, handshake_state), 24);
        assert_eq!(offset_of!(RingHeader, space_waiter), 28);
        assert_eq!(offset_of!(RingHeader, reserved), 32);
    }
}
