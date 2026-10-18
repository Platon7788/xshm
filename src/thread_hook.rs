//! Хук старта рабочих потоков xshm (Rust API — не часть
//! протокола и не видим пиру).
//!
//! DispatchWorker и ClientWorker наследуют хук создающего потока.
//! Политика хранится в TLS: объекты других потоков программы не затрагиваются.
//!
//! Назначение — служебная разметка потоков хостом: например, профайлер
//! (`prof-shm`) делает свои транспортные потоки «тихими» для учёта памяти
//! (`prof_core::mem::quiet_thread`), не трогая xshm-потоки приложения.

use std::cell::Cell;
use std::io;
use std::thread::{Builder, JoinHandle};

thread_local! {
    /// Хук, который получат рабочие потоки, созданные этим потоком.
    static HOOK: Cell<Option<fn()>> = const { Cell::new(None) };
}

/// Поставить (или снять — `None`) хук старта для рабочих потоков xshm,
/// которые создаст **текущий** поток (и, по наследству, потоки, которые
/// создадут они). Хук зовётся первой строкой нового потока, до любой
/// работы библиотеки; уже запущенные потоки не затрагиваются.
pub fn set_thread_start_hook(hook: Option<fn()>) {
    let _ = HOOK.try_with(|h| h.set(hook));
}

/// Хук старта, действующий на текущем потоке ([`set_thread_start_hook`]
/// или унаследованный рабочим потоком xshm).
#[must_use]
pub fn thread_start_hook() -> Option<fn()> {
    HOOK.try_with(Cell::get).ok().flatten()
}

/// Создать рабочий поток библиотеки: хук старта текущего потока
/// наследуется и зовётся первым.
pub(crate) fn spawn<F, T>(builder: Builder, f: F) -> io::Result<JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let hook = thread_start_hook();
    builder.spawn(move || {
        if let Some(hook) = hook {
            set_thread_start_hook(Some(hook));
            hook();
        }
        f()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static CALLS: AtomicUsize = AtomicUsize::new(0);

    fn mark() {
        CALLS.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn hook_runs_first_and_is_inherited_by_worker_spawned_workers() {
        let outer = std::thread::spawn(|| {
            set_thread_start_hook(Some(mark));
            let child = spawn(Builder::new(), || {
                assert_eq!(CALLS.load(Ordering::SeqCst), 1, "хук — первой строкой");
                // Поток, порождённый рабочим, наследует хук.
                spawn(Builder::new(), thread_start_hook)
                    .unwrap()
                    .join()
                    .unwrap()
            })
            .unwrap();
            let inherited = child.join().unwrap();
            set_thread_start_hook(None);
            inherited
        });
        let inherited = outer.join().unwrap();
        let expected: fn() = mark;
        assert_eq!(inherited.map(|f| f as usize), Some(expected as usize));
        assert_eq!(CALLS.load(Ordering::SeqCst), 2);
        // Поток без хука — рабочие без хука.
        let none = spawn(Builder::new(), thread_start_hook)
            .unwrap()
            .join()
            .unwrap();
        assert!(none.is_none());
    }
}
