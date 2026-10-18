//! Безопасный Rust API над единственным C-ядром xipc из xhook/ipc.
//!
//! Каналы, рукопожатие, разделяемая память и NT-ожидания реализованы в C.
//! Rust управляет владением, временем жизни, отменой и ограниченными очередями.
//! Приём копирует данные: ссылок на изменяемую пиром память API не выдаёт.
mod thread_hook;
mod transport;
pub use thread_hook::{set_thread_start_hook, thread_start_hook};
pub use transport::*;
