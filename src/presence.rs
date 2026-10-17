//! Маяк присутствия сервера (0.10, Task 11 надёжности; INTEROP R19).
//!
//! Каждый именованный сервер single-client канала (`SharedServer::start*`,
//! `AutoServer`, лобби Dispatch) держит маяк своего имени -- [`Beacon`]
//! `<имя>_beacon` (+ `<имя>_beacon_state`), база Multi -- маяк
//! `<база>_multi_beacon` (своё имя: канал `X` и база Multi `X` маяк не
//! делят), в том же пространстве имён и с тем же дескриптором, что секция
//! канала. Порядок сервера (R19): `raise` -- строго ПОСЛЕ публикации секции и
//! всех 7 событий (Multi -- после всех слотов), `lower` -- ПЕРВЫМ шагом
//! `Drop`/`stop`. Тогда «маяк взведён, а канала нет» бывает только после
//! падения сервера (или мгновение гонки «клиент не нашёл канал -- сервер его
//! опубликовал и взвёл маяк»). Без маяка -- выделенные каналы Dispatch (их
//! клиент подключается один раз) и слоты Multi (маяк у базы).
//!
//! **Маяк -- лучшее усилие** (решение W(1)): объекты маяка может заранее
//! занять кто угодно с доступом к имени (другой тип, чужой DACL), а DACL
//! маяка, созданного клиентом первым, задаёт клиент (решение R-L). Отказ
//! создания или взвода НЕ отказывает старт сервера: сервер работает без
//! маяка ([`PresenceStatus::Unavailable`]), клиенты находят его резервной
//! проверкой (`discovery_fallback`, решение W(3)).
//!
//! Клиент вне подключения (`AutoClient`, `MultiClient`,
//! `SharedClient::wait_for_server*`) после неудачной попытки не опрашивает,
//! а спит в ядре ([`PresenceWatch::wait`]) -- не дольше `discovery_fallback`
//! (по умолчанию 10 с; `None` -- только события):
//! - **сервера нет** ([`Failure::Absent`]): маяк не взведён -- ждать
//!   `[wake, маяк]`; взведён -- `clear_if_stale`: сброшен (сервер мёртв) --
//!   ещё одна попытка; не сброшен (сервер жив по состоянию маяка) -- одна
//!   повторная попытка на поколение взвода, затем `[wake, процесс сервера из
//!   состояния маяка]`;
//! - **сервер есть, подключиться нельзя** ([`Failure::Refused`]: другой
//!   формат, порча, отказ в доступе) -- `[wake, процесс сервера]`; личность
//!   сервера из `ControlBlock` секции сверяется с личностью из состояния
//!   маяка (решение W(2)).
//!
//! Личность сервера -- данные, которые пишет любой с доступом к секции или
//! маяку. Поэтому (решение W(2)): личность без времени создания (`0` --
//! «любой процесс с этим PID») и собственный PID клиента не принимаются;
//! личности секции и маяка, расходящиеся между собой, -- тоже; маяк входит в
//! набор ожидания, только если он НЕ взведён (проверка прямо перед
//! ожиданием; взведённое событие ручного сброса крутило бы цикл), а новый
//! взвод поверх взведённого маяка ловит резервный таймаут. Ждать нечего
//! (маяк не открылся, процесс не открылся) -- [`PresenceWait::Unavailable`]:
//! вызывающий повторяет по своей паузе (резерв, решение R-M).
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

/// Резервная проверка обнаружения сервера по умолчанию (решение W(3)):
/// клиент вне подключения, ждущий маяк присутствия, всё равно делает
/// попытку не реже раза в это время -- защита от маяка, занятого заранее и
/// никогда не взводимого, сервера без маяка (пир без R19, отказ создания
/// маяка) и подменённой личности сервера. Только пока сервер не найден;
/// подключённый клиент не просыпается.
pub const DEFAULT_DISCOVERY_FALLBACK: Duration = Duration::from_secs(10);

/// Суффикс имени маяка базы Multi: маяк `<база>_multi_beacon`, а не
/// `<база>_beacon` (тот -- у канала с именем базы).
const MULTI_BEACON_SUFFIX: &str = "_multi";

/// Имя, по которому строится маяк базы Multi `base` (`<база>_multi` ->
/// `<база>_multi_beacon`).
pub(crate) fn multi_beacon_name(base: &str) -> String {
    format!("{base}{MULTI_BEACON_SUFFIX}")
}

/// Проверка `discovery_fallback` (m1 повторного ревью): нулевой интервал
/// превратил бы каждое ожидание обнаружения в немедленную попытку -- цикл
/// без сна. `None` (только события) и любой ненулевой -- допустимы.
pub(crate) fn validate_fallback(fallback: Option<Duration>) -> Result<()> {
    if fallback == Some(Duration::ZERO) {
        return Err(ShmError::InvalidConfig(
            "discovery_fallback must be non-zero (None disables it)",
        ));
    }
    Ok(())
}

/// Меньший из двух необязательных таймаутов (`None` -- без ограничения).
pub(crate) fn min_timeout(a: Option<Duration>, b: Option<Duration>) -> Option<Duration> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, None) => a,
        (None, b) => b,
    }
}

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

/// Состояние маяка присутствия сервера (0.10, INTEROP R19):
/// `SharedServer::presence`, `AutoServer::presence`, `MultiServer::presence`,
/// `DispatchServer::presence`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PresenceStatus {
    /// Маяк взведён: клиенты, ждущие сервер, просыпаются сразу.
    Active,
    /// Маяка нет по устройству: anonymous-сервер, выделенный канал Dispatch,
    /// слот Multi (маяк -- у базы).
    Disabled,
    /// Маяк не создан или не взведён (имя занято объектом другого типа, DACL
    /// маяка, созданного клиентом другого пользователя, не включает сервер,
    /// нет привилегии): сервер работает без маяка, клиенты находят его
    /// резервной проверкой `discovery_fallback` (по умолчанию -- в пределах
    /// 10 с). Причина -- ошибка создания или взвода.
    Unavailable(ShmError),
}

/// Маяк присутствия сервера (R19): создаётся вместе с каналом, взводится
/// после его публикации, сбрасывается первым шагом остановки.
#[derive(Debug)]
pub(crate) struct ServerPresence {
    beacon: Beacon,
}

impl ServerPresence {
    /// Открыть (или создать) маяк `name` в пространстве `root` с
    /// дескриптором канала `security`.
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

    /// Лучшее усилие (решение W(1)): создать и взвести маяк; любая ошибка
    /// -- сервер без маяка и её причина в статусе, старт не отказывает.
    pub(crate) fn start(
        root: Option<&Arc<PrivateNamespace>>,
        name: &str,
        security: &SecurityAttributes,
    ) -> (Option<Self>, PresenceStatus) {
        match Self::create(root, name, security) {
            Ok(presence) => match presence.raise() {
                Ok(()) => (Some(presence), PresenceStatus::Active),
                Err(err) => {
                    // Личность могла успеть попасть в состояние маяка --
                    // обнулить, чтобы клиенты не ждали этот процесс.
                    presence.lower();
                    (None, PresenceStatus::Unavailable(err))
                }
            },
            Err(err) => (None, PresenceStatus::Unavailable(err)),
        }
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
    /// `ControlBlock` секции, если она открылась (недоверенные данные).
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

/// Личность сервера, которую можно ждать (решение W(2)): есть PID и время
/// создания (`0` у `open_peer` -- «любой процесс с этим PID»), и это не сам
/// клиент (ожидание выхода собственного процесса -- вечное).
fn trusted(identity: &(u32, u64)) -> bool {
    let (pid, start) = *identity;
    pid != 0 && start != 0 && pid != std::process::id()
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
    /// Истёк таймаут ожидания (резервная проверка `discovery_fallback` или
    /// таймаут вызывающего): worker-ы делают попытку.
    TimedOut,
    /// Ждать нечего (процесс сервера не открылся, а таймаута нет):
    /// резервный повтор по таймеру (решение R-M).
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
    /// маяк держит; `None` -- пространство держит вызывающий). Существующий
    /// маяк открывается по DACL создателя; `security` -- только для
    /// объектов, которые создаёт этот вызов (клиент пришёл первым; решение
    /// R-L -- та же политика, что у канала в опциях клиента).
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
    /// событие вызывающего (индекс 0), `timeout` -- уже ограниченный
    /// резервной проверкой (`None` -- без таймаута), `wakeup` -- зовётся на
    /// каждом возврате из ожидания в ядре.
    pub(crate) fn wait(
        &mut self,
        failure: Failure,
        wake: Option<BorrowedHandle<'_>>,
        timeout: Option<Duration>,
        wakeup: &dyn Fn(),
    ) -> Result<PresenceWait> {
        match failure {
            Failure::Absent => self.wait_absent(wake, timeout, wakeup),
            // C1 повторного ревью: личность, чей процесс уже завершился,
            // -- не «отвергнувший сервер» (обычно -- личность упавшего
            // сервера, которую `clear_if_stale` оставил в состоянии маяка, или
            // подмена). Ждать её выхода нечего, а немедленный `Retry` при
            // секции, которая снова отвергнет, -- горячий цикл попыток без
            // единого ожидания. Без личности -- набор ожидания: маяк (если не
            // взведён) и резервная проверка.
            Failure::Refused(section) => match self.cross_checked(section) {
                Some(server) if !is_process_gone(server.0, server.1) => {
                    self.wait_server(server, wake, timeout, wakeup)
                }
                _ => self.wait_set(wake, None, timeout, wakeup),
            },
        }
    }

    /// Личность отвергнувшего сервера (решение W(2)): из `ControlBlock`
    /// секции и из состояния маяка, только доверенные; обе есть и
    /// расходятся -- не верим ни одной.
    fn cross_checked(&self, section: Option<(u32, u64)>) -> Option<(u32, u64)> {
        let beacon = self.beacon.server_identity().filter(trusted);
        // m2 повторного ревью: PID секции сверяется с маяком и тогда, когда
        // её личность недоверенная (время 0 -- сервер 0.9 или подмена):
        // секция указывает на другой процесс -- маяку тоже не верим.
        if let (Some((section_pid, _)), Some((beacon_pid, _))) = (section, beacon)
            && section_pid != 0
            && section_pid != beacon_pid
        {
            return None;
        }
        let section = section.filter(trusted);
        match (section, beacon) {
            (Some(section), Some(beacon)) if section != beacon => None,
            (Some(section), _) => Some(section),
            (None, beacon) => beacon,
        }
    }

    /// «Сервера нет».
    fn wait_absent(
        &mut self,
        wake: Option<BorrowedHandle<'_>>,
        timeout: Option<Duration>,
        wakeup: &dyn Fn(),
    ) -> Result<PresenceWait> {
        if !self.beacon.is_raised()? {
            return self.wait_set(wake, None, timeout, wakeup);
        }
        if self.beacon.clear_if_stale()? {
            // Маяк был за упавший сервер и сброшен (взвод нового сервера,
            // случившийся в этом окне, возвращён по поколению). Ещё одна
            // попытка: маяк без состояния гасит и взвод живого сервера,
            // опубликовавшего канал после нашей неудачи, -- попытка после
            // сброса его застанет (R19: публикация раньше взвода).
            return Ok(PresenceWait::Retry);
        }
        // Сервер жив по состоянию маяка, а канала нет: скорее всего гонка с
        // его публикацией -- одна повторная попытка на поколение взвода.
        // Повторно на том же поколении -- канал он не держит (пир без R19,
        // ручной `raise` приложения, подменённое состояние): ждать его
        // выхода -- только доверенную личность и не дольше резервной
        // проверки; иначе -- только резервная проверка (маяк взведён).
        if let Some(generation) = self.beacon.generation()
            && self.live_retry != Some(generation)
        {
            self.live_retry = Some(generation);
            return Ok(PresenceWait::Retry);
        }
        match self.beacon.server_identity().filter(trusted) {
            Some(server) => self.wait_server(server, wake, timeout, wakeup),
            None => self.wait_set(wake, None, timeout, wakeup),
        }
    }

    /// Ждать выхода процесса сервера `(pid, start)`.
    fn wait_server(
        &self,
        (pid, start): (u32, u64),
        wake: Option<BorrowedHandle<'_>>,
        timeout: Option<Duration>,
        wakeup: &dyn Fn(),
    ) -> Result<PresenceWait> {
        match ProcessWatch::open_peer(pid, start) {
            Some(server) => self.wait_set(wake, Some(&server), timeout, wakeup),
            // Сервер успел завершиться -- попытка, затем маяк. Только путь
            // «сервера нет» (маяк взведён живым по состоянию сервером): там
            // следующий круг гасит маяк (`clear_if_stale`) и засыпает на нём
            // -- повторов без ожидания не больше одного. Путь отказа сюда
            // мёртвую личность не передаёт (C1).
            None if is_process_gone(pid, start) => Ok(PresenceWait::Retry),
            // Не открылся (права, служба в другой сессии) -- без подмены
            // источника личности: резервный повтор (решение W(2)).
            None => Ok(PresenceWait::Unavailable),
        }
    }

    /// Одно ожидание `[wake, процесс сервера, маяк]`: `wake` -- если задан
    /// (индекс 0), процесс -- если ждём его, маяк -- только если он НЕ
    /// взведён (проверка прямо перед ожиданием: взвод после неё остаётся
    /// взведённым и разбудит; взведённый маяк вернул бы ожидание сразу --
    /// горячий цикл). Набор пуст -- спать до таймаута; без таймаута --
    /// `Unavailable`.
    fn wait_set(
        &self,
        wake: Option<BorrowedHandle<'_>>,
        server: Option<&ProcessWatch>,
        timeout: Option<Duration>,
        wakeup: &dyn Fn(),
    ) -> Result<PresenceWait> {
        let beacon_down = !self.beacon.is_raised()?;
        let mut handles: Vec<BorrowedHandle<'_>> = Vec::with_capacity(3);
        handles.extend(wake);
        handles.extend(server.map(AsHandle::as_handle));
        if beacon_down {
            handles.push(self.beacon.as_handle());
        }
        if handles.is_empty() {
            let Some(timeout) = timeout else {
                return Ok(PresenceWait::Unavailable);
            };
            std::thread::sleep(timeout);
            wakeup();
            return Ok(PresenceWait::TimedOut);
        }
        #[cfg(test)]
        PRESENCE_KERNEL_WAITS.with(|c| c.set(c.get() + 1));
        let fired = wait_any(&handles, timeout);
        wakeup();
        Ok(match fired? {
            None => PresenceWait::TimedOut,
            Some(0) if wake.is_some() => PresenceWait::Woken,
            Some(_) => PresenceWait::Retry,
        })
    }
}

/// Ошибки «вне подключения», о которых уже сообщено (M1 ревью Task 11):
/// одинаковая ошибка ПОДРЯД сообщается один раз; другая ошибка между ними --
/// и повтор снова сообщается. Ошибки маяка -- отдельный ряд (иначе
/// чередование «ошибка попытки, ошибка маяка» сообщалось бы на каждом
/// круге резерва).
#[derive(Debug, Default)]
pub(crate) struct Reported {
    last: Option<ShmError>,
    presence: Option<ShmError>,
}

impl Reported {
    /// Ошибка попытки: `true` -- сообщить (не повтор предыдущей).
    pub(crate) fn first(&mut self, err: &ShmError) -> bool {
        if self.last.as_ref() == Some(err) {
            return false;
        }
        self.last = Some(err.clone());
        true
    }

    /// Ошибка попытки, о которой сообщается всегда (занятый канал): только
    /// запомнить как последнюю.
    pub(crate) fn note(&mut self, err: &ShmError) {
        self.last = Some(err.clone());
    }

    /// Ошибка маяка: `true` -- сообщить (не повтор предыдущей ошибки маяка).
    pub(crate) fn presence_first(&mut self, err: &ShmError) -> bool {
        // Та же ошибка, что у попытки (отказ в доступе и к каналу, и к
        // маяку), -- уже сообщена.
        if self.presence.as_ref() == Some(err) || self.last.as_ref() == Some(err) {
            return false;
        }
        self.presence = Some(err.clone());
        true
    }

    /// Подключились -- ряды окончены.
    pub(crate) const fn clear(&mut self) {
        self.last = None;
        self.presence = None;
    }
}

/// Параметры [`SharedClient::wait_for_server_with`](crate::SharedClient::wait_for_server_with)
/// (0.10): политика маяка, если его создаёт клиент, резервная проверка и
/// частное пространство имён. Строится от `DiscoveryOptions::default()`
/// (`ObjectSecurity::CurrentUser`, [`DEFAULT_DISCOVERY_FALLBACK`], без
/// пространства) методами.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct DiscoveryOptions {
    /// Политика объектов маяка, если его создаёт ожидающий (сервера ещё
    /// нет). DACL маяка задаёт первый создатель (решение R-L): берите ту
    /// же политику, что у канала сервера (`ChannelConfig::security`) --
    /// иначе сервер другого пользователя не откроет маяк и будет найден
    /// только резервной проверкой.
    pub security: ObjectSecurity,
    /// Резервная проверка (решение W(3)): пока сервер не найден, проверять
    /// канал не реже этого интервала, даже если маяк молчит. `None` --
    /// только события; `Some(Duration::ZERO)` -- `ShmError::InvalidConfig`
    /// (цикл без сна).
    pub fallback: Option<Duration>,
    /// Частное пространство имён канала (`None` -- `\BaseNamedObjects`).
    pub namespace: Option<Arc<PrivateNamespace>>,
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        Self {
            security: ObjectSecurity::default(),
            fallback: Some(DEFAULT_DISCOVERY_FALLBACK),
            namespace: None,
        }
    }
}

impl DiscoveryOptions {
    /// Заменить политику маяка.
    #[must_use]
    pub fn security(mut self, security: ObjectSecurity) -> Self {
        self.security = security;
        self
    }

    /// Заменить резервную проверку.
    #[must_use]
    pub const fn fallback(mut self, fallback: Option<Duration>) -> Self {
        self.fallback = fallback;
        self
    }

    /// Искать канал в частном пространстве имён.
    #[must_use]
    pub fn namespace(mut self, namespace: Arc<PrivateNamespace>) -> Self {
        self.namespace = Some(namespace);
        self
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
/// появится (`Ok(true)`), не дольше `timeout` (`Ok(false)`); `root` --
/// пространство (у `options.namespace` приоритет вызывающего).
pub(crate) fn wait_for_server(
    root: Option<&PrivateNamespace>,
    name: &str,
    timeout: Option<Duration>,
    options: &DiscoveryOptions,
) -> Result<bool> {
    validate_fallback(options.fallback)?;
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
            // Маяк недоступен (нет прав, нет привилегии `Global\`, занят
            // объектом другого типа) -- резерв ниже, маяк пробуем снова на
            // следующем круге.
            watch = PresenceWatch::open(root, None, name, &options.security).ok();
        }
        let waited = match watch.as_mut() {
            Some(watch) => watch.wait(
                Failure::Absent,
                None,
                min_timeout(left, options.fallback),
                &|| {},
            )?,
            None => PresenceWait::Unavailable,
        };
        match waited {
            PresenceWait::Retry | PresenceWait::Woken | PresenceWait::TimedOut => {}
            PresenceWait::Unavailable => {
                let pause = min_timeout(
                    Some(PRESENCE_RETRY_DELAY),
                    min_timeout(left, options.fallback),
                );
                std::thread::sleep(pause.unwrap_or(PRESENCE_RETRY_DELAY));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! M4 ревью Task 11: исходы `PresenceWatch::wait` по ветвям.

    use super::*;
    use crate::test_support::{LiveChild, dead_process};
    use crate::win::EventHandle;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn unique(tag: &str) -> String {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        format!(
            "PRES_UNIT_{tag}_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Клиентский маяк и «серверный» (тот же объект) для подделки
    /// состояния.
    fn pair(tag: &str) -> (PresenceWatch, Beacon) {
        let name = unique(tag);
        let watch = PresenceWatch::open(None, None, &name, &ObjectSecurity::default()).unwrap();
        let server = Beacon::open(&name).unwrap();
        (watch, server)
    }

    fn waits() -> u32 {
        PRESENCE_KERNEL_WAITS.with(std::cell::Cell::get)
    }

    #[test]
    fn absent_beacon_down_retries_on_raise_and_times_out_on_fallback() {
        let (mut watch, server) = pair("DOWN");
        let before = waits();
        let started = Instant::now();
        assert_eq!(
            watch
                .wait(
                    Failure::Absent,
                    None,
                    Some(Duration::from_millis(150)),
                    &|| {}
                )
                .unwrap(),
            PresenceWait::TimedOut
        );
        assert!(started.elapsed() >= Duration::from_millis(140));
        assert_eq!(waits() - before, 1, "одно ожидание в ядре");

        let raiser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            server.raise().unwrap();
            server
        });
        assert_eq!(
            watch
                .wait(Failure::Absent, None, Some(Duration::from_secs(5)), &|| {})
                .unwrap(),
            PresenceWait::Retry
        );
        drop(raiser.join().unwrap());
    }

    #[test]
    fn absent_stale_beacon_is_cleared() {
        let (mut watch, server) = pair("STALE");
        let (pid, start) = dead_process();
        server.raise_as(pid, start).unwrap();
        assert_eq!(
            watch.wait(Failure::Absent, None, None, &|| {}).unwrap(),
            PresenceWait::Retry
        );
        assert!(!server.is_raised().unwrap(), "маяк упавшего сброшен");
    }

    /// Взведён живым процессом без канала: одна попытка на поколение, затем
    /// ожидание его выхода; новый взвод (новое поколение) -- снова попытка.
    #[test]
    fn live_beacon_retries_once_per_generation_then_waits_process() {
        let (mut watch, server) = pair("LIVE");
        let mut child = LiveChild::spawn();
        server.raise_as(child.pid, child.start).unwrap();
        let wait = |watch: &mut PresenceWatch| {
            watch
                .wait(Failure::Absent, None, Some(Duration::from_secs(5)), &|| {})
                .unwrap()
        };
        assert_eq!(wait(&mut watch), PresenceWait::Retry, "попытка поколения");
        server.raise_as(child.pid, child.start).unwrap();
        assert_eq!(wait(&mut watch), PresenceWait::Retry, "новое поколение");

        let started = Instant::now();
        let killer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            child.kill();
        });
        assert_eq!(wait(&mut watch), PresenceWait::Retry, "выход процесса");
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(100), "{took:?}");
        assert!(took < Duration::from_secs(4), "{took:?}");
        killer.join().unwrap();
    }

    /// Подменённая личность (время 0 -- «любой процесс с PID») и свой PID
    /// не принимаются: после попытки поколения -- только резервный таймаут,
    /// а не вечное ожидание чужого/своего процесса.
    #[test]
    fn untrusted_beacon_identity_waits_only_fallback() {
        let child = LiveChild::spawn();
        for (tag, identity) in [
            ("START0", (child.pid, 0)),
            (
                "OWN",
                (std::process::id(), crate::win::current_process_start_time()),
            ),
        ] {
            let (mut watch, server) = pair(tag);
            server.raise_as(identity.0, identity.1).unwrap();
            let timeout = Some(Duration::from_millis(150));
            assert_eq!(
                watch.wait(Failure::Absent, None, timeout, &|| {}).unwrap(),
                PresenceWait::Retry,
                "{tag}: попытка поколения"
            );
            let (started, before) = (Instant::now(), waits());
            assert_eq!(
                watch.wait(Failure::Absent, None, timeout, &|| {}).unwrap(),
                PresenceWait::TimedOut,
                "{tag}"
            );
            assert!(started.elapsed() < Duration::from_secs(2), "{tag}");
            // Процесс в набор не попал: маяк взведён, `wake` нет -- набор
            // пуст, ожидания в ядре нет (только резервный таймаут).
            assert_eq!(waits() - before, 0, "{tag}: ждали недоверенный процесс");
        }
    }

    /// Отказ: личности секции и маяка совпали -- ждём процесс; разошлись --
    /// не верим ни одной (только таймаут).
    #[test]
    fn refused_identity_is_cross_checked_with_beacon() {
        let (mut watch, server) = pair("REFUSED");
        let mut a = LiveChild::spawn();
        let mut b = LiveChild::spawn();
        server.raise_as(a.pid, a.start).unwrap();
        let b_identity = (b.pid, b.start);
        // B завершается во время ожидания: доверься мы личности секции --
        // вернулись бы `Retry` через ~50 мс, а не по таймауту.
        let b_killer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            b.kill();
        });
        let started = Instant::now();
        assert_eq!(
            watch
                .wait(
                    Failure::Refused(Some(b_identity)),
                    None,
                    Some(Duration::from_millis(200)),
                    &|| {}
                )
                .unwrap(),
            PresenceWait::TimedOut,
            "расхождение -- не ждём процесс B"
        );
        assert!(started.elapsed() >= Duration::from_millis(190));
        b_killer.join().unwrap();

        let identity = (a.pid, a.start);
        let killer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            a.kill();
        });
        assert_eq!(
            watch
                .wait(
                    Failure::Refused(Some(identity)),
                    None,
                    Some(Duration::from_secs(5)),
                    &|| {}
                )
                .unwrap(),
            PresenceWait::Retry,
            "совпадение -- выход процесса"
        );
        killer.join().unwrap();
    }

    #[test]
    fn wake_is_index_zero() {
        let (mut watch, _server) = pair("WAKE");
        let wake = EventHandle::create_unnamed(false).unwrap();
        wake.set().unwrap();
        let woke = AtomicU32::new(0);
        assert_eq!(
            watch
                .wait(Failure::Absent, Some(wake.as_handle()), None, &|| {
                    woke.fetch_add(1, Ordering::Relaxed);
                })
                .unwrap(),
            PresenceWait::Woken
        );
        assert_eq!(woke.load(Ordering::Relaxed), 1, "пробуждение посчитано");
    }

    #[test]
    fn reported_dedupes_runs_not_episodes() {
        let mut reported = Reported::default();
        assert!(reported.first(&ShmError::Timeout));
        assert!(!reported.first(&ShmError::Timeout));
        assert!(reported.first(&ShmError::NotReady));
        assert!(reported.first(&ShmError::Timeout), "новый ряд");
        reported.note(&ShmError::AlreadyConnected);
        assert!(reported.first(&ShmError::Timeout));
        assert!(reported.presence_first(&ShmError::AccessDenied));
        assert!(!reported.presence_first(&ShmError::AccessDenied));
        assert!(!reported.first(&ShmError::Timeout), "ряды независимы");
    }

    /// m2 повторного ревью: личность секции без времени (сервер 0.9 или
    /// подмена) -- её PID всё равно сверяется с маяком: другой процесс --
    /// маяку не верим (не ждём B, даже когда он завершается во время
    /// ожидания).
    #[test]
    fn untrusted_section_pid_still_cross_checks_beacon() {
        let (mut watch, server) = pair("PIDCHECK");
        let a = LiveChild::spawn();
        let mut b = LiveChild::spawn();
        server.raise_as(b.pid, b.start).unwrap();
        let killer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            b.kill();
        });
        let started = Instant::now();
        assert_eq!(
            watch
                .wait(
                    Failure::Refused(Some((a.pid, 0))),
                    None,
                    Some(Duration::from_millis(200)),
                    &|| {}
                )
                .unwrap(),
            PresenceWait::TimedOut
        );
        assert!(started.elapsed() >= Duration::from_millis(190));
        killer.join().unwrap();
        drop(a);
    }

    /// C1: отказ, единственная личность (маяка) -- завершившийся процесс:
    /// не немедленный `Retry`, а набор ожидания (маяк не взведён --
    /// ожидание в ядре до таймаута).
    #[test]
    fn refused_dead_identity_waits_instead_of_retry() {
        let (mut watch, server) = pair("DEADID");
        let (pid, start) = dead_process();
        server.raise_as(pid, start).unwrap();
        // Как после `clear_if_stale`: событие сброшено, личность осталась.
        assert!(server.clear_if_stale().unwrap());
        assert_eq!(server.server_identity(), Some((pid, start)));
        let (started, before) = (Instant::now(), waits());
        assert_eq!(
            watch
                .wait(
                    Failure::Refused(None),
                    None,
                    Some(Duration::from_millis(150)),
                    &|| {}
                )
                .unwrap(),
            PresenceWait::TimedOut
        );
        assert!(started.elapsed() >= Duration::from_millis(140));
        assert_eq!(waits() - before, 1, "ожидание маяка в ядре");
    }
}
