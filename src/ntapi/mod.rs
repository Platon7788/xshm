//! NT API модуль - прямые вызовы ntdll.dll
//!
//! Минимальная реализация без внешних зависимостей.
//! Статическая линковка с ntdll.dll через #[link(name = "ntdll")].
//!
//! # Особенности
//!
//! - Никакого TLS
//! - Никакого GetProcAddress в runtime
//! - Линкер резолвит адреса при загрузке модуля
//! - Минимальный overhead

pub mod funcs;
pub mod helpers;
pub mod types;

// Реэкспорт для удобства
pub use funcs::*;
pub use helpers::*;
pub use types::*;
