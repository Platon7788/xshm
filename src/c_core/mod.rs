//! Безопасный интерфейс единственного C-ядра xipc.
//!
//! Временный модуль миграции; после переноса режимов заменит старое ядро.
//! Соединение имеет одного владельца, операции требуют `&mut self`.
//! Приём копирует данные в буфер вызывающего: ссылок на изменяемую пиром
//! разделяемую память этот API не предоставляет.
mod cancel;
mod connection;
mod error;
mod options;

pub use cancel::CancellationToken;
pub use connection::{Client, Server, Session};
pub use error::{DisconnectReason, Error, Result};
pub use options::{ChannelConfig, Layout, RingConfig, Security, Wait};
