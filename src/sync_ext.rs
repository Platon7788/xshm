//! Единая политика отравления блокировок библиотеки.
//!
//! Отравление (`PoisonError`) значит лишь «какой-то поток запаниковал,
//! держа блокировку». Данные под блокировками xshm (очереди команд, карты
//! клиентов, флаги) при этом остаются согласованными: каждая правка -- одно
//! присваивание или вставка/удаление. Поэтому отравление снимается, а не
//! пробрасывается дальше паникой: `lock().unwrap()` в `stop()`/`Drop` после
//! паники в колбэке пользователя превратил бы одну панику в каскад (паника в
//! `Drop` во время раскрутки -- `abort`). На `panic = "abort"` из манифеста
//! xshm полагаться нельзя: стратегию паники выбирает конечный бинарь
//! потребителя.

use std::any::Any;
use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::thread::JoinHandle;

use crate::error::{Result, ShmError};

/// Захват `Mutex` без паники на отравлении.
pub(crate) trait LockExt<T: ?Sized> {
    /// `lock()`, снимающий отравление (`PoisonError::into_inner`).
    fn lock_clean(&self) -> MutexGuard<'_, T>;
}

impl<T: ?Sized> LockExt<T> for Mutex<T> {
    fn lock_clean(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Захват `RwLock` без паники на отравлении.
pub(crate) trait RwLockExt<T: ?Sized> {
    /// `read()`, снимающий отравление.
    fn read_clean(&self) -> RwLockReadGuard<'_, T>;
    /// `write()`, снимающий отравление.
    fn write_clean(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T: ?Sized> RwLockExt<T> for RwLock<T> {
    fn read_clean(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_clean(&self) -> RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Payload паники -> `ShmError::WorkerPanicked`.
fn panic_error(payload: &(dyn Any + Send)) -> ShmError {
    let message = payload
        .downcast_ref::<&'static str>()
        .copied()
        .unwrap_or("non-literal panic message (see panic hook output)");
    ShmError::WorkerPanicked { message }
}

/// Дождаться worker-поток. Паника потока -- `Err(WorkerPanicked)`, а не
/// молчаливое `let _ = handle.join()`: вызывающий отдаёт её в `on_error`
/// обработчика (или явно объясняет, почему некуда).
pub(crate) fn join_worker(handle: JoinHandle<()>) -> Result<()> {
    handle.join().map_err(|payload| panic_error(&*payload))
}

#[cfg(test)]
mod tests {
    use super::{LockExt, RwLockExt};
    use std::sync::{Arc, Mutex, RwLock};

    /// Паника под `lock_clean` отравляет мьютекс, но следующий
    /// `lock_clean` всё равно отдаёт данные (а не паникует сам).
    #[test]
    fn lock_clean_recovers_from_poison() {
        let shared = Arc::new(Mutex::new(41_u32));
        let worker = Arc::clone(&shared);
        let joined = std::thread::spawn(move || {
            let mut guard = worker.lock_clean();
            *guard += 1;
            panic!("паника под блокировкой (ожидаемая в тесте)");
        })
        .join();
        assert!(joined.is_err(), "поток должен был запаниковать");
        assert!(shared.is_poisoned());
        assert_eq!(*shared.lock_clean(), 42);

        let rw = Arc::new(RwLock::new(String::from("a")));
        let writer = Arc::clone(&rw);
        let joined = std::thread::spawn(move || {
            let mut guard = writer.write_clean();
            guard.push('b');
            panic!("паника под блокировкой записи (ожидаемая в тесте)");
        })
        .join();
        assert!(joined.is_err());
        assert!(rw.is_poisoned());
        assert_eq!(rw.read_clean().as_str(), "ab");
        rw.write_clean().push('c');
        assert_eq!(rw.read_clean().as_str(), "abc");
    }

    /// Паника worker-а не теряется в `let _ = handle.join()`: литерал
    /// паники доходит до `ShmError::WorkerPanicked`, штатный выход -- `Ok`.
    #[test]
    fn join_worker_reports_panic_payload() {
        let panicked = std::thread::spawn(|| panic!("колбэк упал (ожидаемо в тесте)"));
        assert_eq!(
            super::join_worker(panicked),
            Err(crate::ShmError::WorkerPanicked {
                message: "колбэк упал (ожидаемо в тесте)"
            })
        );
        let formatted = std::thread::spawn(|| panic!("код {}", 42));
        assert!(matches!(
            super::join_worker(formatted),
            Err(crate::ShmError::WorkerPanicked { .. })
        ));
        assert_eq!(super::join_worker(std::thread::spawn(|| {})), Ok(()));
    }
}
