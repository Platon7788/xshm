//! Безопасный интерфейс единственного C-ядра xipc.
//!
//! Временный модуль миграции; после переноса режимов заменит старое ядро.
//! Соединение имеет одного владельца, операции требуют `&mut self`.
//! Приём копирует данные в буфер вызывающего: ссылок на изменяемую пиром
//! разделяемую память этот API не предоставляет.
mod cancel;
mod connection;
mod dispatch;
mod error;
mod multi;
mod options;
mod resources;

pub use cancel::CancellationToken;
pub use connection::{Client, Server, Session};
pub use dispatch::{
    DispatchConnectionInfo, DispatchEvent, DispatchPeer, DispatchServer, DispatchWake, Registration,
};
pub use error::{DisconnectReason, Error, Result};
pub use multi::{MultiEvent, MultiPeer, MultiServer, MultiWake};
pub use options::{
    ChannelConfig, ConnectBudget, Layout, MultiClientOptions, MultiOptions, RingConfig, Security,
    Wait,
};
pub use options::{DispatchClientOptions, DispatchOptions};
pub use resources::{Beacon, PrivateNamespace};
