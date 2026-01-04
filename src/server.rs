use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::constants::{
    HANDSHAKE_CLIENT_HELLO, HANDSHAKE_IDLE, HANDSHAKE_SERVER_READY, MAX_MESSAGE_SIZE,
    MIN_MESSAGE_SIZE,
};
use crate::error::{Result, ShmError};
use crate::events::SharedEvents;
use crate::naming::mapping_name;
use crate::ring::{RingBuffer, WriteOutcome};
use crate::shared::SharedView;
use crate::win::Mapping;

pub struct SharedServer {
    _name: String,
    _mapping: Mapping,
    view: SharedView,
    events: SharedEvents,
    ring_tx: RingBuffer,
    ring_rx: RingBuffer,
    connected: bool,
}

unsafe impl Send for SharedServer {}

impl SharedServer {
    pub fn start(name: &str) -> Result<Self> {
        let map_name = mapping_name(name);
        let mapping = Mapping::create(&map_name)?;
        let view = unsafe { SharedView::new(mapping.as_ptr()) };

        let control = view.control_block_mut();
        control.reset();
        let generation = control.generation.load(Ordering::Relaxed);

        unsafe {
            let header_a = &*view.ring_header_a();
            header_a.reset(generation);
            let header_b = &*view.ring_header_b();
            header_b.reset(generation);
        }

        let events = SharedEvents::create(name)?;

        let ring_tx = unsafe { RingBuffer::new(view.ring_header_a(), view.ring_buffer_a()) };
        let ring_rx = unsafe { RingBuffer::new(view.ring_header_b(), view.ring_buffer_b()) };

        Ok(Self {
            _name: name.to_owned(),
            _mapping: mapping,
            view,
            events,
            ring_tx,
            ring_rx,
            connected: false,
        })
    }

    pub fn wait_for_client(&mut self, timeout: Option<Duration>) -> Result<()> {
        if self.connected {
            return Err(ShmError::AlreadyConnected);
        }

        if !self.events.connect_req.wait(timeout)? {
            return Err(ShmError::Timeout);
        }

        let control = self.view.control_block();
        let client_state = control.client_state.load(Ordering::Acquire);
        if client_state != HANDSHAKE_CLIENT_HELLO {
            return Err(ShmError::HandshakeFailed);
        }

        // ВАЖНО: сначала сбрасываем буферы, потом обновляем generation
        // Это гарантирует, что клиент увидит чистые буферы когда прочитает новый generation
        let current_gen = control.generation.load(Ordering::Acquire);
        let new_generation = current_gen.wrapping_add(1);
        
        unsafe {
            (&*self.view.ring_header_a()).reset(new_generation);
            (&*self.view.ring_header_b()).reset(new_generation);
        }
        
        // Теперь атомарно публикуем новый generation
        control.generation.store(new_generation, Ordering::Release);

        let header_a = unsafe { &*self.view.ring_header_a() };
        let header_b = unsafe { &*self.view.ring_header_b() };

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

        self.events.connect_ack.set()?;
        self.connected = true;
        Ok(())
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// Доступ к событиям сервера (для внутреннего использования)
    pub fn events(&self) -> &SharedEvents {
        &self.events
    }
    
    /// Доступ к shared view (для внутреннего использования)
    pub(crate) fn view(&self) -> &SharedView {
        &self.view
    }
    
    /// Установка состояния подключения (для внутреннего использования)
    pub(crate) fn set_connected(&mut self, connected: bool) {
        self.connected = connected;
    }

    pub(crate) fn mark_disconnected(&mut self) {
        self.connected = false;
        
        // Сбрасываем состояние в shared memory для возможности reconnect
        let control = self.view.control_block();
        control.server_state.store(HANDSHAKE_IDLE, Ordering::Release);
        control.client_state.store(HANDSHAKE_IDLE, Ordering::Release);
        
        unsafe {
            (&*self.view.ring_header_a())
                .handshake_state
                .store(HANDSHAKE_IDLE, Ordering::Release);
            (&*self.view.ring_header_b())
                .handshake_state
                .store(HANDSHAKE_IDLE, Ordering::Release);
        }
    }

    fn ensure_connected(&self) -> Result<()> {
        if !self.connected {
            Err(ShmError::NotConnected)
        } else {
            Ok(())
        }
    }

    pub fn send_to_client(&self, payload: &[u8]) -> Result<WriteOutcome> {
        self.ensure_connected()?;
        if payload.len() < MIN_MESSAGE_SIZE {
            return Err(ShmError::MessageTooSmall);
        }
        if payload.len() > MAX_MESSAGE_SIZE {
            return Err(ShmError::MessageTooLarge);
        }
        let result = self.ring_tx.write_message(payload)?;
        if result.was_empty {
            let _ = self.events.s2c.data.set();
        }
        Ok(result)
    }

    pub fn receive_from_client(&self, buffer: &mut Vec<u8>) -> Result<usize> {
        self.ensure_connected()?;
        let len = self.ring_rx.read_message(buffer)?;
        if self.ring_rx.message_count() == 0 {
            let _ = self.events.c2s.space.set();
        }
        Ok(len)
    }

    pub fn poll_client(&self, timeout: Option<Duration>) -> Result<bool> {
        self.ensure_connected()?;
        if !self.ring_rx.is_empty() {
            return Ok(true);
        }
        self.events.c2s.data.wait(timeout)
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
        unsafe {
            (&*self.view.ring_header_a())
                .handshake_state
                .store(HANDSHAKE_IDLE, Ordering::Release);
            (&*self.view.ring_header_b())
                .handshake_state
                .store(HANDSHAKE_IDLE, Ordering::Release);
        }
        if self.connected {
            let _ = self.events.disconnect.set();
        }
    }
}
