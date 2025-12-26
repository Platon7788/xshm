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
