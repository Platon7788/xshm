use crate::constants::{
    EVENT_CONNECT_REQ_SUFFIX, EVENT_CONNECT_SUFFIX, EVENT_DATA_SUFFIX, EVENT_DISCONNECT_SUFFIX,
    EVENT_SPACE_SUFFIX,
};
use crate::error::Result;
use crate::naming::{Direction, event_name};
use crate::win::EventHandle;

pub struct ChannelEvents {
    pub data: EventHandle,
    pub space: EventHandle,
}

pub struct SharedEvents {
    pub s2c: ChannelEvents,
    pub c2s: ChannelEvents,
    pub connect_ack: EventHandle,
    pub connect_req: EventHandle,
    pub disconnect: EventHandle,
}

/// Raw handles событий для передачи в kernel driver
/// 
/// Handles представлены как `isize` для совместимости с Windows HANDLE типом.
/// Эти handles можно передать в драйвер через IOCTL для event-driven IPC.
#[derive(Debug, Clone, Copy)]
pub struct EventHandles {
    /// Server→Client data event handle (s2c.data)
    /// User-mode сигнализирует когда данные доступны для чтения драйвером
    pub s2c_data: isize,
    
    /// Client→Server data event handle (c2s.data)
    /// Driver сигнализирует когда данные доступны для чтения user-mode
    pub c2s_data: isize,
}

impl SharedEvents {
    pub fn create(base: &str) -> Result<Self> {
        Ok(Self {
            s2c: ChannelEvents {
                data: EventHandle::create(&event_name(
                    base,
                    Direction::ServerToClient,
                    EVENT_DATA_SUFFIX,
                ))?,
                space: EventHandle::create(&event_name(
                    base,
                    Direction::ServerToClient,
                    EVENT_SPACE_SUFFIX,
                ))?,
            },
            c2s: ChannelEvents {
                data: EventHandle::create(&event_name(
                    base,
                    Direction::ClientToServer,
                    EVENT_DATA_SUFFIX,
                ))?,
                space: EventHandle::create(&event_name(
                    base,
                    Direction::ClientToServer,
                    EVENT_SPACE_SUFFIX,
                ))?,
            },
            connect_ack: EventHandle::create(&event_name(
                base,
                Direction::ServerToClient,
                EVENT_CONNECT_SUFFIX,
            ))?,
            connect_req: EventHandle::create(&event_name(
                base,
                Direction::ClientToServer,
                EVENT_CONNECT_REQ_SUFFIX,
            ))?,
            disconnect: EventHandle::create(&event_name(
                base,
                Direction::ServerToClient,
                EVENT_DISCONNECT_SUFFIX,
            ))?,
        })
    }

    /// Получить raw handles событий для передачи в kernel driver
    /// 
    /// Возвращает структуру с raw handles (isize) для:
    /// - s2c_data: Server→Client data event (user signals when data available for driver)
    /// - c2s_data: Client→Server data event (driver signals when data available for user)
    /// 
    /// Эти handles можно передать в драйвер через IOCTL для event-driven IPC.
    pub fn get_event_handles(&self) -> EventHandles {
        EventHandles {
            s2c_data: self.s2c.data.raw_handle(),
            c2s_data: self.c2s.data.raw_handle(),
        }
    }

    pub fn open(base: &str) -> Result<Self> {
        Ok(Self {
            s2c: ChannelEvents {
                data: EventHandle::open(&event_name(
                    base,
                    Direction::ServerToClient,
                    EVENT_DATA_SUFFIX,
                ))?,
                space: EventHandle::open(&event_name(
                    base,
                    Direction::ServerToClient,
                    EVENT_SPACE_SUFFIX,
                ))?,
            },
            c2s: ChannelEvents {
                data: EventHandle::open(&event_name(
                    base,
                    Direction::ClientToServer,
                    EVENT_DATA_SUFFIX,
                ))?,
                space: EventHandle::open(&event_name(
                    base,
                    Direction::ClientToServer,
                    EVENT_SPACE_SUFFIX,
                ))?,
            },
            connect_ack: EventHandle::open(&event_name(
                base,
                Direction::ServerToClient,
                EVENT_CONNECT_SUFFIX,
            ))?,
            connect_req: EventHandle::open(&event_name(
                base,
                Direction::ClientToServer,
                EVENT_CONNECT_REQ_SUFFIX,
            ))?,
            disconnect: EventHandle::open(&event_name(
                base,
                Direction::ServerToClient,
                EVENT_DISCONNECT_SUFFIX,
            ))?,
        })
    }
}
