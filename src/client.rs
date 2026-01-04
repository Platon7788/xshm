use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::constants::{
    HANDSHAKE_CLIENT_HELLO, HANDSHAKE_IDLE, HANDSHAKE_SERVER_READY, MAX_MESSAGE_SIZE,
    MIN_MESSAGE_SIZE, SHARED_MAGIC, SHARED_VERSION,
};
use crate::error::{Result, ShmError};
use crate::events::SharedEvents;
use crate::naming::mapping_name;
use crate::ring::{RingBuffer, WriteOutcome};
use crate::shared::SharedView;
use crate::win::Mapping;

pub struct SharedClient {
    _name: String,
    _mapping: Mapping,
    view: SharedView,
    events: SharedEvents,
    ring_tx: RingBuffer,
    ring_rx: RingBuffer,
    connected: bool,
}

unsafe impl Send for SharedClient {}

impl SharedClient {
    pub fn connect(name: &str, timeout: Duration) -> Result<Self> {
        let map_name = mapping_name(name);
        let mapping = Mapping::open(&map_name)?;
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

        unsafe {
            (&*view.ring_header_a())
                .handshake_state
                .store(HANDSHAKE_CLIENT_HELLO, Ordering::Release);
            (&*view.ring_header_b())
                .handshake_state
                .store(HANDSHAKE_CLIENT_HELLO, Ordering::Release);
        }

        events.connect_req.set()?;

        if !events.connect_ack.wait(Some(timeout))? {
            view.control_block()
                .client_state
                .store(HANDSHAKE_IDLE, Ordering::Release);
            unsafe {
                (&*view.ring_header_a())
                    .handshake_state
                    .store(HANDSHAKE_IDLE, Ordering::Release);
                (&*view.ring_header_b())
                    .handshake_state
                    .store(HANDSHAKE_IDLE, Ordering::Release);
            }
            return Err(ShmError::Timeout);
        }

        if view.control_block().server_state.load(Ordering::Acquire) != HANDSHAKE_SERVER_READY {
            view.control_block()
                .client_state
                .store(HANDSHAKE_IDLE, Ordering::Release);
            unsafe {
                (&*view.ring_header_a())
                    .handshake_state
                    .store(HANDSHAKE_IDLE, Ordering::Release);
                (&*view.ring_header_b())
                    .handshake_state
                    .store(HANDSHAKE_IDLE, Ordering::Release);
            }
            return Err(ShmError::HandshakeFailed);
        }

        let generation = view.control_block().generation.load(Ordering::Acquire);
        unsafe {
            (&*view.ring_header_a())
                .connection_gen
                .store(generation, Ordering::Release);
            (&*view.ring_header_b())
                .connection_gen
                .store(generation, Ordering::Release);
            (&*view.ring_header_a())
                .handshake_state
                .store(HANDSHAKE_SERVER_READY, Ordering::Release);
            (&*view.ring_header_b())
                .handshake_state
                .store(HANDSHAKE_SERVER_READY, Ordering::Release);
        }

        let ring_tx = unsafe { RingBuffer::new(view.ring_header_b(), view.ring_buffer_b()) };
        let ring_rx = unsafe { RingBuffer::new(view.ring_header_a(), view.ring_buffer_a()) };

        let client = Self {
            _name: name.to_owned(),
            _mapping: mapping,
            view,
            events,
            ring_tx,
            ring_rx,
            connected: true,
        };

        Ok(client)
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    pub(crate) fn events(&self) -> &SharedEvents {
        &self.events
    }

    pub(crate) fn mark_disconnected(&mut self) {
        self.connected = false;
    }

    fn ensure_connected(&self) -> Result<()> {
        if !self.connected {
            Err(ShmError::NotConnected)
        } else {
            Ok(())
        }
    }

    pub fn send_to_server(&self, payload: &[u8]) -> Result<WriteOutcome> {
        self.ensure_connected()?;
        if payload.len() < MIN_MESSAGE_SIZE {
            return Err(ShmError::MessageTooSmall);
        }
        if payload.len() > MAX_MESSAGE_SIZE {
            return Err(ShmError::MessageTooLarge);
        }
        let result = self.ring_tx.write_message(payload)?;
        if result.was_empty {
            let _ = self.events.c2s.data.set();
        }
        Ok(result)
    }

    pub fn receive_from_server(&self, buffer: &mut Vec<u8>) -> Result<usize> {
        self.ensure_connected()?;
        let len = self.ring_rx.read_message(buffer)?;
        if self.ring_rx.message_count() == 0 {
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
            let control = self.view.control_block();
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
            let _ = self.events.disconnect.set();
            self.connected = false;
        }
    }
}
