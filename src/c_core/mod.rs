//! Безопасный интерфейс единственного C-ядра xipc.
//!
//! Временный модуль миграции; после переноса режимов заменит старое ядро.
//! Соединение имеет одного владельца, операции требуют `&mut self`.
//! Приём копирует данные в буфер вызывающего: ссылок на изменяемую пиром
//! разделяемую память этот API не предоставляет.
mod cancel;
mod client_queue;
mod client_worker;
mod connection;
mod dispatch;
mod error;
mod multi;
mod options;
mod resources;
mod wait;
mod worker;

pub use cancel::CancellationToken;
pub use client_queue::ClientSender;
pub use client_worker::{ClientEvent, ClientWorker};
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
pub use options::{ClientConnectOptions, Readiness};
pub use options::{DispatchClientOptions, DispatchOptions};
pub use resources::{Beacon, PrivateNamespace};
pub use wait::{ProcessExit, wait_any};
pub use worker::{CommandCompletion, DispatchSender, DispatchWorker, QueueLimits};
