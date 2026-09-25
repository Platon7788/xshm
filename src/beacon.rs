//! Маяк сервера: событийное обнаружение без опроса.
//!
//! Клиенту, который ждёт появления сервера (профайлер в приложении ждёт
//! viewer), незачем раз в полсекунды пробовать подключиться: [`Beacon`] —
//! именованное событие-уведомление с ручным сбросом `<имя>_beacon`.
//! Сервер при старте его взводит ([`Beacon::raise`]), при штатной остановке
//! — сбрасывает ([`Beacon::lower`]); клиенты спят в [`Beacon::wait`] /
//! [`Beacon::wait_any`] и просыпаются все сразу, когда сервер появился.
//! Событие живёт, пока открыт хоть один дескриптор, поэтому порядок запуска
//! любой.
//!
//! **Порядок сервера**: взводить маяк строго ПОСЛЕ того, как сервер готов
//! принимать (лобби создано), сбрасывать — ДО остановки. Тогда клиент,
//! проснувшийся по маяку, застаёт сервер, а «маяк взведён, сервера нет»
//! бывает только после падения сервера.
//!
//! **Сервер упал, не сбросив маяк**: маяк остаётся взведённым. Клиент,
//! которому при взведённом маяке не удалось подключиться, потому что
//! сервера нет, сам сбрасывает маяк ([`Beacon::lower`]) и ПОВТОРНО проверяет
//! сервер: если сервер тем временем поднялся, то взвёл маяк уже после
//! создания лобби — клиент либо найдёт лобби при повторной проверке, либо
//! проснётся по новому взводу. Без опроса и без потерянных пробуждений.
//!
//! Кроме именованного маяка:
//! - [`Beacon::unnamed`] — безымянное событие того же вида для сигналов
//!   внутри процесса (например, «остановись» рядом с маяком в
//!   [`Beacon::wait_any`]); по имени его не открыть и не занять заранее;
//! - [`ProcessExit`] — удерживаемый handle процесса (сигнален, когда процесс
//!   завершился); вместе с маяками ждётся через [`wait_any`].

use std::time::Duration;

use crate::error::Result;
use crate::naming::has_explicit_namespace;
use crate::win::{EventHandle, ProcessWatch};

mod sealed {
    pub trait Sealed {}
}

/// Объект, который можно ждать в [`wait_any`]: [`Beacon`], [`ProcessExit`].
/// Запечатан — реализации только внутри `xshm`.
pub trait Waitable: sealed::Sealed {
    /// Числовой handle для `NtWaitForMultipleObjects` (деталь реализации).
    #[doc(hidden)]
    fn wait_handle(&self) -> isize;
}

/// Ждать, пока сработает любой из объектов (`None` — без таймаута).
/// `Some(i)` — индекс сработавшего (при одновременном срабатывании —
/// наименьший), `None` — таймаут или пустой список.
///
/// # Errors
///
/// Ошибка ОС.
pub fn wait_any(objects: &[&dyn Waitable], timeout: Option<Duration>) -> Result<Option<usize>> {
    let handles: Vec<isize> = objects.iter().map(|o| o.wait_handle()).collect();
    crate::win::wait_any(&handles, timeout)
}

/// Маяк сервера (см. модуль).
#[derive(Debug)]
pub struct Beacon {
    event: EventHandle,
}

impl sealed::Sealed for Beacon {}

impl Waitable for Beacon {
    fn wait_handle(&self) -> isize {
        self.event.raw_handle()
    }
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
    /// Ошибка ОС (имя недопустимо, нет прав на `Global\`). 0.9: имя уже
    /// занято событием другого типа (автосброс вместо уведомления --
    /// `WindowsError { code: STATUS_OBJECT_TYPE_MISMATCH }`) или объектом с
    /// ограничивающим DACL (`EVENT_ALL_ACCESS` не выдан --
    /// `STATUS_ACCESS_DENIED`): подменённый маяк не открывается.
    pub fn open(name: &str) -> Result<Self> {
        Ok(Self {
            event: EventHandle::open_or_create_notification(&beacon_name(name))?,
        })
    }

    /// Безымянный маяк: то же событие-уведомление с ручным сбросом, но без
    /// имени — виден только через этот объект (внутри процесса). Нужен как
    /// сигнал «остановись» в одном [`Beacon::wait_any`] с именованным
    /// маяком: предсказуемое имя позволило бы чужому процессу открыть
    /// событие и остановить (или заранее занять) его.
    ///
    /// # Errors
    ///
    /// Ошибка ОС.
    pub fn unnamed() -> Result<Self> {
        Ok(Self {
            event: EventHandle::create_unnamed(true)?,
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
    /// `Some(i)` — индекс взведённого (при нескольких — наименьший), `None`
    /// — таймаут. Так клиент ждёт сразу «сервер появился» и свой сигнал
    /// остановки ([`Beacon::unnamed`]) — без опроса. Смешанный набор с
    /// [`ProcessExit`] — через [`wait_any`].
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

/// Наблюдение за завершением процесса: удерживаемый handle
/// (`SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION`), сигнален, когда
/// процесс завершился. Пока handle открыт, переиспользование PID не путает
/// наблюдателя. Ждётся вместе с маяками через [`wait_any`] — например,
/// клиент, отвергнутый сервером, спит до смерти этого сервера или до его
/// штатного сигнала, без опроса.
#[derive(Debug)]
pub struct ProcessExit {
    watch: ProcessWatch,
}

impl sealed::Sealed for ProcessExit {}

impl Waitable for ProcessExit {
    fn wait_handle(&self) -> isize {
        self.watch.raw_handle()
    }
}

impl ProcessExit {
    /// Открыть процесс `pid`. `None` — PID 0, процесса уже нет или не
    /// хватает прав (служба в другой сессии).
    #[must_use]
    pub fn open(pid: u32) -> Option<Self> {
        ProcessWatch::open(pid).map(|watch| Self { watch })
    }

    /// PID наблюдаемого процесса.
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.watch.pid()
    }

    /// Процесс подтверждённо завершился (без ожидания).
    #[must_use]
    pub fn has_exited(&self) -> bool {
        self.watch.has_exited()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

    /// Уникальное имя в пределах прогона: PID + счётчик (раньше —
    /// `Instant::now().elapsed()`, то есть почти всегда 0: имена тестов
    /// совпадали и маяки делились между ними).
    fn unique(tag: &str) -> String {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        format!(
            "xshm-beacon-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    #[test]
    fn unique_names_differ() {
        assert_ne!(unique("x"), unique("x"));
    }

    /// 0.9: чужой процесс заранее создал `<имя>_beacon` автосбросным
    /// событием -- `open` отвергает подмену (NtQueryEvent), а не открывает
    /// сломанный маяк.
    #[test]
    fn open_rejects_squatted_auto_reset_event() {
        let name = unique("squat");
        let _squatter = EventHandle::create(&beacon_name(&name)).unwrap();
        assert!(
            Beacon::open(&name).is_err(),
            "маяк с автосбросом должен отвергаться"
        );
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

    /// Безымянные маяки независимы (у каждого своё событие) и работают в
    /// `wait_any` рядом с именованным.
    #[test]
    fn unnamed_beacons_are_private_and_waitable() {
        let named = Beacon::open(&unique("mix")).unwrap();
        let (s1, s2) = (Beacon::unnamed().unwrap(), Beacon::unnamed().unwrap());
        s1.raise().unwrap();
        assert!(s1.is_raised().unwrap());
        assert!(
            !s2.is_raised().unwrap(),
            "у каждого безымянного — своё событие"
        );
        assert_eq!(
            Beacon::wait_any(&[&named, &s2, &s1], Some(Duration::from_secs(1))).unwrap(),
            Some(2)
        );
        s1.lower().unwrap();
        assert!(!s1.is_raised().unwrap());
    }

    /// `ProcessExit` живого процесса не сигнален; `wait_any` смешивает его с
    /// маяками.
    #[test]
    fn process_exit_of_live_process_does_not_fire() {
        let me = ProcessExit::open(std::process::id()).expect("свой процесс");
        assert_eq!(me.pid(), std::process::id());
        assert!(!me.has_exited());
        assert!(ProcessExit::open(0).is_none());
        let stop = Beacon::unnamed().unwrap();
        assert_eq!(
            wait_any(&[&me, &stop], Some(Duration::from_millis(20))).unwrap(),
            None
        );
        stop.raise().unwrap();
        assert_eq!(
            wait_any(&[&me, &stop], Some(Duration::from_secs(1))).unwrap(),
            Some(1)
        );
    }

    /// Завершение процесса будит `wait_any` без опроса. `Child` держит свой
    /// handle процесса — объект не исчезнет, даже если процесс выйдет раньше
    /// `open`.
    #[test]
    fn process_exit_fires_when_process_ends() {
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "exit", "0"])
            .spawn()
            .expect("cmd");
        let exit = ProcessExit::open(child.id()).expect("handle процесса");
        let stop = Beacon::unnamed().unwrap();
        assert_eq!(
            wait_any(&[&stop, &exit], Some(Duration::from_secs(10))).unwrap(),
            Some(1)
        );
        assert!(exit.has_exited());
        let _ = child.wait();
    }
}
