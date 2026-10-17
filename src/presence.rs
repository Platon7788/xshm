//! Маяк присутствия сервера (0.10, Task 11 надёжности; INTEROP R19).
//!
//! Каждый именованный сервер single-client канала (`SharedServer::start*`,
//! `AutoServer`, лобби Dispatch) и каждая база Multi держит маяк своего
//! имени -- [`Beacon`] `<имя>_beacon` (+ `<имя>_beacon_state`) в том же
//! пространстве имён и с тем же дескриптором, что секция канала. Порядок
//! сервера (R19): `raise` -- строго ПОСЛЕ публикации секции и всех 7 событий
//! (Multi -- после всех слотов), `lower` -- ПЕРВЫМ шагом `Drop`/`stop`. Тогда
//! «маяк взведён, а канала нет» бывает только после падения сервера (или
//! мгновение гонки «клиент не нашёл канал -- сервер его опубликовал и взвёл
//! маяк»). Без маяка -- выделенные каналы Dispatch (их клиент подключается
//! один раз) и слоты Multi (маяк у базы).
//!
//! Клиент вне подключения (`AutoClient`, `MultiClient`,
//! `SharedClient::wait_for_server*`) после неудачной попытки не опрашивает,
//! а спит в ядре ([`PresenceWatch::wait`]):
//! - **сервера нет** ([`Failure::Absent`]): маяк не взведён -- ждать
//!   `[wake, маяк]`; взведён -- `clear_if_stale`: сброшен (сервер мёртв) --
//!   ещё одна попытка, затем сон на маяке; не сброшен (сервер жив по
//!   состоянию маяка) -- одна повторная попытка на поколение взвода, затем
//!   `[wake, процесс сервера из состояния маяка]`;
//! - **сервер есть, подключиться нельзя** ([`Failure::Refused`]: другой
//!   формат, порча, отказ в доступе) -- `[wake, процесс сервера]` (PID и
//!   время -- из `ControlBlock` секции или из состояния маяка);
//! - ждать нечего (маяк не открылся, процесс не открылся, состояние маяка
//!   испорчено) -- [`PresenceWait::Unavailable`]: вызывающий повторяет по
//!   своей паузе (резерв, решение R-M).
//!
//! `wake` -- индекс 0 (П1 Task 10): маяк и процесс пира -- объекты, которые
//! пир может держать взведёнными, а остановку они не заслоняют.

use std::os::windows::io::{AsHandle, BorrowedHandle};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::beacon::Beacon;
use crate::error::{Result, ShmError};
use crate::events::SharedEvents;
use crate::namespace::PrivateNamespace;
use crate::naming::ObjectPath;
use crate::ntapi::types::STATUS_OBJECT_NAME_NOT_FOUND;
use crate::security::{ObjectSecurity, SecurityAttributes};
use crate::shared::SharedView;
use crate::win::{Mapping, ProcessWatch, is_announced_process_exited, is_process_gone, wait_any};

/// `NTSTATUS` `STATUS_OBJECT_PATH_NOT_FOUND` (путь в пространстве имён).
const STATUS_OBJECT_PATH_NOT_FOUND: u32 = 0xC000_003A;

/// Пауза резервного повтора `SharedClient::wait_for_server*`, когда ждать
/// нечего (маяк недоступен, решение R-M) -- как `reconnect_delay` по
/// умолчанию.
pub(crate) const PRESENCE_RETRY_DELAY: Duration = Duration::from_millis(250);

#[cfg(test)]
thread_local! {
    /// Тесты: ожидания в ядре `PresenceWatch` этого потока.
    pub(crate) static PRESENCE_KERNEL_WAITS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Держит ли сервер маяк присутствия (R19).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Presence {
    /// Именованный сервер канала, лобби, база Multi.
    On,
    /// Выделенный канал Dispatch, слот Multi.
    Off,
}

/// Маяк присутствия сервера (R19): создаётся вместе с каналом, взводится
/// после его публикации, сбрасывается первым шагом остановки.
#[derive(Debug)]
pub(crate) struct ServerPresence {
    beacon: Beacon,
}

impl ServerPresence {
    /// Открыть (или создать) маяк `name` в пространстве `root` с
    /// дескриптором канала `security`. Ошибка -- ошибка старта сервера (как
    /// занятое имя секции): например, `<имя>_beacon` заранее занят событием
    /// другого типа.
    pub(crate) fn create(
        root: Option<&Arc<PrivateNamespace>>,
        name: &str,
        security: &SecurityAttributes,
    ) -> Result<Self> {
        Ok(Self {
            beacon: Beacon::open_at(root.map(Arc::as_ref), root.cloned(), name, security)?,
        })
    }

    /// Канал опубликован: личность сервера в состояние маяка, затем взвод.
    pub(crate) fn raise(&self) -> Result<()> {
        self.beacon.raise()
    }

    /// Сервер уходит (первым шагом): личность -- в ноль, маяк сброшен.
    /// Ошибка сброса здесь некуда сообщить (`Drop`); маяк, оставшийся
    /// взведённым, клиенты гасят сами (`clear_if_stale`, PID 0 после
    /// обнуления личности).
    pub(crate) fn lower(&self) {
        let _ = self.beacon.lower();
    }
}

/// Неудачная попытка подключения вне сессии, после которой есть что ждать.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// Сервера нет: секции или событий нет (`STATUS_OBJECT_NAME_NOT_FOUND`),
    /// сегмент не опубликован (`NotReady`), Multi -- нет ни одного слота
    /// (`NotConnected`), процесс сервера сегмента завершился (`PeerDied`,
    /// секцию держит кто-то ещё).
    Absent,
    /// Сервер есть, но подключиться нельзя (`VersionMismatch`, `Corrupted`,
    /// `AccessDenied`, `InvalidConfig`); PID и время сервера из
    /// `ControlBlock` секции, если она открылась.
    Refused(Option<(u32, u64)>),
}

/// Ошибка «объекта нет» (секции или события канала).
const fn is_not_found(err: &ShmError) -> bool {
    matches!(err, ShmError::WindowsError { code, .. }
        if *code == STATUS_OBJECT_NAME_NOT_FOUND.cast_unsigned()
            || *code == STATUS_OBJECT_PATH_NOT_FOUND)
}

/// Разобрать ошибку попытки подключения к каналу `name` (single-client:
/// секция `name`; Multi передаёт `None` -- секции базы нет). `None` --
/// ждать нечего: занятый канал, таймаут ответа сервера, прочие ошибки --
/// прежняя пауза вызывающего.
pub(crate) fn classify(
    err: &ShmError,
    root: Option<&PrivateNamespace>,
    section: Option<&str>,
) -> Option<Failure> {
    match err {
        ShmError::NotReady | ShmError::PeerDied => Some(Failure::Absent),
        e if is_not_found(e) => Some(Failure::Absent),
        ShmError::VersionMismatch { .. }
        | ShmError::Corrupted
        | ShmError::AccessDenied
        | ShmError::InvalidConfig(_) => {
            let server = section.and_then(|name| section_server(root, name));
            match server {
                // Сервер сегмента подтверждённо завершился (секцию держит
                // кто-то ещё): ждать его выхода бессмысленно -- «сервера
                // нет», ждём нового на маяке.
                Some((pid, start)) if is_process_gone(pid, start) => Some(Failure::Absent),
                server => Some(Failure::Refused(server)),
            }
        }
        _ => None,
    }
}

/// PID и время сервера из заголовка секции `name` (любая версия формата).
fn section_server(root: Option<&PrivateNamespace>, name: &str) -> Option<(u32, u64)> {
    let mapping = Mapping::open_header(ObjectPath::mapping(root, name)).ok()?;
    SharedView::peek_server(&mapping)
}

/// Чем кончилось ожидание [`PresenceWatch::wait`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PresenceWait {
    /// Повторить попытку: маяк взведён, сервер упавшего маяка сброшен,
    /// процесс отвергнувшего сервера завершился.
    Retry,
    /// Сработал `wake` вызывающего (остановка или новая команда):
    /// вызывающий решает сам и, если работает дальше, зовёт `wait` снова.
    Woken,
    /// Истёк таймаут ожидания.
    TimedOut,
    /// Ждать нечего (процесс сервера не открылся, состояние маяка
    /// испорчено): резервный повтор по таймеру (решение R-M).
    Unavailable,
}

/// Маяк присутствия на стороне клиента: открывается (или создаётся --
/// клиент может стартовать раньше сервера) один раз на жизнь worker-а.
#[derive(Debug)]
pub(crate) struct PresenceWatch {
    beacon: Beacon,
    /// Поколение взвода, на котором «маяк взведён живым сервером, а канала
    /// нет» уже дало одну повторную попытку (гонка с публикацией сервера);
    /// второй раз на том же поколении -- ждать процесс сервера.
    live_retry: Option<u32>,
}

impl PresenceWatch {
    /// Открыть маяк `name` в пространстве `root` (`keep` -- ссылка, которую
    /// маяк держит; `None` -- пространство держит вызывающий). Политика
    /// `security` -- для объектов, которые создаёт этот вызов (первым
    /// пришёл клиент); существующий маяк открывается по DACL создателя
    /// (решение R-L).
    pub(crate) fn open(
        root: Option<&PrivateNamespace>,
        keep: Option<&Arc<PrivateNamespace>>,
        name: &str,
        security: &ObjectSecurity,
    ) -> Result<Self> {
        Ok(Self {
            beacon: Beacon::open_at(root, keep.cloned(), name, &security.attributes()?)?,
            live_retry: None,
        })
    }

    /// Ждать после неудачной попытки `failure` (см. модуль). `wake` --
    /// событие вызывающего (индекс 0), `timeout` -- `None` без таймаута,
    /// `wakeup` -- зовётся на каждом возврате из ожидания в ядре.
    pub(crate) fn wait(
        &mut self,
        failure: Failure,
        wake: Option<BorrowedHandle<'_>>,
        timeout: Option<Duration>,
        wakeup: &dyn Fn(),
    ) -> Result<PresenceWait> {
        match failure {
            Failure::Absent => self.wait_absent(wake, timeout, wakeup),
            Failure::Refused(Some((pid, start))) => match ProcessWatch::open_peer(pid, start) {
                Some(server) => wait_on(wake, &server, timeout, wakeup),
                // Процесс не открылся (права, служба в другой сессии) --
                // личность из состояния маяка.
                None => self.wait_identified(wake, timeout, wakeup),
            },
            Failure::Refused(None) => self.wait_identified(wake, timeout, wakeup),
        }
    }

    /// «Сервера нет».
    fn wait_absent(
        &mut self,
        wake: Option<BorrowedHandle<'_>>,
        timeout: Option<Duration>,
        wakeup: &dyn Fn(),
    ) -> Result<PresenceWait> {
        if self.beacon.is_raised()? {
            if self.beacon.clear_if_stale()? {
                // Маяк был за упавший сервер и сброшен (взвод нового сервера,
                // случившийся в этом окне, возвращён по поколению). Ещё одна
                // попытка: маяк без состояния гасит и взвод живого сервера,
                // опубликовавшего канал после нашей неудачи, -- попытка после
                // сброса его застанет (R19: публикация раньше взвода).
                return Ok(PresenceWait::Retry);
            }
            // Сервер жив по состоянию маяка, а канала нет: скорее всего
            // гонка с его публикацией -- одна повторная попытка на поколение
            // взвода. Повторно на том же поколении -- канал сервер не держит
            // (пир без R19 или ручной `raise` приложения): ждать его выхода.
            let Some(generation) = self.beacon.generation() else {
                // Состояние испорчено -- доверять нечему.
                return Ok(PresenceWait::Unavailable);
            };
            if self.live_retry != Some(generation) {
                self.live_retry = Some(generation);
                return Ok(PresenceWait::Retry);
            }
            return self.wait_identified(wake, timeout, wakeup);
        }
        wait_on(wake, &self.beacon, timeout, wakeup)
    }

    /// Ждать выхода процесса сервера из состояния маяка.
    fn wait_identified(
        &self,
        wake: Option<BorrowedHandle<'_>>,
        timeout: Option<Duration>,
        wakeup: &dyn Fn(),
    ) -> Result<PresenceWait> {
        let Some((pid, start)) = self.beacon.server_identity() else {
            return Ok(PresenceWait::Unavailable);
        };
        match ProcessWatch::open_peer(pid, start) {
            Some(server) => wait_on(wake, &server, timeout, wakeup),
            // Сервер маяка успел завершиться -- попытка, затем маяк.
            None if is_process_gone(pid, start) => Ok(PresenceWait::Retry),
            None => Ok(PresenceWait::Unavailable),
        }
    }
}

/// Одно ожидание `[wake, target]` (`wake` -- индекс 0).
fn wait_on(
    wake: Option<BorrowedHandle<'_>>,
    target: &impl AsHandle,
    timeout: Option<Duration>,
    wakeup: &dyn Fn(),
) -> Result<PresenceWait> {
    let target = target.as_handle();
    let set = [wake.unwrap_or(target), target];
    let handles = if wake.is_some() { &set[..] } else { &set[1..] };
    #[cfg(test)]
    PRESENCE_KERNEL_WAITS.with(|c| c.set(c.get() + 1));
    let fired = wait_any(handles, timeout);
    wakeup();
    Ok(match fired? {
        None => PresenceWait::TimedOut,
        Some(0) if wake.is_some() => PresenceWait::Woken,
        Some(_) => PresenceWait::Retry,
    })
}

/// Ошибки эпизода «вне подключения», о которых уже сообщено: одинаковая
/// ошибка сообщается один раз за эпизод (эпизод кончается подключением).
#[derive(Debug, Default)]
pub(crate) struct Reported(Vec<ShmError>);

impl Reported {
    /// `true` -- о такой ошибке в этом эпизоде ещё не сообщали (и теперь
    /// она учтена).
    pub(crate) fn first(&mut self, err: &ShmError) -> bool {
        if self.0.contains(err) {
            return false;
        }
        self.0.push(err.clone());
        true
    }

    /// Подключились -- эпизод окончен.
    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }
}

/// Сервер канала `name` на месте: секция опубликована и прошла проверку
/// раскладки, все 7 событий открываются, процесс сервера не завершился.
fn probe(root: Option<&PrivateNamespace>, name: &str) -> Result<()> {
    let mapping = Mapping::open(ObjectPath::mapping(root, name))?;
    // SAFETY: отображение длиной `mapping.size()` живёт, пока жив `mapping`
    // (до конца функции, дольше `view`); адрес view выровнен на гранулу
    // выделения.
    let view = unsafe { SharedView::attach(mapping.as_ptr(), mapping.size()) }?;
    let control = view.control_block();
    let server = (
        control
            .server_pid
            .load(std::sync::atomic::Ordering::Acquire),
        control.server_start_time(),
    );
    SharedEvents::open_in(root, name)?;
    if is_announced_process_exited(server.0, server.1) {
        return Err(ShmError::PeerDied);
    }
    Ok(())
}

/// `SharedClient::wait_for_server*`: ждать, пока сервер канала `name` не
/// появится (`Ok(true)`), не дольше `timeout` (`Ok(false)`).
pub(crate) fn wait_for_server(
    root: Option<&PrivateNamespace>,
    name: &str,
    timeout: Option<Duration>,
) -> Result<bool> {
    let deadline = crate::ring::deadline_of(timeout);
    let mut watch: Option<PresenceWatch> = None;
    loop {
        let err = match probe(root, name) {
            Ok(()) => return Ok(true),
            Err(err) => err,
        };
        if classify(&err, root, Some(name)) != Some(Failure::Absent) {
            return Err(err);
        }
        let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
        if left.is_some_and(|left| left.is_zero()) {
            return Ok(false);
        }
        if watch.is_none() {
            // Маяк недоступен (нет прав, нет привилегии `Global\`) -- резерв
            // ниже, маяк пробуем снова на следующем круге.
            watch = PresenceWatch::open(root, None, name, &ObjectSecurity::default()).ok();
        }
        let waited = match watch.as_mut() {
            Some(watch) => watch.wait(Failure::Absent, None, left, &|| {})?,
            None => PresenceWait::Unavailable,
        };
        match waited {
            PresenceWait::Retry | PresenceWait::Woken => {}
            PresenceWait::TimedOut => return Ok(false),
            PresenceWait::Unavailable => {
                std::thread::sleep(
                    left.map_or(PRESENCE_RETRY_DELAY, |left| left.min(PRESENCE_RETRY_DELAY)),
                );
            }
        }
    }
}
