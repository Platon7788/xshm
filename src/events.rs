use crate::constants::{
    EVENT_CONNECT_REQ_SUFFIX, EVENT_CONNECT_SUFFIX, EVENT_DATA_SUFFIX, EVENT_DISCONNECT_SUFFIX,
    EVENT_SPACE_SUFFIX,
};
use crate::error::Result;
use crate::namespace::PrivateNamespace;
use crate::naming::{Direction, ObjectPath};
use crate::security::SecurityAttributes;
use crate::win::EventHandle;

#[derive(Debug)]
pub struct ChannelEvents {
    pub data: EventHandle,
    pub space: EventHandle,
}

#[derive(Debug)]
pub struct SharedEvents {
    pub s2c: ChannelEvents,
    pub c2s: ChannelEvents,
    pub connect_ack: EventHandle,
    pub connect_req: EventHandle,
    pub disconnect: EventHandle,
}

/// Сырые handle-ы всех 7 событий канала -- для передачи в kernel driver
/// (anonymous-канал, INTEROP §b.5).
///
/// Handle-ы -- `isize` (Windows `HANDLE`) в таблице процесса сервера и
/// принадлежат серверу: живут, пока жив `SharedServer`, закрываются его
/// `Drop`. Драйвер, получив их IOCTL-ом в контексте процесса сервера, берёт
/// свои ссылки на объекты (`ObReferenceObjectByHandle`) и управляет их
/// временем жизни сам. Все события -- SynchronizationEvent (автосброс):
/// не `KeResetEvent`/`KePulseEvent`; сигнал -- `KeSetEvent`, кто и когда
/// сигналит -- §e.2 INTEROP.
///
/// `#[non_exhaustive]`: значения выдаёт только сервер
/// (`SharedServer::get_event_handles`), поля читаются по имени.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct EventHandles {
    /// `S2C_DATA`: сервер-писатель взводит по правилу §e.2 п. 1 (кадр лёг
    /// в пустое по позициям кольцо); ждёт драйвер-читатель.
    pub s2c_data: isize,
    /// `C2S_DATA`: драйвер-писатель взводит по тому же правилу; ждёт
    /// сервер (`poll_client`, `recv_async`).
    pub c2s_data: isize,
    /// `S2C_SPACE`: драйвер-читатель взводит, сняв заявку `space_waiter`
    /// или опустошив кольцо сервер -> драйвер; ждёт сервер-писатель
    /// (`wait_for_space`, `send_async`).
    pub s2c_space: isize,
    /// `C2S_SPACE`: сервер-читатель взводит по тому же правилу; ждёт
    /// драйвер-писатель.
    pub c2s_space: isize,
    /// `S2C_CONNECT`: сервер взводит после фиксации рукопожатия (S7); ждёт
    /// драйвер после своей заявки.
    pub connect_ack: isize,
    /// `C2S_CONNECT_REQ`: драйвер взводит после `CLIENT_HELLO`; ждёт сервер
    /// (`wait_for_client`, `accept_async`).
    pub connect_req: isize,
    /// `S2C_DISCONNECT`: взводит уходящая сторона (сначала `IDLE`, потом
    /// сигнал -- R14); ждут обе.
    pub disconnect: isize,
}

impl SharedEvents {
    /// Создать 7 событий канала `base` с дескриптором `security` (0.10) --
    /// в частном пространстве `namespace` (относительные имена) или, если
    /// `None`, в namespace имени канала.
    pub(crate) fn create(
        namespace: Option<&PrivateNamespace>,
        base: &str,
        security: &SecurityAttributes,
    ) -> Result<Self> {
        let create = |direction, suffix| {
            EventHandle::create(
                ObjectPath::event(namespace, base, direction, suffix),
                security,
            )
        };
        Ok(Self {
            s2c: ChannelEvents {
                data: create(Direction::ServerToClient, EVENT_DATA_SUFFIX)?,
                space: create(Direction::ServerToClient, EVENT_SPACE_SUFFIX)?,
            },
            c2s: ChannelEvents {
                data: create(Direction::ClientToServer, EVENT_DATA_SUFFIX)?,
                space: create(Direction::ClientToServer, EVENT_SPACE_SUFFIX)?,
            },
            connect_ack: create(Direction::ServerToClient, EVENT_CONNECT_SUFFIX)?,
            connect_req: create(Direction::ClientToServer, EVENT_CONNECT_REQ_SUFFIX)?,
            disconnect: create(Direction::ServerToClient, EVENT_DISCONNECT_SUFFIX)?,
        })
    }

    /// 7 безымянных событий anonymous-канала (Task 13): SynchronizationEvent
    /// -- тот же вид, что у именованных событий канала, -- с дескриптором
    /// `security` (тем же, что у anonymous-секции). По имени их не открыть:
    /// пир (драйвер) получает handle-ы через `get_event_handles`.
    pub(crate) fn create_unnamed(security: &SecurityAttributes) -> Result<Self> {
        let create = || EventHandle::create_unnamed_secured(security);
        Ok(Self {
            s2c: ChannelEvents {
                data: create()?,
                space: create()?,
            },
            c2s: ChannelEvents {
                data: create()?,
                space: create()?,
            },
            connect_ack: create()?,
            connect_req: create()?,
            disconnect: create()?,
        })
    }

    /// Сырые handle-ы всех 7 событий -- для передачи в kernel driver (см.
    /// `EventHandles`). Handle-ы остаются собственностью `self`.
    #[must_use]
    pub fn get_event_handles(&self) -> EventHandles {
        EventHandles {
            s2c_data: self.s2c.data.raw_handle(),
            c2s_data: self.c2s.data.raw_handle(),
            s2c_space: self.s2c.space.raw_handle(),
            c2s_space: self.c2s.space.raw_handle(),
            connect_ack: self.connect_ack.raw_handle(),
            connect_req: self.connect_req.raw_handle(),
            disconnect: self.disconnect.raw_handle(),
        }
    }

    /// Открыть 7 событий канала `base` (в namespace имени канала).
    pub fn open(base: &str) -> Result<Self> {
        Self::open_in(None, base)
    }

    /// Открыть 7 событий канала `base` в частном пространстве `namespace`
    /// (`None` -- как [`SharedEvents::open`]). Все -- автосбросные
    /// (`open_auto_reset`).
    pub(crate) fn open_in(namespace: Option<&PrivateNamespace>, base: &str) -> Result<Self> {
        let open = |direction, suffix| {
            EventHandle::open_auto_reset(ObjectPath::event(namespace, base, direction, suffix))
        };
        Ok(Self {
            s2c: ChannelEvents {
                data: open(Direction::ServerToClient, EVENT_DATA_SUFFIX)?,
                space: open(Direction::ServerToClient, EVENT_SPACE_SUFFIX)?,
            },
            c2s: ChannelEvents {
                data: open(Direction::ClientToServer, EVENT_DATA_SUFFIX)?,
                space: open(Direction::ClientToServer, EVENT_SPACE_SUFFIX)?,
            },
            connect_ack: open(Direction::ServerToClient, EVENT_CONNECT_SUFFIX)?,
            connect_req: open(Direction::ClientToServer, EVENT_CONNECT_REQ_SUFFIX)?,
            disconnect: open(Direction::ServerToClient, EVENT_DISCONNECT_SUFFIX)?,
        })
    }
}
