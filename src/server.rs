use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::constants::{HANDSHAKE_CLIENT_HELLO, HANDSHAKE_IDLE, HANDSHAKE_SERVER_READY};
use crate::error::{Result, ShmError};
use crate::events::SharedEvents;
use crate::naming::mapping_name;
use crate::ring::{RingBuffer, WriteOutcome};
use crate::shared::SharedView;
use crate::win::Mapping;

#[derive(Debug)]
pub struct SharedServer {
    _mapping: Mapping,
    view: SharedView,
    events: Option<SharedEvents>, // None для anonymous режима
    ring_tx: RingBuffer,
    ring_rx: RingBuffer,
    connected: bool,
}

// SAFETY: все поля либо владеющие (`Mapping`), либо синхронизируются через
// атомарные операции в shared memory; привязки к потоку-создателю нет.
unsafe impl Send for SharedServer {}

impl SharedServer {
    pub fn start(name: &str) -> Result<Self> {
        let map_name = mapping_name(name);
        let mapping = Mapping::create(&map_name)?;
        // SAFETY: `Mapping` только что создал отображение нужного размера и
        // держит его живым, пока жив сам (а он переезжает в возвращаемую структуру).
        let view = unsafe { SharedView::new(mapping.as_ptr()) };

        // SAFETY: сегмент только что создан этим процессом, клиент к нему ещё не
        // подключён (его handshake начинается позже), поэтому эксклюзивная ссылка
        // на control block здесь не алиасится ни другим потоком, ни другим процессом.
        let control = unsafe { &mut *view.control_block_ptr() };
        control.reset();
        let generation = control.generation.load(Ordering::Relaxed);

        let (header_a, header_b) = view.headers();
        header_a.reset(generation);
        header_b.reset(generation);

        let events = SharedEvents::create(name)?;

        // SAFETY: указатели получены от `SharedView`, т.е. лежат внутри живого
        // маппинга, выровнены по layout'у и не пересекаются между собой; маппинг
        // переезжает в возвращаемую структуру и живёт не меньше колец.
        let ring_tx = unsafe { RingBuffer::new(view.ring_header_a(), view.ring_buffer_a()) };
        // SAFETY: см. выше (второе кольцо, другой диапазон того же маппинга).
        let ring_rx = unsafe { RingBuffer::new(view.ring_header_b(), view.ring_buffer_b()) };

        Ok(Self {
            _mapping: mapping,
            view,
            events: Some(events),
            ring_tx,
            ring_rx,
            connected: false,
        })
    }

    /// Создание anonymous сервера без имени (только через handle)
    ///
    /// Anonymous сервер создает section без имени в глобальном namespace.
    /// Доступ к section возможен только через handle, что идеально для
    /// передачи handle в kernel driver. Events не создаются, используется
    /// polling-based handshake через `wait_for_client_noevent()`.
    ///
    /// **ВАЖНО:** Это НЕ то же самое, что `start("")` (пустая строка).
    /// Windows NT API требует именно `ObjectName = NULL` в `OBJECT_ATTRIBUTES`
    /// для создания anonymous section. Пустая строка через `NtName::new("")`
    /// создаст `UNICODE_STRING` с путем `"\\BaseNamedObjects\\"`, что является
    /// именованной секцией, а не anonymous. Поэтому нужна отдельная функция.
    pub fn start_anonymous() -> Result<Self> {
        let mapping = Mapping::create_anonymous()?;
        // SAFETY: `Mapping` только что создал отображение нужного размера и
        // держит его живым, пока жив сам (а он переезжает в возвращаемую структуру).
        let view = unsafe { SharedView::new(mapping.as_ptr()) };

        // SAFETY: сегмент только что создан этим процессом, клиент к нему ещё не
        // подключён (его handshake начинается позже), поэтому эксклюзивная ссылка
        // на control block здесь не алиасится ни другим потоком, ни другим процессом.
        let control = unsafe { &mut *view.control_block_ptr() };
        control.reset();
        let generation = control.generation.load(Ordering::Relaxed);

        let (header_a, header_b) = view.headers();
        header_a.reset(generation);
        header_b.reset(generation);

        // Events не создаются для anonymous режима - используется polling

        // SAFETY: указатели получены от `SharedView`, т.е. лежат внутри живого
        // маппинга, выровнены по layout'у и не пересекаются между собой; маппинг
        // переезжает в возвращаемую структуру и живёт не меньше колец.
        let ring_tx = unsafe { RingBuffer::new(view.ring_header_a(), view.ring_buffer_a()) };
        // SAFETY: см. выше (второе кольцо, другой диапазон того же маппинга).
        let ring_rx = unsafe { RingBuffer::new(view.ring_header_b(), view.ring_buffer_b()) };

        Ok(Self {
            _mapping: mapping,
            view,
            events: None, // No events for anonymous mode
            ring_tx,
            ring_rx,
            connected: false,
        })
    }

    /// Получить handles событий для передачи в kernel driver
    ///
    /// Возвращает `None` если сервер создан в anonymous режиме (без событий).
    /// В этом случае используется polling через `wait_for_client_noevent()`.
    #[must_use]
    pub fn get_event_handles(&self) -> Option<crate::events::EventHandles> {
        self.events.as_ref().map(|e| e.get_event_handles())
    }

    pub fn wait_for_client(&mut self, timeout: Option<Duration>) -> Result<()> {
        if self.connected {
            return Err(ShmError::AlreadyConnected);
        }

        // Для anonymous режима используем polling
        if self.events.is_none() {
            return self.wait_for_client_noevent(timeout);
        }

        if !self.events.as_ref().unwrap().connect_req.wait(timeout)? {
            return Err(ShmError::Timeout);
        }

        let client_state = self
            .view
            .control_block()
            .client_state
            .load(Ordering::Acquire);
        if client_state != HANDSHAKE_CLIENT_HELLO {
            return Err(ShmError::HandshakeFailed);
        }

        self.complete_handshake()
    }

    /// Ожидание клиента без событий (polling по shared memory).
    /// Использовать когда нет доступа к именованным событиям.
    pub fn wait_for_client_noevent(&mut self, timeout: Option<Duration>) -> Result<()> {
        if self.connected {
            return Err(ShmError::AlreadyConnected);
        }

        let control = self.view.control_block();
        let start = std::time::Instant::now();

        loop {
            let client_state = control.client_state.load(Ordering::Acquire);
            if client_state == HANDSHAKE_CLIENT_HELLO {
                break;
            }

            if let Some(t) = timeout
                && start.elapsed() >= t
            {
                return Err(ShmError::Timeout);
            }

            std::thread::sleep(Duration::from_millis(1));
        }

        self.complete_handshake()
    }

    /// Завершение серверной стороны handshake, общее для событийного и
    /// polling-путей: сброс колец -> публикация нового generation ->
    /// SERVER_READY -> сигнал connect_ack.
    ///
    /// Порядок критичен: буферы сбрасываются ДО публикации generation
    /// (`Release`), поэтому клиент, прочитавший новый generation (`Acquire`),
    /// гарантированно видит уже очищенные кольца.
    fn complete_handshake(&mut self) -> Result<()> {
        let control = self.view.control_block();
        let new_generation = control.generation.load(Ordering::Acquire).wrapping_add(1);

        let (header_a, header_b) = self.view.headers();
        header_a.reset(new_generation);
        header_b.reset(new_generation);

        control.generation.store(new_generation, Ordering::Release);

        header_a
            .handshake_state
            .store(HANDSHAKE_SERVER_READY, Ordering::Release);
        header_b
            .handshake_state
            .store(HANDSHAKE_SERVER_READY, Ordering::Release);

        control
            .server_state
            .store(HANDSHAKE_SERVER_READY, Ordering::Release);
        control
            .client_state
            .store(HANDSHAKE_SERVER_READY, Ordering::Release);

        // Для anonymous-сервера событий нет -- клиент узнаёт о готовности
        // опросом `server_state`.
        if let Some(events) = &self.events {
            events.connect_ack.set()?;
        }
        self.connected = true;
        Ok(())
    }

    #[must_use]
    pub const fn is_connected(&self) -> bool {
        self.connected
    }

    /// Доступ к событиям сервера (для внутреннего использования)
    ///
    /// ВАЖНО: Для anonymous режима возвращает None - события не создаются.
    /// Используйте только для named режима (SharedServer::start).
    #[must_use]
    pub const fn events(&self) -> Option<&SharedEvents> {
        self.events.as_ref()
    }

    /// Проверка, является ли сервер anonymous (без имени и events)
    #[must_use]
    pub const fn is_anonymous(&self) -> bool {
        self.events.is_none()
    }

    /// Получить raw HANDLE секции для передачи в kernel driver
    /// ВАЖНО: Handle принадлежит Mapping, не закрывать вручную!
    #[must_use]
    pub fn section_handle(&self) -> isize {
        self._mapping.section_handle()
    }

    /// Доступ к shared view (для внутреннего использования)
    pub(crate) const fn view(&self) -> &SharedView {
        &self.view
    }

    /// Установка состояния подключения (для внутреннего использования)
    pub(crate) const fn set_connected(&mut self, connected: bool) {
        self.connected = connected;
    }

    pub(crate) fn mark_disconnected(&mut self) {
        self.connected = false;

        // Сбрасываем состояние в shared memory для возможности reconnect
        let control = self.view.control_block();
        control
            .server_state
            .store(HANDSHAKE_IDLE, Ordering::Release);
        control
            .client_state
            .store(HANDSHAKE_IDLE, Ordering::Release);

        let (header_a, header_b) = self.view.headers();
        header_a
            .handshake_state
            .store(HANDSHAKE_IDLE, Ordering::Release);
        header_b
            .handshake_state
            .store(HANDSHAKE_IDLE, Ordering::Release);
    }

    const fn ensure_connected(&self) -> Result<()> {
        if !self.connected {
            Err(ShmError::NotConnected)
        } else {
            Ok(())
        }
    }

    pub fn send_to_client(&self, payload: &[u8]) -> Result<WriteOutcome> {
        self.ensure_connected()?;
        let result = self.ring_tx.write_message(payload)?;
        // Сигнализируем только если events доступны
        if let Some(ref events) = self.events
            && result.was_empty
        {
            let _ = events.s2c.data.set();
        }
        Ok(result)
    }

    pub fn receive_from_client(&self, buffer: &mut Vec<u8>) -> Result<usize> {
        self.ensure_connected()?;
        let len = self.ring_rx.read_message(buffer)?;
        // Сигнализируем только если events доступны
        if let Some(ref events) = self.events
            && self.ring_rx.message_count() == 0
        {
            let _ = events.c2s.space.set();
        }
        Ok(len)
    }

    pub fn poll_client(&self, timeout: Option<Duration>) -> Result<bool> {
        self.ensure_connected()?;
        if !self.ring_rx.is_empty() {
            return Ok(true);
        }
        // Для anonymous режима просто проверяем буфер (polling)
        if self.events.is_none() {
            return Ok(false); // Нет данных, но не timeout
        }
        self.events.as_ref().unwrap().c2s.data.wait(timeout)
    }
}

impl Drop for SharedServer {
    fn drop(&mut self) {
        let control = self.view.control_block();
        control
            .server_state
            .store(HANDSHAKE_IDLE, Ordering::Release);
        control
            .client_state
            .store(HANDSHAKE_IDLE, Ordering::Release);
        let (header_a, header_b) = self.view.headers();
        header_a
            .handshake_state
            .store(HANDSHAKE_IDLE, Ordering::Release);
        header_b
            .handshake_state
            .store(HANDSHAKE_IDLE, Ordering::Release);
        if self.connected
            && let Some(ref events) = self.events
        {
            let _ = events.disconnect.set();
        }
    }
}
