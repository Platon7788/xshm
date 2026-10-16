use std::ptr::NonNull;
use std::sync::atomic::Ordering;

use crate::constants::*;
use crate::error::{Result, ShmError};
use crate::layout::{ChannelLayout, ControlBlock, LayoutBlock, RingConfig, RingHeader, split_time};
use crate::ring::RingBuffer;

/// Типизированная проекция на отображённый сегмент.
///
/// Весь unsafe, связанный с раскладкой сегмента, сосредоточен здесь:
/// раскладка либо записана нами самими (`initialize`), либо прочитана из
/// чужого сегмента и проверена против фактического размера отображения
/// (`attach`). Наружу отдаются безопасные ссылки и кольца.
#[derive(Debug)]
pub struct SharedView {
    base: NonNull<u8>,
    layout: ChannelLayout,
    /// Смещения `RingHeader` колец S2C и C2S (проверены против размера).
    s2c_offset: usize,
    c2s_offset: usize,
}

// SAFETY: тип хранит только адрес отображения, которое живёт не меньше самого
// `SharedView` (инвариант конструкторов) и не привязано к потоку-создателю.
unsafe impl Send for SharedView {}
// SAFETY: все поля, доступные через отданные наружу ссылки, атомарные --
// одновременный доступ из нескольких потоков (и процессов) безопасен.
unsafe impl Sync for SharedView {}

impl SharedView {
    /// Инициализировать только что созданный сегмент раскладкой `layout` и
    /// опубликовать его: `magic` пишется ПОСЛЕДНИМ (Release), поэтому пир,
    /// открывший секцию по имени раньше, видит `magic == 0` (`NotReady`), а
    /// не частично заполненный заголовок.
    ///
    /// # Safety
    /// `base` -- начало валидного, выровненного минимум на 64 байта
    /// отображения размером не менее `layout.segment_size()` байт, которое
    /// остаётся отображённым всё время жизни результата; `layout` проверен
    /// (`ChannelLayout::validate`).
    ///
    /// `server_start` -- время создания процесса-сервера
    /// (`win::current_process_start_time`, `0` -- не сообщать); пишется
    /// вместе с `server_pid` до `magic`.
    ///
    /// Ошибки: `Corrupted` -- нулевой `base`; `InvalidConfig` -- значение
    /// раскладки не помещается в `u32` поля `LayoutBlock` (проверенная
    /// раскладка всегда помещается). При ошибке `magic` не публикуется.
    pub unsafe fn initialize(
        base: *mut u8,
        layout: ChannelLayout,
        server_pid: u32,
        server_start: u64,
    ) -> Result<Self> {
        let segment_size = u32::try_from(layout.segment_size())
            .map_err(|_| ShmError::InvalidConfig("segment size does not fit into u32"))?;
        let view = Self {
            base: NonNull::new(base).ok_or(ShmError::Corrupted)?,
            layout,
            s2c_offset: ChannelLayout::s2c_offset(),
            c2s_offset: layout.c2s_offset(),
        };

        // Relaxed: все записи ниже публикует Release-запись `magic` в конце.
        let control = view.control_block();
        control.version.store(SHARED_VERSION, Ordering::Relaxed);
        control.generation.store(1, Ordering::Relaxed);
        control
            .server_state
            .store(HANDSHAKE_IDLE, Ordering::Relaxed);
        control
            .client_state
            .store(HANDSHAKE_IDLE, Ordering::Relaxed);
        control.slot_claim.store(CLAIM_FREE, Ordering::Relaxed);
        control.slot_owner_pid.store(0, Ordering::Relaxed);
        control.server_pid.store(server_pid, Ordering::Relaxed);
        control.client_pid.store(0, Ordering::Relaxed);
        let (start_lo, start_hi) = split_time(server_start);
        control.server_start_lo.store(start_lo, Ordering::Relaxed);
        control.server_start_hi.store(start_hi, Ordering::Relaxed);
        control.client_start_lo.store(0, Ordering::Relaxed);
        control.client_start_hi.store(0, Ordering::Relaxed);
        control.owner_start_lo.store(0, Ordering::Relaxed);
        control.owner_start_hi.store(0, Ordering::Relaxed);
        control.slot_count.store(0, Ordering::Relaxed);

        let block = view.layout_block();
        block.segment_size.store(segment_size, Ordering::Relaxed);
        block.flags.store(0, Ordering::Relaxed);
        block.s2c.store(view.s2c_offset, &layout.server_to_client)?;
        block.c2s.store(view.c2s_offset, &layout.client_to_server)?;

        let (header_a, header_b) = view.headers();
        header_a.reset(1);
        header_b.reset(1);

        control.magic.store(SHARED_MAGIC, Ordering::Release);
        Ok(view)
    }

    /// Разобрать чужой сегмент: `magic` (Acquire) -> версия -> раскладка ->
    /// границы колец против `mapped_len`. Раскладка копируется в `SharedView`
    /// один раз: дальнейшие изменения `LayoutBlock` пиром ни на что не влияют.
    ///
    /// Ошибки: `NotReady` -- сервер ещё не опубликовал сегмент (`magic == 0`),
    /// `VersionMismatch` -- другой формат, `Corrupted` -- чужой объект или
    /// раскладка, не помещающаяся в отображение.
    ///
    /// # Safety
    /// `base` -- начало валидного отображения длиной `mapped_len` байт,
    /// выровненного минимум на 64 байта (адрес view всегда выровнен на
    /// гранулу выделения) и живущего не меньше результата.
    pub unsafe fn attach(base: *mut u8, mapped_len: usize) -> Result<Self> {
        if mapped_len < SEGMENT_HEADER_SIZE {
            return Err(ShmError::Corrupted);
        }
        let base = NonNull::new(base).ok_or(ShmError::Corrupted)?;
        // SAFETY: первые SEGMENT_HEADER_SIZE байт лежат внутри отображения
        // (проверено выше), адрес выровнен на 64 (контракт функции).
        let control = unsafe { &*base.as_ptr().cast::<ControlBlock>() };
        match control.magic.load(Ordering::Acquire) {
            SHARED_MAGIC => {}
            0 => return Err(ShmError::NotReady),
            _ => return Err(ShmError::Corrupted),
        }
        let version = control.version.load(Ordering::Acquire);
        if version != SHARED_VERSION {
            return Err(ShmError::VersionMismatch {
                found: version,
                expected: SHARED_VERSION,
            });
        }

        // SAFETY: LayoutBlock -- байты 64..128, внутри проверенного префикса.
        let block_ptr = unsafe { base.as_ptr().add(size_of::<ControlBlock>()) };
        // SAFETY: адрес внутри префикса (выше), выровнен на 64 (база
        // выровнена на 64, `ControlBlock` -- ровно 64 байта); поля атомарные.
        let block = unsafe { &*block_ptr.cast::<LayoutBlock>() };
        if block.flags.load(Ordering::Relaxed) != 0 {
            return Err(ShmError::Corrupted);
        }
        let segment_size = block.segment_size.load(Ordering::Relaxed) as usize;
        if segment_size > mapped_len {
            return Err(ShmError::Corrupted);
        }
        let (s2c_offset, server_to_client) = block.s2c.load();
        let (c2s_offset, client_to_server) = block.c2s.load();
        let layout = ChannelLayout {
            server_to_client,
            client_to_server,
        };
        layout.validate().map_err(|_| ShmError::Corrupted)?;
        let ring_ok = |offset: usize, ring: &RingConfig| {
            offset.is_multiple_of(64)
                && offset >= SEGMENT_HEADER_SIZE
                && offset
                    .checked_add(size_of::<RingHeader>() + ring.capacity)
                    .is_some_and(|end| end <= segment_size)
        };
        let s2c_end = s2c_offset + size_of::<RingHeader>() + server_to_client.capacity;
        let c2s_end = c2s_offset + size_of::<RingHeader>() + client_to_server.capacity;
        let disjoint = s2c_end <= c2s_offset || c2s_end <= s2c_offset;
        if !ring_ok(s2c_offset, &server_to_client)
            || !ring_ok(c2s_offset, &client_to_server)
            || !disjoint
        {
            return Err(ShmError::Corrupted);
        }

        Ok(Self {
            base,
            layout,
            s2c_offset,
            c2s_offset,
        })
    }

    /// `ControlBlock` чужого сегмента без разбора колец (перебор слотов
    /// Multi отображает только заголовок -- `Mapping::open_header`).
    /// `None` -- сегмент не опубликован, другого формата или чужой объект.
    pub fn peek_control(mapping: &crate::win::Mapping) -> Option<&ControlBlock> {
        if mapping.size() < SEGMENT_HEADER_SIZE {
            return None;
        }
        // SAFETY: отображение не меньше заголовка (проверено), адрес view
        // выровнен на гранулу выделения; ссылка живёт не дольше `mapping`.
        let control = unsafe { &*mapping.as_ptr().cast::<ControlBlock>() };
        let published = control.magic.load(Ordering::Acquire) == SHARED_MAGIC
            && control.version.load(Ordering::Acquire) == SHARED_VERSION;
        published.then_some(control)
    }

    /// Раскладка канала (проверенная копия).
    pub const fn layout(&self) -> &ChannelLayout {
        &self.layout
    }

    pub fn control_block(&self) -> &ControlBlock {
        // SAFETY: base указывает на начало маппинга (инвариант конструкторов),
        // ControlBlock -- первое поле layout'а; все поля атомарные.
        unsafe { &*self.base.as_ptr().cast::<ControlBlock>() }
    }

    const fn layout_block(&self) -> &LayoutBlock {
        // SAFETY: LayoutBlock лежит сразу за ControlBlock, внутри маппинга
        // (размер >= SEGMENT_HEADER_SIZE -- инвариант конструкторов).
        let block = unsafe { self.base.add(size_of::<ControlBlock>()) };
        // SAFETY: адрес выше выровнен на 64 и валиден, пока жив `self`; все
        // поля атомарные.
        unsafe { block.cast::<LayoutBlock>().as_ref() }
    }

    const fn header_at(&self, offset: usize) -> &RingHeader {
        // SAFETY: offset -- одно из двух проверенных смещений (заголовок и
        // данные кольца внутри маппинга).
        let header = unsafe { self.base.add(offset) };
        // SAFETY: смещение кратно 64 -- адрес выровнен; валиден, пока жив
        // `self`; поля атомарные.
        unsafe { header.cast::<RingHeader>().as_ref() }
    }

    /// Заголовок кольца Server -> Client.
    pub const fn header_a(&self) -> &RingHeader {
        self.header_at(self.s2c_offset)
    }

    /// Заголовок кольца Client -> Server.
    pub const fn header_b(&self) -> &RingHeader {
        self.header_at(self.c2s_offset)
    }

    /// Оба заголовка разом -- почти все операции протокола (reset, смена
    /// `handshake_state`, публикация `connection_gen`) делаются симметрично.
    pub const fn headers(&self) -> (&RingHeader, &RingHeader) {
        (self.header_a(), self.header_b())
    }

    fn ring_at(&self, offset: usize, config: &RingConfig) -> RingBuffer {
        // SAFETY: заголовок и `config.capacity` байт данных за ним лежат
        // внутри маппинга (проверенное смещение, инвариант конструкторов);
        // кольцо не переживает маппинг -- владелец (`SharedServer`/
        // `SharedClient`) держит `Mapping` рядом с кольцами.
        let header = unsafe { self.base.add(offset) };
        // SAFETY: данные -- сразу за 64-байтным заголовком, внутри маппинга.
        let data = unsafe { header.add(size_of::<RingHeader>()) };
        // SAFETY: см. выше: обе области внутри маппинга, не пересекаются,
        // заголовок выровнен на 64; `config` проверен (`attach`/`initialize`).
        unsafe { RingBuffer::from_parts(header.cast::<RingHeader>(), data, config) }
    }

    /// Кольцо Server -> Client.
    pub fn ring_a(&self) -> RingBuffer {
        self.ring_at(self.s2c_offset, &self.layout.server_to_client)
    }

    /// Кольцо Client -> Server.
    pub fn ring_b(&self) -> RingBuffer {
        self.ring_at(self.c2s_offset, &self.layout.client_to_server)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::{Layout, alloc_zeroed, dealloc};

    struct Mem(*mut u8, Layout);
    impl Drop for Mem {
        fn drop(&mut self) {
            // SAFETY: выделено alloc_zeroed с этим же Layout.
            unsafe { dealloc(self.0, self.1) };
        }
    }

    fn mem(len: usize) -> Mem {
        let layout = Layout::from_size_align(len, 4096).unwrap();
        // SAFETY: ненулевой размер.
        let ptr = unsafe { alloc_zeroed(layout) };
        assert!(!ptr.is_null());
        Mem(ptr, layout)
    }

    /// Разбор тестового сегмента длиной `len` (не больше выделенного `m`).
    fn attach(m: &Mem, len: usize) -> Result<SharedView> {
        assert!(len <= m.1.size());
        // SAFETY: `m` выровнен на 4096 и живёт дольше результата в тестах;
        // len не больше выделенного.
        unsafe { SharedView::attach(m.0, len) }
    }

    fn small() -> ChannelLayout {
        ChannelLayout {
            server_to_client: RingConfig::new(8192).max_messages(7),
            client_to_server: RingConfig::new(4096).max_message_size(100),
        }
    }

    #[test]
    fn attach_reads_back_initialized_layout() {
        let layout = small();
        let m = mem(layout.segment_size());
        // SAFETY: m живёт до конца теста, размер = segment_size.
        let _server = unsafe { SharedView::initialize(m.0, layout, 42, 0) }.unwrap();
        // SAFETY: см. выше.
        let view = unsafe { SharedView::attach(m.0, layout.segment_size()) }.unwrap();
        assert_eq!(*view.layout(), layout);
        assert_eq!(view.control_block().server_pid.load(Ordering::Relaxed), 42);
        assert_eq!(view.ring_b().max_message_size(), 100);
    }

    #[test]
    fn attach_rejects_unpublished_foreign_and_truncated_segments() {
        let layout = small();
        let len = layout.segment_size();
        let m = mem(len);
        assert_eq!(attach(&m, len).unwrap_err(), ShmError::NotReady);
        // SAFETY: см. выше.
        let view = unsafe { SharedView::initialize(m.0, layout, 1, 0) }.unwrap();
        // Сегмент меньше заявленного.
        assert_eq!(attach(&m, len - 1).unwrap_err(), ShmError::Corrupted);
        // Чужая версия.
        view.control_block()
            .version
            .store(0x0001_0000, Ordering::Relaxed);
        assert!(matches!(
            attach(&m, len),
            Err(ShmError::VersionMismatch {
                found: 0x0001_0000,
                ..
            })
        ));
        view.control_block()
            .version
            .store(SHARED_VERSION, Ordering::Relaxed);
        // Враждебная раскладка: кольца перекрываются.
        let block = view.layout_block();
        block
            .c2s
            .offset
            .store(SEGMENT_HEADER_SIZE as u32 + 64, Ordering::Relaxed);
        assert_eq!(attach(&m, len).unwrap_err(), ShmError::Corrupted);
        // Ёмкость не степень двойки.
        block
            .c2s
            .offset
            .store(layout.c2s_offset() as u32, Ordering::Relaxed);
        block.c2s.capacity.store(4000, Ordering::Relaxed);
        assert_eq!(attach(&m, len).unwrap_err(), ShmError::Corrupted);
    }
}
