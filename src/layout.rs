//! Раскладка разделяемой памяти (формат `SHARED_VERSION = 0x0002_0000`) и
//! публичная конфигурация колец.
//!
//! ```text
//! 0    ControlBlock   (64)  magic, version, generation, состояния, PID, claim
//! 64   LayoutBlock    (64)  размер сегмента + дескрипторы обоих направлений
//! A    RingHeader S2C (64)  ┐ A = ring_s2c.offset (канонически 128)
//! A+64 кольцо S2C (cap_s2c) ┘
//! B    RingHeader C2S (64)  ┐ B = ring_c2s.offset (канонически A+64+cap_s2c)
//! B+64 кольцо C2S (cap_c2s) ┘
//! ```
//!
//! Сервер пишет `LayoutBlock` до публикации `magic`; клиент читает его после
//! `magic` (Acquire) и сверяет с фактическим размером отображения, прежде
//! чем трогать кольца (`SharedView::attach`). Смещения колец записаны явно, а
//! не выводятся из ёмкостей: клиент проверяет только границы, поэтому сервер
//! вправе разложить кольца иначе (например, выровнять на страницу), не меняя
//! версию формата.

use core::mem::offset_of;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::constants::*;
use crate::error::{Result, ShmError};

// Wire-поля -- `AtomicU32` в `repr(C)`: раскладка пира C23 (`uint32_t`)
// совпадает, только если `AtomicU32` выровнен как `u32`. На x86/x86_64 это
// так (`"32"`); для 64 бит -- НЕТ на i686 (`AtomicU64` выровнен на 8, `u64`
// -- на 4), поэтому 64-битных атомиков в wire-типах нет: время процесса
// хранится двумя `u32` (`*_start_lo`/`*_hi`).
#[cfg(not(target_has_atomic_primitive_alignment = "32"))]
compile_error!("xshm: AtomicU32 обязан быть выровнен как u32 (раскладка разделяемой памяти)");

/// Параметры одного направления канала (одного кольца).
///
/// Выбирает сервер при создании канала; клиент получает их из сегмента и
/// подстраивается сам (`SharedClient::layout`).
///
/// ```
/// use xshm::RingConfig;
/// // 8 МиБ, до 10 000 сообщений, сообщения до 1 МиБ.
/// let ring = RingConfig::new(8 << 20).max_messages(10_000).max_message_size(1 << 20);
/// assert!(ring.validate().is_ok());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RingConfig {
    /// Ёмкость кольца в байтах: степень двойки в
    /// `MIN_RING_CAPACITY..=MAX_RING_CAPACITY`.
    pub capacity: usize,
    /// Сколько непрочитанных сообщений может лежать в кольце одновременно
    /// (`>= 1`). Политика писателя: при `send` с перезаписью лишние
    /// вытесняют старейшие, `try_send` получает `QueueFull`.
    pub max_messages: u32,
    /// Максимальный размер payload одного сообщения (`>= 1`); кадр
    /// (`FRAME_HEADER_SIZE` + payload, выровненный на `FRAME_ALIGN`) обязан
    /// помещаться в пустое кольцо.
    pub max_message_size: usize,
}

impl RingConfig {
    /// 2 МиБ, 500 сообщений, сообщения до 65 535 байт -- раскладка xshm <= 0.9.
    pub const DEFAULT: Self = Self {
        capacity: DEFAULT_RING_CAPACITY,
        max_messages: DEFAULT_MAX_MESSAGES,
        max_message_size: DEFAULT_MAX_MESSAGE_SIZE,
    };

    /// Кольцо ёмкостью `capacity` без лимита числа сообщений (его ограничивает
    /// только объём) и с максимальным сообщением, какое влезает в пустое
    /// кольцо.
    #[must_use]
    pub const fn new(capacity: usize) -> Self {
        Self {
            capacity,
            max_messages: u32::MAX,
            max_message_size: max_payload_for(capacity),
        }
    }

    /// Задать лимит числа непрочитанных сообщений.
    #[must_use]
    pub const fn max_messages(mut self, max_messages: u32) -> Self {
        self.max_messages = max_messages;
        self
    }

    /// Задать максимальный размер payload.
    #[must_use]
    pub const fn max_message_size(mut self, max_message_size: usize) -> Self {
        self.max_message_size = max_message_size;
        self
    }

    /// Проверка ограничений (см. документацию полей).
    pub const fn validate(&self) -> Result<()> {
        if !self.capacity.is_power_of_two() {
            return Err(ShmError::InvalidConfig(
                "ring capacity must be a power of two",
            ));
        }
        if self.capacity < MIN_RING_CAPACITY || self.capacity > MAX_RING_CAPACITY {
            return Err(ShmError::InvalidConfig(
                "ring capacity must be within MIN_RING_CAPACITY..=MAX_RING_CAPACITY",
            ));
        }
        if self.max_messages == 0 {
            return Err(ShmError::InvalidConfig("max_messages must be at least 1"));
        }
        if self.max_message_size < MIN_MESSAGE_SIZE {
            return Err(ShmError::InvalidConfig(
                "max_message_size must be at least 1",
            ));
        }
        if self.max_message_size > max_payload_for(self.capacity) {
            return Err(ShmError::InvalidConfig(
                "max_message_size does not fit into an empty ring (capacity - FRAME_HEADER_SIZE)",
            ));
        }
        Ok(())
    }
}

impl Default for RingConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Наибольший payload, кадр которого помещается в пустое кольцо ёмкости
/// `capacity` (кратной `FRAME_ALIGN`).
#[must_use]
pub const fn max_payload_for(capacity: usize) -> usize {
    capacity.saturating_sub(FRAME_HEADER_SIZE)
}

/// Конфигурация обоих направлений канала.
///
/// ```
/// use xshm::{ChannelLayout, RingConfig};
/// // Сервер шлёт крупные снимки, клиент -- короткие команды.
/// let layout = ChannelLayout {
///     server_to_client: RingConfig::new(32 << 20).max_message_size(4 << 20),
///     client_to_server: RingConfig::new(64 << 10).max_messages(128),
/// };
/// assert!(layout.validate().is_ok());
/// assert_eq!(layout.segment_size(), 128 + 64 + (32 << 20) + 64 + (64 << 10));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ChannelLayout {
    /// Кольцо сервер -> клиент.
    pub server_to_client: RingConfig,
    /// Кольцо клиент -> сервер.
    pub client_to_server: RingConfig,
}

impl ChannelLayout {
    /// Оба направления по `RingConfig::DEFAULT`.
    pub const DEFAULT: Self = Self::symmetric(RingConfig::DEFAULT);

    /// Одинаковые параметры в обе стороны.
    #[must_use]
    pub const fn symmetric(ring: RingConfig) -> Self {
        Self {
            server_to_client: ring,
            client_to_server: ring,
        }
    }

    /// Проверка обоих направлений.
    pub const fn validate(&self) -> Result<()> {
        if let Err(err) = self.server_to_client.validate() {
            return Err(err);
        }
        self.client_to_server.validate()
    }

    /// Размер сегмента при канонической раскладке (заголовки + оба кольца).
    #[must_use]
    pub const fn segment_size(&self) -> usize {
        SEGMENT_HEADER_SIZE
            + size_of::<RingHeader>() * 2
            + self.server_to_client.capacity
            + self.client_to_server.capacity
    }

    /// Каноническое смещение `RingHeader` кольца S2C.
    pub(crate) const fn s2c_offset() -> usize {
        SEGMENT_HEADER_SIZE
    }

    /// Каноническое смещение `RingHeader` кольца C2S.
    pub(crate) const fn c2s_offset(&self) -> usize {
        SEGMENT_HEADER_SIZE + size_of::<RingHeader>() + self.server_to_client.capacity
    }
}

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
    /// auto-режима): размер кадра (заголовок + payload с выравниванием) в
    /// байтах, `0` -- никто не ждёт. Читатель, освободивший достаточно места,
    /// снимает заявку и сигналит событие `SPACE`
    /// (см. `RingBuffer::take_space_waiter`).
    pub space_waiter: AtomicU32,
    pub reserved: [AtomicU32; 8],
}

// Раскладка `RingHeader` -- INTEROP §c.3: проверяется при каждой сборке
// (в том числе у потребителя), а не только `cargo test`.
const _: () = {
    assert!(size_of::<RingHeader>() == 64);
    assert!(align_of::<RingHeader>() == 64);
    assert!(offset_of!(RingHeader, write_pos) == 0);
    assert!(offset_of!(RingHeader, read_pos) == 4);
    assert!(offset_of!(RingHeader, message_count) == 8);
    assert!(offset_of!(RingHeader, drop_count) == 12);
    assert!(offset_of!(RingHeader, sequence) == 16);
    assert!(offset_of!(RingHeader, connection_gen) == 20);
    assert!(offset_of!(RingHeader, handshake_state) == 24);
    assert!(offset_of!(RingHeader, space_waiter) == 28);
    assert!(offset_of!(RingHeader, reserved) == 32);
};

impl RingHeader {
    /// Relaxed: сброс выполняется либо до публикации сегмента (`magic`,
    /// Release), либо сервером в `complete_handshake` до публикации нового
    /// `generation` (Release) -- пир читает поля только после Acquire-чтения
    /// одного из них.
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

/// Заголовок сегмента. Все поля атомарные: сегмент виден другим процессам с
/// момента создания секции, поэтому даже инициализация идёт через атомики, а
/// не через `&mut`.
#[repr(C, align(64))]
pub struct ControlBlock {
    /// `SHARED_MAGIC`; публикуется ПОСЛЕДНИМ (Release), когда весь сегмент
    /// уже инициализирован. `0` -- сервер ещё не закончил инициализацию.
    pub magic: AtomicU32,
    pub version: AtomicU32,
    pub generation: AtomicU32,
    pub server_state: AtomicU32,
    pub client_state: AtomicU32,
    /// Multi: токен клиента, захватившего слот (`CLAIM_FREE` = свободен).
    pub slot_claim: AtomicU32,
    /// Multi: PID процесса, захватившего слот (liveness брошенного захвата).
    pub slot_owner_pid: AtomicU32,
    /// PID процесса-сервера: клиент держит handle процесса для детекции смерти.
    pub server_pid: AtomicU32,
    /// PID клиента: пишется до `CLIENT_HELLO`, сервер забирает `swap(0)`.
    pub client_pid: AtomicU32,
    /// Время создания процесса-сервера (`KERNEL_USER_TIMES::CreateTime`,
    /// 100-нс FILETIME), младшая половина. Пишется в `initialize` до
    /// `magic` вместе с `server_pid`; `0` -- сервер время не сообщил.
    pub server_start_lo: AtomicU32,
    /// Старшая половина `server_start`.
    pub server_start_hi: AtomicU32,
    /// Время создания процесса-клиента, младшая половина: пишется до
    /// `client_pid`/`CLIENT_HELLO`, сервер забирает парой с `client_pid`
    /// (`swap(0)`), откат handshake обнуляет.
    pub client_start_lo: AtomicU32,
    /// Старшая половина `client_start`.
    pub client_start_hi: AtomicU32,
    /// Multi: время создания процесса, захватившего слот, младшая половина;
    /// пишется сразу после CAS `slot_claim`, до `slot_owner_pid`.
    pub owner_start_lo: AtomicU32,
    /// Старшая половина `owner_start`.
    pub owner_start_hi: AtomicU32,
    /// Резерв под число слотов Multi; сейчас всегда `0`.
    pub slot_count: AtomicU32,
}

// Раскладка `ControlBlock` -- INTEROP §c.1.
const _: () = {
    assert!(size_of::<ControlBlock>() == 64);
    assert!(align_of::<ControlBlock>() == 64);
    assert!(offset_of!(ControlBlock, magic) == 0);
    assert!(offset_of!(ControlBlock, version) == 4);
    assert!(offset_of!(ControlBlock, generation) == 8);
    assert!(offset_of!(ControlBlock, server_state) == 12);
    assert!(offset_of!(ControlBlock, client_state) == 16);
    assert!(offset_of!(ControlBlock, slot_claim) == 20);
    assert!(offset_of!(ControlBlock, slot_owner_pid) == 24);
    assert!(offset_of!(ControlBlock, server_pid) == 28);
    assert!(offset_of!(ControlBlock, client_pid) == 32);
    assert!(offset_of!(ControlBlock, server_start_lo) == 36);
    assert!(offset_of!(ControlBlock, server_start_hi) == 40);
    assert!(offset_of!(ControlBlock, client_start_lo) == 44);
    assert!(offset_of!(ControlBlock, client_start_hi) == 48);
    assert!(offset_of!(ControlBlock, owner_start_lo) == 52);
    assert!(offset_of!(ControlBlock, owner_start_hi) == 56);
    assert!(offset_of!(ControlBlock, slot_count) == 60);
};

/// Склеить 64-битное время из половин `lo`/`hi` (на i686 атомарной 64-битной
/// записи нет -- поля хранятся двумя `u32`).
#[must_use]
pub(crate) const fn join_time(lo: u32, hi: u32) -> u64 {
    (lo as u64) | ((hi as u64) << 32)
}

/// Разбить 64-битное время на половины `(lo, hi)`.
#[must_use]
pub(crate) const fn split_time(time: u64) -> (u32, u32) {
    (time as u32, (time >> 32) as u32)
}

impl ControlBlock {
    /// Время создания процесса-сервера. Неизменно после публикации `magic`
    /// (клиент уже прочитал его с Acquire в `attach`), Acquire здесь -- с
    /// запасом.
    #[must_use]
    pub(crate) fn server_start_time(&self) -> u64 {
        join_time(
            self.server_start_lo.load(Ordering::Acquire),
            self.server_start_hi.load(Ordering::Acquire),
        )
    }

    /// Клиент: заявить время своего процесса. Вызывать ДО записи
    /// `client_pid` (Release) -- она публикует обе половины.
    pub(crate) fn announce_client_start(&self, time: u64) {
        let (lo, hi) = split_time(time);
        self.client_start_lo.store(lo, Ordering::Release);
        self.client_start_hi.store(hi, Ordering::Release);
    }

    /// Сервер: забрать время клиента (одноразово, как `client_pid`). Вызывать
    /// ПОСЛЕ `client_pid.swap` (AcqRel), иначе половины могут быть ещё не
    /// видны.
    pub(crate) fn take_client_start(&self) -> u64 {
        join_time(
            self.client_start_lo.swap(0, Ordering::AcqRel),
            self.client_start_hi.swap(0, Ordering::AcqRel),
        )
    }

    /// Клиент Multi: заявить время своего процесса после захвата слота.
    /// Вызывать ДО записи `slot_owner_pid` (Release).
    pub(crate) fn announce_owner_start(&self, time: u64) {
        let (lo, hi) = split_time(time);
        self.owner_start_lo.store(lo, Ordering::Release);
        self.owner_start_hi.store(hi, Ordering::Release);
    }

    /// Время владельца claim-а. Читать ПОСЛЕ Acquire-чтения `slot_owner_pid`.
    #[must_use]
    pub(crate) fn owner_start_time(&self) -> u64 {
        join_time(
            self.owner_start_lo.load(Ordering::Acquire),
            self.owner_start_hi.load(Ordering::Acquire),
        )
    }
}

/// Дескриптор одного кольца в `LayoutBlock`.
#[repr(C)]
pub struct RingDescriptor {
    /// Смещение `RingHeader` от начала сегмента; данные -- сразу за ним.
    pub offset: AtomicU32,
    pub capacity: AtomicU32,
    pub max_messages: AtomicU32,
    pub max_message_size: AtomicU32,
}

/// Самоописание сегмента: пишется сервером до публикации `magic` и далее не
/// меняется.
#[repr(C, align(64))]
pub struct LayoutBlock {
    /// Полный размер сегмента в байтах.
    pub segment_size: AtomicU32,
    /// Флаги формата; в `0x0002_0000` всегда `0` (иное значение -- отказ).
    pub flags: AtomicU32,
    /// Кольцо сервер -> клиент.
    pub s2c: RingDescriptor,
    /// Кольцо клиент -> сервер.
    pub c2s: RingDescriptor,
    pub reserved: [AtomicU32; 6],
}

// Раскладка `RingDescriptor`/`LayoutBlock` -- INTEROP §c.2 (смещения
// `LayoutBlock` -- от его начала, т.е. от байта 64 сегмента).
const _: () = {
    assert!(size_of::<RingDescriptor>() == 16);
    assert!(align_of::<RingDescriptor>() == 4);
    assert!(offset_of!(RingDescriptor, offset) == 0);
    assert!(offset_of!(RingDescriptor, capacity) == 4);
    assert!(offset_of!(RingDescriptor, max_messages) == 8);
    assert!(offset_of!(RingDescriptor, max_message_size) == 12);

    assert!(size_of::<LayoutBlock>() == 64);
    assert!(align_of::<LayoutBlock>() == 64);
    assert!(offset_of!(LayoutBlock, segment_size) == 0);
    assert!(offset_of!(LayoutBlock, flags) == 4);
    assert!(offset_of!(LayoutBlock, s2c) == 8);
    assert!(offset_of!(LayoutBlock, s2c.offset) == 8);
    assert!(offset_of!(LayoutBlock, s2c.capacity) == 12);
    assert!(offset_of!(LayoutBlock, s2c.max_messages) == 16);
    assert!(offset_of!(LayoutBlock, s2c.max_message_size) == 20);
    assert!(offset_of!(LayoutBlock, c2s) == 24);
    assert!(offset_of!(LayoutBlock, c2s.offset) == 24);
    assert!(offset_of!(LayoutBlock, c2s.capacity) == 28);
    assert!(offset_of!(LayoutBlock, c2s.max_messages) == 32);
    assert!(offset_of!(LayoutBlock, c2s.max_message_size) == 36);
    assert!(offset_of!(LayoutBlock, reserved) == 40);

    assert!(SEGMENT_HEADER_SIZE == size_of::<ControlBlock>() + size_of::<LayoutBlock>());
};

impl RingDescriptor {
    /// Relaxed: запись до публикации `magic` (Release).
    ///
    /// Значения уходят в разделяемую память как `u32`: усечение молча
    /// исказило бы раскладку, которую увидит пир, поэтому -- `try_from`.
    /// Проверенная раскладка (`ChannelLayout::validate`, ёмкость до
    /// `MAX_RING_CAPACITY`) всегда помещается; ошибка -- `InvalidConfig`.
    pub(crate) fn store(&self, offset: usize, ring: &RingConfig) -> Result<()> {
        let too_large = |_| ShmError::InvalidConfig("ring layout value does not fit into u32");
        let offset = u32::try_from(offset).map_err(too_large)?;
        let capacity = u32::try_from(ring.capacity).map_err(too_large)?;
        let max_message_size = u32::try_from(ring.max_message_size).map_err(too_large)?;
        self.offset.store(offset, Ordering::Relaxed);
        self.capacity.store(capacity, Ordering::Relaxed);
        self.max_messages
            .store(ring.max_messages, Ordering::Relaxed);
        self.max_message_size
            .store(max_message_size, Ordering::Relaxed);
        Ok(())
    }

    /// Relaxed: чтение после Acquire-чтения `magic`, поля неизменны.
    pub(crate) fn load(&self) -> (usize, RingConfig) {
        (
            self.offset.load(Ordering::Relaxed) as usize,
            RingConfig {
                capacity: self.capacity.load(Ordering::Relaxed) as usize,
                max_messages: self.max_messages.load(Ordering::Relaxed),
                max_message_size: self.max_message_size.load(Ordering::Relaxed) as usize,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Размеры и смещения -- часть wire-формата (INTEROP §c): любое изменение
    /// ломает совместимость с нативными пирами и kernel-драйвером.
    #[test]
    fn layout_sizes_are_exact() {
        assert_eq!(size_of::<ControlBlock>(), 64);
        assert_eq!(align_of::<ControlBlock>(), 64);
        assert_eq!(size_of::<LayoutBlock>(), 64);
        assert_eq!(align_of::<LayoutBlock>(), 64);
        assert_eq!(size_of::<RingHeader>(), 64);
        assert_eq!(align_of::<RingHeader>(), 64);
        assert_eq!(size_of::<RingDescriptor>(), 16);
        assert_eq!(
            SEGMENT_HEADER_SIZE,
            size_of::<ControlBlock>() + size_of::<LayoutBlock>()
        );
        assert_eq!(ChannelLayout::DEFAULT.segment_size(), 4_194_560);
    }

    #[test]
    fn control_block_offsets_are_stable() {
        assert_eq!(offset_of!(ControlBlock, magic), 0);
        assert_eq!(offset_of!(ControlBlock, version), 4);
        assert_eq!(offset_of!(ControlBlock, generation), 8);
        assert_eq!(offset_of!(ControlBlock, server_state), 12);
        assert_eq!(offset_of!(ControlBlock, client_state), 16);
        assert_eq!(offset_of!(ControlBlock, slot_claim), 20);
        assert_eq!(offset_of!(ControlBlock, slot_owner_pid), 24);
        assert_eq!(offset_of!(ControlBlock, server_pid), 28);
        assert_eq!(offset_of!(ControlBlock, client_pid), 32);
        assert_eq!(offset_of!(ControlBlock, server_start_lo), 36);
        assert_eq!(offset_of!(ControlBlock, server_start_hi), 40);
        assert_eq!(offset_of!(ControlBlock, client_start_lo), 44);
        assert_eq!(offset_of!(ControlBlock, client_start_hi), 48);
        assert_eq!(offset_of!(ControlBlock, owner_start_lo), 52);
        assert_eq!(offset_of!(ControlBlock, owner_start_hi), 56);
        assert_eq!(offset_of!(ControlBlock, slot_count), 60);
    }

    #[test]
    fn layout_block_offsets_are_stable() {
        assert_eq!(offset_of!(LayoutBlock, segment_size), 0);
        assert_eq!(offset_of!(LayoutBlock, flags), 4);
        assert_eq!(offset_of!(LayoutBlock, s2c), 8);
        assert_eq!(offset_of!(LayoutBlock, c2s), 24);
        assert_eq!(offset_of!(LayoutBlock, reserved), 40);
        assert_eq!(offset_of!(RingDescriptor, offset), 0);
        assert_eq!(offset_of!(RingDescriptor, capacity), 4);
        assert_eq!(offset_of!(RingDescriptor, max_messages), 8);
        assert_eq!(offset_of!(RingDescriptor, max_message_size), 12);
    }

    #[test]
    fn ring_header_offsets_are_stable() {
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

    #[test]
    fn ring_config_validation() {
        assert!(RingConfig::DEFAULT.validate().is_ok());
        assert!(RingConfig::new(MIN_RING_CAPACITY).validate().is_ok());
        assert!(RingConfig::new(MAX_RING_CAPACITY).validate().is_ok());
        // Не степень двойки.
        assert!(RingConfig::new(3 * 4096).validate().is_err());
        // Вне диапазона.
        assert!(RingConfig::new(2048).validate().is_err());
        assert!(RingConfig::new(MAX_RING_CAPACITY * 2).validate().is_err());
        // Нулевые лимиты.
        assert!(RingConfig::DEFAULT.max_messages(0).validate().is_err());
        assert!(RingConfig::DEFAULT.max_message_size(0).validate().is_err());
        // Сообщение, не помещающееся в пустое кольцо.
        let ring = RingConfig::new(4096);
        assert_eq!(ring.max_message_size, 4096 - FRAME_HEADER_SIZE);
        assert!(ring.max_message_size(4096 - 7).validate().is_err());
    }
}
