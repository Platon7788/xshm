use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::constants::{
    HANDSHAKE_CLIENT_HELLO, HANDSHAKE_IDLE, HANDSHAKE_SERVER_READY, RESERVED_CLIENT_PID_INDEX,
    RESERVED_SERVER_PID_INDEX,
};
use crate::error::{Result, ShmError};
use crate::events::SharedEvents;
use crate::naming::mapping_name;
use crate::ring::{FreeSpace, RingBuffer, WriteOutcome};
use crate::shared::SharedView;
use crate::win::{Mapping, ProcessWatch, wait_any};

#[derive(Debug)]
pub struct SharedServer {
    _mapping: Mapping,
    view: SharedView,
    events: Option<SharedEvents>, // None для anonymous режима
    ring_tx: RingBuffer,
    ring_rx: RingBuffer,
    connected: bool,
    /// Удерживаемый handle процесса клиента (если клиент 0.8+ передал PID и
    /// процесс удалось открыть) -- для детекции его смерти.
    peer: Option<ProcessWatch>,
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
        // Протокол 0.8+: PID сервера для детекции его смерти клиентом.
        control.reserved[RESERVED_SERVER_PID_INDEX].store(std::process::id(), Ordering::Release);
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
            peer: None,
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
        // Протокол 0.8+: PID сервера для детекции его смерти клиентом.
        control.reserved[RESERVED_SERVER_PID_INDEX].store(std::process::id(), Ordering::Release);
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
            peer: None,
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
        // Протокол 0.8+: PID клиента, записанный им ДО CLIENT_HELLO (видим
        // благодаря Acquire-чтению client_state в wait_for_client*). `swap(0)`:
        // значение одноразовое -- клиент старой версии, пришедший следом,
        // не унаследует чужой PID. 0 -- клиент старой версии, наблюдения нет.
        let client_pid = control.reserved[RESERVED_CLIENT_PID_INDEX].swap(0, Ordering::AcqRel);
        self.peer = ProcessWatch::open_peer(client_pid);
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
        // Закрываем handle процесса бывшего клиента (RAII).
        self.peer = None;

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

    /// Отправка клиенту **без перезаписи** непрочитанных данных.
    ///
    /// Сообщение либо целиком ложится в свободное место кольца
    /// Server -> Client, либо возвращается `Err(ShmError::QueueFull)` и
    /// кольцо не меняется. Подробная семантика -- `RingBuffer::try_write_message`
    /// (атомарность, консервативность проверки, `MessageTooSmall`/`MessageTooLarge`).
    /// Ждать освобождения места -- `wait_for_space`.
    pub fn try_send_to_client(&self, payload: &[u8]) -> Result<WriteOutcome> {
        self.ensure_connected()?;
        let result = self.ring_tx.try_write_message(payload)?;
        if let Some(ref events) = self.events
            && result.was_empty
        {
            let _ = events.s2c.data.set();
        }
        Ok(result)
    }

    /// Свободное место в кольце Server -> Client.
    ///
    /// Из потока, который пишет в это кольцо, -- нижняя граница: если
    /// `free_space().fits(n)`, следующий `try_send_to_client` с payload длины
    /// `n` пройдёт. Вне подключения возвращает `FreeSpace::ZERO`.
    #[must_use]
    pub fn free_space(&self) -> FreeSpace {
        if self.connected {
            self.ring_tx.free_space()
        } else {
            FreeSpace::ZERO
        }
    }

    /// Ждать, пока в кольце Server -> Client освободится место под payload
    /// длины `payload_len`. `Ok(true)` -- место есть, `Ok(false)` -- таймаут;
    /// `timeout = None` -- без ограничения (мёртвый клиент место не
    /// освободит -- лучше задавать таймаут и параллельно следить за живостью).
    ///
    /// Будит сигнал читателя (событие `SPACE`, только когда места реально
    /// хватает); без событий (anonymous) и со старым клиентом, не знающим про
    /// заявку, работает опросом с шагом не больше 50 мс.
    pub fn wait_for_space(&self, payload_len: usize, timeout: Option<Duration>) -> Result<bool> {
        self.ensure_connected()?;
        let event = self.events.as_ref().map(|e| &e.s2c.space);
        self.ring_tx
            .wait_for_space(payload_len, event, self.peer.as_ref(), timeout)
    }

    /// Для auto-режима: заявка/снятие ожидания места в исходящем кольце.
    pub(crate) fn arm_space_waiter(&self, frame: u32) {
        self.ring_tx.arm_space_waiter(frame);
    }

    pub(crate) fn disarm_space_waiter(&self) {
        self.ring_tx.disarm_space_waiter();
    }

    pub fn receive_from_client(&self, buffer: &mut Vec<u8>) -> Result<usize> {
        self.ensure_connected()?;
        let len = self.ring_rx.read_message(buffer)?;
        // Сигнализируем только если events доступны. SPACE -- когда кольцо
        // опустело (как раньше) ИЛИ когда писатель оставил заявку и места
        // ему теперь хватает (`try_send`/`wait_for_space` на той стороне).
        if let Some(ref events) = self.events
            && (self.ring_rx.take_space_waiter() || self.ring_rx.message_count() == 0)
        {
            let _ = events.c2s.space.set();
        }
        Ok(len)
    }

    /// Ждать данных от клиента. `Ok(true)` -- в кольце есть сообщение,
    /// `Ok(false)` -- таймаут (или anonymous-режим без данных),
    /// `Err(PeerDied)` -- процесс клиента умер и всё, что он успел записать,
    /// уже прочитано (пока в кольце есть данные, возвращается `Ok(true)`).
    pub fn poll_client(&self, timeout: Option<Duration>) -> Result<bool> {
        self.ensure_connected()?;
        if !self.ring_rx.is_empty() {
            return Ok(true);
        }
        // Для anonymous режима просто проверяем буфер (polling)
        let Some(events) = self.events.as_ref() else {
            return match &self.peer {
                Some(peer) if peer.has_exited() && self.ring_rx.is_empty() => {
                    Err(ShmError::PeerDied)
                }
                _ => Ok(false), // Нет данных, но не timeout
            };
        };
        poll_with_peer(&events.c2s.data, self.peer.as_ref(), &self.ring_rx, timeout)
    }

    /// PID процесса клиента, если клиент 0.8+ передал его в handshake.
    #[must_use]
    pub fn peer_pid(&self) -> Option<u32> {
        self.peer.as_ref().map(ProcessWatch::pid)
    }

    /// Жив ли процесс клиента: `Some(true/false)` -- клиент наблюдается через
    /// удерживаемый handle процесса; `None` -- не подключён или наблюдения
    /// нет (клиент старой версии, не передал PID, или handle не открылся --
    /// например, клиент в другой сессии без прав). Один syscall, без блокировки.
    #[must_use]
    pub fn is_peer_alive(&self) -> Option<bool> {
        if !self.connected {
            return None;
        }
        self.peer.as_ref().map(|peer| !peer.has_exited())
    }

    /// Handle процесса клиента для набора ожидания worker-а (auto-режим).
    pub(crate) fn peer_wait_handle(&self) -> Option<isize> {
        self.peer.as_ref().map(ProcessWatch::raw_handle)
    }
}

/// Общий для сервера и клиента `poll_*`: ждём событие данных ИЛИ смерть пира.
///
/// Индекс 0 -- данные (приоритет: при одновременном сигнале NT возвращает
/// наименьший индекс, так что последние данные мёртвого пира не теряются).
pub(crate) fn poll_with_peer(
    data: &crate::win::EventHandle,
    peer: Option<&ProcessWatch>,
    ring: &RingBuffer,
    timeout: Option<Duration>,
) -> Result<bool> {
    let Some(peer) = peer else {
        return data.wait(timeout);
    };
    match wait_any(&[data.raw_handle(), peer.raw_handle()], timeout)? {
        Some(0) => Ok(true),
        Some(_) if !ring.is_empty() => Ok(true),
        Some(_) => Err(ShmError::PeerDied),
        None => Ok(false),
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
