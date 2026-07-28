use std::ptr::NonNull;

use crate::layout::{ControlBlock, RingHeader};

/// Типизированная проекция на отображённый сегмент.
///
/// Весь unsafe, связанный с раскладкой сегмента, сосредоточен здесь: инвариант
/// «`base` указывает на валидный маппинг размером не меньше
/// `shared_mapping_size()`» проверяется один раз в конструкторе, а наружу
/// отдаются безопасные ссылки (`control_block`, `header_a`, `header_b`).
#[derive(Debug)]
pub struct SharedView {
    base: NonNull<u8>,
}

// SAFETY: тип хранит только адрес отображения, которое живёт не меньше самого
// `SharedView` (инвариант конструктора) и не привязано к потоку-создателю.
unsafe impl Send for SharedView {}
// SAFETY: все поля, доступные через отданные наружу ссылки, атомарные --
// одновременный доступ из нескольких потоков (и процессов) безопасен.
unsafe impl Sync for SharedView {}

impl SharedView {
    /// # Safety
    /// `base` обязан указывать на начало валидного маппинга размером не
    /// менее `shared_mapping_size()` байт (layout: `ControlBlock`
    /// `+RingHeader_A+RingBuffer_A+RingHeader_B+RingBuffer_B`), выровненного
    /// минимум на 64 байта, и оставаться валидным (не unmapped) на всё время
    /// жизни возвращаемого `SharedView` -- это гарантирует вызывающий код,
    /// держащий соответствующий `Mapping` живым.
    pub const unsafe fn new(base: *mut u8) -> Self {
        SharedView {
            base: NonNull::new(base).expect("shared mapping pointer must be valid"),
        }
    }

    pub fn control_block(&self) -> &ControlBlock {
        // SAFETY: base указывает на начало маппинга (инвариант конструктора
        // new), ControlBlock -- первое поле layout'а; маппинг живёт не
        // меньше self (см. SharedView::new).
        unsafe { &*self.base.as_ptr().cast::<ControlBlock>() }
    }

    pub const fn control_block_ptr(&self) -> *mut ControlBlock {
        self.base.as_ptr().cast::<ControlBlock>()
    }

    /// Заголовок кольца Server -> Client.
    pub fn header_a(&self) -> &RingHeader {
        // SAFETY: `ring_header_a()` даёт указатель внутрь маппинга (см. инвариант
        // конструктора), выровненный на 64 байта самим layout'ом; все поля
        // `RingHeader` атомарные, поэтому shared-ссылка допускает и запись.
        unsafe { &*self.ring_header_a() }
    }

    /// Заголовок кольца Client -> Server.
    pub fn header_b(&self) -> &RingHeader {
        // SAFETY: см. `header_a`.
        unsafe { &*self.ring_header_b() }
    }

    /// Оба заголовка разом -- почти все операции протокола (reset, смена
    /// `handshake_state`, публикация `connection_gen`) делаются симметрично.
    pub fn headers(&self) -> (&RingHeader, &RingHeader) {
        (self.header_a(), self.header_b())
    }

    pub const fn ring_header_a(&self) -> *mut RingHeader {
        // SAFETY: смещение на size_of::<ControlBlock>() остаётся внутри
        // маппинга -- следующее поле layout'а сразу после ControlBlock.
        unsafe {
            self.base
                .as_ptr()
                .add(size_of::<ControlBlock>())
                .cast::<RingHeader>()
        }
    }

    pub const fn ring_header_b(&self) -> *mut RingHeader {
        // SAFETY: ring_buffer_a() + RING_CAPACITY -- следующее поле layout'а
        // (RingHeader_B) сразу после RingBuffer_A, остаётся внутри маппинга.
        unsafe {
            self.ring_buffer_a()
                .add(crate::constants::RING_CAPACITY)
                .cast::<RingHeader>()
        }
    }

    pub const fn ring_buffer_a(&self) -> *mut u8 {
        // SAFETY: смещение на size_of::<RingHeader>() от ring_header_a() --
        // следующее поле layout'а (RingBuffer_A), остаётся внутри маппинга.
        unsafe {
            self.ring_header_a()
                .cast::<u8>()
                .add(size_of::<RingHeader>())
        }
    }

    pub const fn ring_buffer_b(&self) -> *mut u8 {
        // SAFETY: смещение на size_of::<RingHeader>() от ring_header_b() --
        // последнее поле layout'а (RingBuffer_B), остаётся внутри маппинга
        // (гарантировано размером, выделенным shared_mapping_size()).
        unsafe {
            self.ring_header_b()
                .cast::<u8>()
                .add(size_of::<RingHeader>())
        }
    }
}
