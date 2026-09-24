use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::constants::{
    HANDSHAKE_CLIENT_HELLO, HANDSHAKE_IDLE, HANDSHAKE_SERVER_READY, SHARED_MAGIC, SHARED_VERSION,
};
use crate::error::{Result, ShmError};
use crate::events::SharedEvents;
use crate::naming::mapping_name;
use crate::ring::{FreeSpace, RingBuffer, WriteOutcome};
use crate::shared::SharedView;
use crate::win::Mapping;

/// Откат клиентской стороны handshake в IDLE.
///
/// Вызывается на всех неуспешных путях `SharedClient::connect`: без него в
/// shared memory остался бы «висящий» `CLIENT_HELLO`, который сервер принял бы
/// за живую заявку на подключение.
fn rollback_handshake(view: &SharedView) {
    view.control_block()
        .client_state
        .store(HANDSHAKE_IDLE, Ordering::Release);
    let (header_a, header_b) = view.headers();
    header_a
        .handshake_state
        .store(HANDSHAKE_IDLE, Ordering::Release);
    header_b
        .handshake_state
        .store(HANDSHAKE_IDLE, Ordering::Release);
}

#[derive(Debug)]
pub struct SharedClient {
    _mapping: Mapping,
    view: SharedView,
    events: SharedEvents,
    ring_tx: RingBuffer,
    ring_rx: RingBuffer,
    connected: bool,
}

// SAFETY: см. `SharedServer` -- владеющий `Mapping` плюс атомарные операции в
// shared memory, привязки к потоку нет.
unsafe impl Send for SharedClient {}

impl SharedClient {
    pub fn connect(name: &str, timeout: Duration) -> Result<Self> {
        let map_name = mapping_name(name);
        let mapping = Mapping::open(&map_name)?;
        // SAFETY: `Mapping::open` уже проверил, что отображение не меньше
        // `shared_mapping_size()`, и держит его живым, пока жив `mapping`.
        let view = unsafe { SharedView::new(mapping.as_ptr()) };

        // Проверка magic и version для валидации shared memory
        let control = view.control_block();
        if control.magic != SHARED_MAGIC {
            return Err(ShmError::Corrupted);
        }
        if control.version != SHARED_VERSION {
            return Err(ShmError::HandshakeFailed);
        }

        let events = SharedEvents::open(name)?;

        view.control_block()
            .client_state
            .store(HANDSHAKE_CLIENT_HELLO, Ordering::Release);

        let (header_a, header_b) = view.headers();
        header_a
            .handshake_state
            .store(HANDSHAKE_CLIENT_HELLO, Ordering::Release);
        header_b
            .handshake_state
            .store(HANDSHAKE_CLIENT_HELLO, Ordering::Release);

        events.connect_req.set()?;

        if !events.connect_ack.wait(Some(timeout))? {
            rollback_handshake(&view);
            return Err(ShmError::Timeout);
        }

        if view.control_block().server_state.load(Ordering::Acquire) != HANDSHAKE_SERVER_READY {
            rollback_handshake(&view);
            return Err(ShmError::HandshakeFailed);
        }

        let generation = view.control_block().generation.load(Ordering::Acquire);
        header_a.connection_gen.store(generation, Ordering::Release);
        header_b.connection_gen.store(generation, Ordering::Release);
        header_a
            .handshake_state
            .store(HANDSHAKE_SERVER_READY, Ordering::Release);
        header_b
            .handshake_state
            .store(HANDSHAKE_SERVER_READY, Ordering::Release);

        // SAFETY: указатели из `SharedView` -- внутрь живого маппинга, выровнены
        // по layout'у и не пересекаются; `mapping` переезжает в возвращаемую
        // структуру, поэтому переживает кольца.
        let ring_tx = unsafe { RingBuffer::new(view.ring_header_b(), view.ring_buffer_b()) };
        // SAFETY: см. выше (встречное кольцо того же маппинга).
        let ring_rx = unsafe { RingBuffer::new(view.ring_header_a(), view.ring_buffer_a()) };

        let client = Self {
            _mapping: mapping,
            view,
            events,
            ring_tx,
            ring_rx,
            connected: true,
        };

        Ok(client)
    }

    #[must_use]
    pub const fn is_connected(&self) -> bool {
        self.connected
    }

    pub(crate) const fn events(&self) -> &SharedEvents {
        &self.events
    }

    pub(crate) const fn mark_disconnected(&mut self) {
        self.connected = false;
    }

    const fn ensure_connected(&self) -> Result<()> {
        if !self.connected {
            Err(ShmError::NotConnected)
        } else {
            Ok(())
        }
    }

    pub fn send_to_server(&self, payload: &[u8]) -> Result<WriteOutcome> {
        self.ensure_connected()?;
        let result = self.ring_tx.write_message(payload)?;
        if result.was_empty {
            let _ = self.events.c2s.data.set();
        }
        Ok(result)
    }

    /// Отправка серверу **без перезаписи** непрочитанных данных.
    ///
    /// Сообщение либо целиком ложится в свободное место кольца
    /// Client -> Server, либо `Err(ShmError::QueueFull)` и кольцо не
    /// меняется. Семантика -- `SharedServer::try_send_to_client`.
    pub fn try_send_to_server(&self, payload: &[u8]) -> Result<WriteOutcome> {
        self.ensure_connected()?;
        let result = self.ring_tx.try_write_message(payload)?;
        if result.was_empty {
            let _ = self.events.c2s.data.set();
        }
        Ok(result)
    }

    /// Свободное место в кольце Client -> Server (нижняя граница из потока
    /// писателя, см. `SharedServer::free_space`). Вне подключения --
    /// `FreeSpace::ZERO`.
    #[must_use]
    pub fn free_space(&self) -> FreeSpace {
        if self.connected {
            self.ring_tx.free_space()
        } else {
            FreeSpace::ZERO
        }
    }

    /// Ждать места в кольце Client -> Server под payload длины `payload_len`
    /// (семантика -- `SharedServer::wait_for_space`).
    pub fn wait_for_space(&self, payload_len: usize, timeout: Option<Duration>) -> Result<bool> {
        self.ensure_connected()?;
        self.ring_tx
            .wait_for_space(payload_len, Some(&self.events.c2s.space), timeout)
    }

    /// Для auto-режима: заявка/снятие ожидания места в исходящем кольце.
    pub(crate) fn arm_space_waiter(&self, frame: u32) {
        self.ring_tx.arm_space_waiter(frame);
    }

    pub(crate) fn disarm_space_waiter(&self) {
        self.ring_tx.disarm_space_waiter();
    }

    pub fn receive_from_server(&self, buffer: &mut Vec<u8>) -> Result<usize> {
        self.ensure_connected()?;
        let len = self.ring_rx.read_message(buffer)?;
        // SPACE -- когда кольцо опустело ИЛИ писатель-сервер ждёт места и
        // его теперь хватает (см. `SharedServer::receive_from_client`).
        if self.ring_rx.take_space_waiter() || self.ring_rx.message_count() == 0 {
            let _ = self.events.s2c.space.set();
        }
        Ok(len)
    }

    pub fn poll_server(&self, timeout: Option<Duration>) -> Result<bool> {
        self.ensure_connected()?;
        if !self.ring_rx.is_empty() {
            return Ok(true);
        }
        self.events.s2c.data.wait(timeout)
    }
}

impl Drop for SharedClient {
    fn drop(&mut self) {
        if self.connected {
            rollback_handshake(&self.view);
            let _ = self.events.disconnect.set();
            self.connected = false;
        }
    }
}
