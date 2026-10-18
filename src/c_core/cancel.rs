//! Одноразовая отмена через безымянный маяк C-ядра, без отдельного NT-слоя Rust.
use super::{Beacon, Result};
use std::ffi::c_void;

/// Разделяемая одноразовая отмена. Событие нельзя сбросить и потерять остановку.
#[derive(Debug, Clone)]
pub struct CancellationToken(Beacon);
impl CancellationToken {
    /// Создать независимый, ещё не сработавший токен.
    pub fn new() -> Result<Self> {
        Ok(Self(Beacon::unnamed()?))
    }
    /// Необратимо отменить операции, использующие этот токен.
    pub fn cancel(&self) -> Result<()> {
        self.0.raise()
    }
    pub(super) fn handle(token: Option<&Self>) -> *mut c_void {
        token.map_or(std::ptr::null_mut(), |t| t.0.handle())
    }
}
