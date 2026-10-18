//! Одноразовое событие отмены. Сигнал нельзя сбросить и потерять остановку.
use super::{Error, Result};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;

#[link(name = "kernel32")]
// SAFETY: стандартный Windows ABI, BOOL — i32, HANDLE — указатель.
unsafe extern "system" {
    fn CreateEventW(
        attrs: *const c_void,
        manual: i32,
        initial: i32,
        name: *const u16,
    ) -> *mut c_void;
    fn SetEvent(event: *mut c_void) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

#[derive(Debug)]
struct Event(NonNull<c_void>);
// SAFETY: HANDLE события можно сигналить из любого потока. Arc сохраняет
// время жизни; ни один вызов не закрывает и не сбрасывает чужой HANDLE.
unsafe impl Send for Event {}
// SAFETY: доступ через &Event только сигналит событие; закрытие эксклюзивно в Drop.
unsafe impl Sync for Event {}
impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: последний Arc; все заимствования и вызовы завершились.
        unsafe { CloseHandle(self.0.as_ptr()) };
    }
}

/// Разделяемая одноразовая отмена. Клонирование сохраняет событие живым.
#[derive(Debug, Clone)]
pub struct CancellationToken(Arc<Event>);
impl CancellationToken {
    /// Создать независимый, ещё не сработавший токен.
    pub fn new() -> Result<Self> {
        // SAFETY: NULL attrs/name допустимы; создаём manual-reset unnamed event.
        let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        let event = NonNull::new(handle).ok_or_else(std::io::Error::last_os_error)?;
        Ok(Self(Arc::new(Event(event))))
    }
    /// Необратимо отменить операции, использующие этот токен.
    pub fn cancel(&self) -> Result<()> {
        // SAFETY: заимствование self удерживает Arc и валидный HANDLE.
        if unsafe { SetEvent(self.0.0.as_ptr()) } == 0 {
            return Err(Error::Os(std::io::Error::last_os_error()));
        }
        Ok(())
    }
    pub(super) fn handle(token: Option<&Self>) -> *mut c_void {
        token.map_or(std::ptr::null_mut(), |t| t.0.0.as_ptr())
    }
}
