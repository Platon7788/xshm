//! Маяк сервера: событийное обнаружение без опроса.
//!
//! Клиенту, который ждёт появления сервера (профайлер в приложении ждёт
//! viewer), незачем раз в полсекунды пробовать подключиться: [`Beacon`] —
//! именованное событие-уведомление с ручным сбросом `<имя>_beacon`.
//! Сервер при старте его взводит ([`Beacon::raise`]), при штатной остановке
//! — сбрасывает ([`Beacon::lower`]); клиенты спят в [`Beacon::wait`] и
//! просыпаются все сразу, когда сервер появился. Событие живёт, пока открыт
//! хоть один дескриптор, поэтому порядок запуска любой.
//!
//! Сервер, упавший не сбросив маяк, оставляет его взведённым: клиент,
//! которому при взведённом маяке не удалось подключиться, должен отступить
//! (таймаут `wait` или своя пауза), а не крутиться в цикле.

use std::time::Duration;

use crate::error::Result;
use crate::naming::has_explicit_namespace;
use crate::win::EventHandle;

/// Маяк сервера (см. модуль).
#[derive(Debug)]
pub struct Beacon {
    event: EventHandle,
}

/// Имя события маяка для базового имени канала/лобби.
fn beacon_name(base: &str) -> String {
    if has_explicit_namespace(base) {
        format!("{base}_beacon")
    } else {
        format!("Local\\{base}_beacon")
    }
}

impl Beacon {
    /// Открыть маяк `name` (или создать, если его ещё нет) — и серверу, и
    /// клиентам.
    ///
    /// # Errors
    ///
    /// Ошибка ОС (имя недопустимо, нет прав на `Global\`).
    pub fn open(name: &str) -> Result<Self> {
        Ok(Self {
            event: EventHandle::open_or_create_notification(&beacon_name(name))?,
        })
    }

    /// Сервер работает: разбудить всех ждущих и оставить маяк взведённым.
    ///
    /// # Errors
    ///
    /// Ошибка ОС.
    pub fn raise(&self) -> Result<()> {
        self.event.set()
    }

    /// Сервер уходит: сбросить маяк.
    ///
    /// # Errors
    ///
    /// Ошибка ОС.
    pub fn lower(&self) -> Result<()> {
        self.event.reset()
    }

    /// Ждать, пока маяк не взведён (`None` — без таймаута). `true` —
    /// взведён.
    ///
    /// # Errors
    ///
    /// Ошибка ОС.
    pub fn wait(&self, timeout: Option<Duration>) -> Result<bool> {
        self.event.wait(timeout)
    }

    /// Ждать, пока взведётся любой из маяков (`None` — без таймаута).
    /// `Some(i)` — индекс взведённого, `None` — таймаут. Так клиент ждёт
    /// сразу «сервер появился» и свой сигнал остановки — без опроса.
    ///
    /// # Errors
    ///
    /// Ошибка ОС.
    pub fn wait_any(beacons: &[&Self], timeout: Option<Duration>) -> Result<Option<usize>> {
        let handles: Vec<isize> = beacons.iter().map(|b| b.event.raw_handle()).collect();
        crate::win::wait_any(&handles, timeout)
    }

    /// Взведён ли сейчас (без ожидания).
    ///
    /// # Errors
    ///
    /// Ошибка ОС.
    pub fn is_raised(&self) -> Result<bool> {
        self.event.wait(Some(Duration::ZERO))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    fn unique(tag: &str) -> String {
        format!(
            "xshm-beacon-{tag}-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        )
    }

    #[test]
    fn waiters_wake_together_on_raise_and_lower_resets() {
        let name = unique("wake");
        let server = Beacon::open(&name).unwrap();
        assert!(!server.is_raised().unwrap(), "новый маяк не взведён");
        let woke = Arc::new(AtomicUsize::new(0));
        let waiters: Vec<_> = (0..3)
            .map(|_| {
                let name = name.clone();
                let woke = Arc::clone(&woke);
                std::thread::spawn(move || {
                    let b = Beacon::open(&name).unwrap();
                    if b.wait(Some(Duration::from_secs(5))).unwrap() {
                        woke.fetch_add(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(woke.load(Ordering::SeqCst), 0, "до взвода все спят");
        server.raise().unwrap();
        for w in waiters {
            w.join().unwrap();
        }
        assert_eq!(woke.load(Ordering::SeqCst), 3, "взвод будит всех");
        assert!(server.is_raised().unwrap(), "остаётся взведённым");
        server.lower().unwrap();
        assert!(!server.is_raised().unwrap());
        assert!(!server.wait(Some(Duration::from_millis(20))).unwrap());
    }

    #[test]
    fn wait_any_reports_which_beacon_fired() {
        let (a, b) = (
            Beacon::open(&unique("any-a")).unwrap(),
            Beacon::open(&unique("any-b")).unwrap(),
        );
        assert_eq!(
            Beacon::wait_any(&[&a, &b], Some(Duration::from_millis(10))).unwrap(),
            None
        );
        b.raise().unwrap();
        assert_eq!(
            Beacon::wait_any(&[&a, &b], Some(Duration::from_secs(1))).unwrap(),
            Some(1)
        );
    }

    #[test]
    fn client_may_open_before_server() {
        let name = unique("order");
        let client = Beacon::open(&name).unwrap();
        let server = Beacon::open(&name).unwrap();
        server.raise().unwrap();
        assert!(client.is_raised().unwrap(), "одно и то же событие");
    }
}
