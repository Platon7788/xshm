//! Маяк присутствия сервера (0.10, Task 11 надёжности; INTEROP R19) и
//! уведомление «claim освобождён» (Task 12; R20/R21).
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
//!
//! # Уведомление «claim освобождён» (Task 12, INTEROP R20/R21)
//!
//! Занятый канал (`AlreadyConnected`) и база Multi без свободного слота
//! (`NoFreeSlot`) клиент тоже не опрашивает. Рядом с маяком (тот же
//! namespace и дескриптор) сервер создаёт событие-уведомление `<имя>_free`
//! (ручной сброс; база Multi -- `<база>_multi_free`), а в секции
//! `<имя>_beacon_state` живут `free_gen@20` (только `fetch_add` SeqCst --
//! `lock xadd`) и `free_waiters@24` (число спящих ждущих, SeqCst). Клиент
//! объекты только ОТКРЫВАЕТ ([`FreeNotifier::open`]): их нет -- пир без
//! R20, резерв -- прежние `BusyBackoff`/`retry_delay` (решение R-M).
//!
//! **Освобождающий (R20)** -- любой, чей CAS `slot_claim: T -> 0` удался
//! (единственный путь в крейте -- [`release_claim_notify`]), и сервер при
//! `raise`/`lower` маяка: `free_gen.fetch_add(1)`, затем `if
//! free_waiters.load() != 0 { NtSetEvent(_free) }`. Без ждущих -- ни одного
//! системного вызова; `NtPulseEvent` не используется.
//!
//! **Ждущий (R21, [`FreeWaiter`])**: `free_waiters += 1`; круг: `g =
//! free_gen` -> попытка (дешёвая проверка `slot_claim == 0`, затем полное
//! подключение/захват) -> удалась -- выход; иначе толчок `CONNECT_REQ` (раз
//! на захват без сессии) -> `NtResetEvent(_free)` -> `fence(SeqCst)` ->
//! `free_gen != g` ? `NtSetEvent(_free)` (вернуть сигнал, который мог
//! стереть наш сброс) и новый круг : сон на `[wake (0), _free (1), процесс
//! держателя (2), процесс сервера (3)]` без таймаута. Выход -- `free_waiters
//! -= 1` (`Drop`). Сбрасывает событие только ждущий и только прямо перед
//! своей перепроверкой.
//!
//! **Доказательство «освобождение не теряется»** (все операции над
//! `free_gen`/`free_waiters` -- SeqCst, одна полная очерёдность S; сброс и
//! взвод -- системные вызовы, завершённые к следующей операции потока).
//! Возьмём освобождение R (CAS `T -> 0`, затем инкремент `I_R`, затем
//! чтение `free_waiters`) и круг ждущего W (чтение `g`, чтение `slot_claim`,
//! сброс, барьер, перепроверка `P`, сон):
//! - (а) CAS R раньше чтения `slot_claim` W -- W видит `0` (или уже чужой
//!   новый захват, у которого будет своё освобождение) и не спит;
//! - (б) CAS R после чтения `slot_claim`, `I_R` раньше `P` в S -- `P` видит
//!   новое поколение: W не спит и возвращает сигнал (`NtSetEvent`), который
//!   его сброс мог стереть у других ждущих;
//! - (в) `I_R` позже `P` в S -- инкремент `free_waiters` у W (до первого `g`)
//!   раньше `P`, значит, раньше чтения `free_waiters` у R: R видит `!= 0` и
//!   взводит событие ПОСЛЕ сброса W (сброс -> барьер -> `P` <S `I_R` ->
//!   чтение -> `NtSetEvent`), к моменту сна событие взведено.
//!
//! Чужой сброс, стерший сигнал R, -- сброс другого ждущего W': либо W'
//! увидел новое поколение и вернул сигнал (случай (б) для W'), либо его
//! попытка ПОСЛЕ освобождения R провалилась -- значит, claim уже снова
//! занят, и спать можно (новое освобождение снова уведомит). Dekker-пара:
//! освобождающий, прочитавший `free_waiters == 0`, в S раньше инкремента
//! ждущего -- тогда и `I_R` раньше первого чтения `g`, а CAS R (Release
//! перед SeqCst-инкрементом) виден чтению `slot_claim` после `g` (Acquire
//! после SeqCst-чтения, увидевшего `I_R`): ждущий видит освобождение сам.
//! Упавший ждущий оставляет `free_waiters > 0` -- цена только в лишних
//! сигналах. Враждебный писатель счётчиков и события может лишь разбудить
//! ждущих лишний раз (работа на пробуждение O(1)) или не дать им проснуться
//! до смерти процесса держателя/сервера -- то же, что пир без R20 (INTEROP
//! §h).
//!
//! **Страховка (fix round 1, решение AE, I2)**: сон ждущего ограничен
//! `discovery_fallback` (Auto/Multi -- опции клиента, `wait_connectable` --
//! `DiscoveryOptions::fallback`; по умолчанию 10 с, `None` -- только
//! события), затем попытка: держатель-пир без R20, освободивший claim без
//! уведомления, и занятость без claim-а (`client_state != IDLE` при
//! свободном claim-е) находятся в её пределах. Только пока клиент ждёт
//! занятый канал -- простой подключённого клиента не задет. Своё
//! освобождение внутри попытки (откат C6) тоже меняет `free_gen`:
//! перепроверка возвращает сигнал, и остальные ждущие просыпаются один
//! лишний раз (безвредно).
//!
//! **Поля владельца (fix round 1, I1)**: клиент очищает свои
//! `slot_owner_pid`/`owner_start` перед каждым освобождением своего claim-а
//! ([`release_own_claim`]), сервер -- поля умершего перед снятием его
//! claim-а (`ControlBlock::forget_owner`); иначе быстрый путь R-O в окне
//! C1-C2 следующего держателя принял бы личность ушедшего (и потом
//! умершего) за его. После пира без очистки это остаётся возможным, поэтому
//! сервер снимает claim R-O только по тройке (токен, личность), умершей не
//! меньше `OWNER_DEATH_CONFIRM` (round 3, решение AI): живого держателя так
//! снимут, лишь если его поток простоял между C1 и C2 дольше срока (A1').
//! Отъём живого держателя модель опровергает (`handshake_model`, вариант
//! `live_rob`: сессия-призрак W, при 3 попытках -- I4).
//!
//! **Толчки без опроса**: захват без сессии (`server_state !=
//! SERVER_READY`) ждущий толкает один раз на наблюдённый токен и ставит
//! разовый повторный толчок через `RESERVE_TIMEOUT + 1 с` (решение R-N) --
//! сервер отнимает протухший захват на нём; подключённая сессия держателя
//! ни толчков, ни таймаута не даёт. Подтверждённо умершего держателя без
//! заявки сервер снимает на толчке не раньше `OWNER_DEATH_CONFIRM` после
//! первого (решение R-O, round 3): толкнув при умершем держателе, ждущий
//! толкает ещё раз через `CONFIRM_RENUDGE_DELAY`. Ждущий Multi
//! толкает занятые слоты без сессии раз на их набор; повторный толчок ему
//! не нужен -- протухший снимает дедлайн диспетчера.

use std::os::windows::io::{AsHandle, BorrowedHandle};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering, fence};
use std::time::{Duration, Instant};

use crate::beacon::{Beacon, STATE_FREE_GEN_OFFSET, STATE_FREE_WAITERS_OFFSET};
use crate::constants::{
    CLAIM_FREE, EVENT_CONNECT_REQ_SUFFIX, HANDSHAKE_CLIENT_HELLO, HANDSHAKE_IDLE,
    HANDSHAKE_SERVER_READY, OWNER_DEATH_CONFIRM, RESERVE_TIMEOUT,
};
use crate::error::{Result, ShmError};
use crate::events::SharedEvents;
use crate::layout::ControlBlock;
use crate::namespace::PrivateNamespace;
use crate::naming::{Direction, ObjectPath};
use crate::ntapi::types::{NOTIFICATION_EVENT, STATUS_OBJECT_NAME_NOT_FOUND};
use crate::security::{ObjectSecurity, SecurityAttributes};
use crate::shared::SharedView;
#[cfg(test)]
use crate::sync_ext::LockExt;
use crate::win::{
    EventHandle, Mapping, ProcessWatch, is_announced_process_exited, is_process_gone, wait_any,
};

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

/// Наименьшая допустимая резервная проверка обнаружения (n1 ревью Task 11):
/// интервал короче превращает ожидание обнаружения почти в опрос (тысячи
/// попыток в секунду, а `NtWaitForMultipleObjects` округляет таймаут вниз до
/// 100 нс -- и до нуля).
pub(crate) const MIN_DISCOVERY_FALLBACK: Duration = Duration::from_millis(1);

/// Проверка `discovery_fallback` (m1 повторного ревью, n1): интервал короче
/// [`MIN_DISCOVERY_FALLBACK`] (1 мс, в том числе ноль) превратил бы каждое
/// ожидание обнаружения в (почти) немедленную попытку -- цикл без сна.
/// `None` (только события) и любой интервал от 1 мс -- допустимы.
pub(crate) fn validate_fallback(fallback: Option<Duration>) -> Result<()> {
    if fallback.is_some_and(|fallback| fallback < MIN_DISCOVERY_FALLBACK) {
        return Err(ShmError::InvalidConfig(
            "discovery_fallback must be at least 1 ms (None disables it)",
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
    /// Уведомитель «claim освобождён» канала (Task 12, R20): `raise` и
    /// `lower` тоже уведомляют -- ждущий занятого канала заново
    /// оценивает канал при смене сервера.
    free: Option<Arc<FreeNotifier>>,
}

impl ServerPresence {
    /// Открыть (или создать) маяк `name` в пространстве `root` с
    /// дескриптором канала `security`.
    pub(crate) fn create(
        root: Option<&Arc<PrivateNamespace>>,
        name: &str,
        security: &SecurityAttributes,
        free: Option<Arc<FreeNotifier>>,
    ) -> Result<Self> {
        Ok(Self {
            beacon: Beacon::open_at(root.map(Arc::as_ref), root.cloned(), name, security)?,
            free,
        })
    }

    /// Канал опубликован: личность сервера в состояние маяка, затем взвод;
    /// затем уведомление «claim освобождён» (R20).
    pub(crate) fn raise(&self) -> Result<()> {
        let raised = self.beacon.raise();
        if let Some(free) = &self.free {
            free.notify();
        }
        raised
    }

    /// Сервер уходит (первым шагом): личность -- в ноль, маяк сброшен,
    /// затем уведомление «claim освобождён» (R20: ждущий занятого канала
    /// не спит на сервере, которого больше нет). Ошибка сброса здесь некуда
    /// сообщить (`Drop`); маяк, оставшийся взведённым, клиенты гасят сами
    /// (`clear_if_stale`, PID 0 после обнуления личности).
    pub(crate) fn lower(&self) {
        let _ = self.beacon.lower();
        if let Some(free) = &self.free {
            free.notify();
        }
    }

    /// Лучшее усилие (решение W(1)): создать и взвести маяк; любая ошибка
    /// -- сервер без маяка и её причина в статусе, старт не отказывает.
    /// `free` -- уведомитель канала (уже созданный вызывающим, тоже лучшее
    /// усилие; `None` -- без него).
    pub(crate) fn start(
        root: Option<&Arc<PrivateNamespace>>,
        name: &str,
        security: &SecurityAttributes,
        free: Option<Arc<FreeNotifier>>,
    ) -> (Option<Self>, PresenceStatus) {
        match Self::create(root, name, security, free) {
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
    /// только события; меньше 1 мс (в том числе `Duration::ZERO`) --
    /// `ShmError::InvalidConfig`
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
        control.server_pid.load(Ordering::Acquire),
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

// ============================================================================
// Уведомление «claim освобождён» (0.10, Task 12 надёжности; INTEROP R20/R21)
// ============================================================================

/// Суффикс события «claim освобождён»: `<имя>_free` рядом с маяком
/// `<имя>_beacon` (тот же namespace и дескриптор; база Multi --
/// `<база>_multi_free`, как и её маяк).
const FREE_SUFFIX: &str = "_free";

/// Уведомитель «claim освобождён» канала (R20): событие-уведомление
/// `<имя>_free` (ручной сброс) и счётчики `free_gen@20`/`free_waiters@24` в
/// секции состояния маяка `<имя>_beacon_state`.
///
/// Создаёт сервер вместе с маяком (лучшее усилие, как маяк: отказ не
/// отказывает старт), клиент только ОТКРЫВАЕТ: объекта нет -- пир без R20,
/// ждущий работает по резерву (`BusyBackoff`/`retry_delay`, решение R-M).
#[derive(Debug)]
pub(crate) struct FreeNotifier {
    event: EventHandle,
    /// Секция состояния маяка: счётчики по смещениям
    /// `STATE_FREE_GEN_OFFSET`/`STATE_FREE_WAITERS_OFFSET` (не меньше
    /// страницы -- проверено при открытии).
    state: Mapping,
    /// Тесты: сколько раз `notify` взвёл событие (статистика).
    #[cfg(test)]
    signals: std::sync::atomic::AtomicU64,
}

impl FreeNotifier {
    /// Сервер: создать (или открыть) `<name>_free` и секцию состояния маяка
    /// `<name>_beacon_state` с дескриптором канала `security`.
    pub(crate) fn create(
        root: Option<&PrivateNamespace>,
        name: &str,
        security: &SecurityAttributes,
    ) -> Result<Self> {
        let event = EventHandle::open_or_create_notification(
            ObjectPath::scoped(root, name, FREE_SUFFIX),
            security,
        )?;
        let state = crate::beacon::open_state_section(root, name, Some(security))?;
        Ok(Self::assemble(event, state))
    }

    /// Клиент: открыть существующие `<name>_free` и состояние маяка (не
    /// создаёт). Нет объекта -- `Err` (пир без R20); событие не
    /// уведомление (подмена: автосброс будил бы одного ждущего) --
    /// `Corrupted`.
    pub(crate) fn open(root: Option<&PrivateNamespace>, name: &str) -> Result<Self> {
        let event = EventHandle::open(ObjectPath::scoped(root, name, FREE_SUFFIX))?;
        if event.event_type()? != NOTIFICATION_EVENT {
            return Err(ShmError::Corrupted);
        }
        let state = crate::beacon::open_state_section(root, name, None)?;
        Ok(Self::assemble(event, state))
    }

    const fn assemble(event: EventHandle, state: Mapping) -> Self {
        Self {
            event,
            state,
            #[cfg(test)]
            signals: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn word(&self, offset: usize) -> Option<&AtomicU32> {
        crate::beacon::state_word(&self.state, offset)
    }

    /// Текущее `free_gen` (SeqCst -- часть Dekker-пары с `free_waiters`).
    pub(crate) fn free_gen(&self) -> u32 {
        self.word(STATE_FREE_GEN_OFFSET)
            .map_or(0, |generation| generation.load(Ordering::SeqCst))
    }

    /// Освобождение (R20): `free_gen += 1`, затем -- только если есть
    /// ждущие -- `NtSetEvent`. Без ждущих -- ни одного системного вызова.
    /// `NtPulseEvent` не используется (сигнал, пришедший между сбросом и
    /// сном ждущего, им терялся бы).
    pub(crate) fn notify(&self) {
        let (Some(generation), Some(waiters)) = (
            self.word(STATE_FREE_GEN_OFFSET),
            self.word(STATE_FREE_WAITERS_OFFSET),
        ) else {
            return;
        };
        // SeqCst (Dekker): инкремент поколения упорядочен раньше чтения
        // `free_waiters`, а у ждущего инкремент `free_waiters` -- раньше
        // чтения поколения (см. модуль, доказательство R21).
        generation.fetch_add(1, Ordering::SeqCst);
        if waiters.load(Ordering::SeqCst) != 0 {
            #[cfg(test)]
            // Relaxed: статистика теста.
            self.signals.fetch_add(1, Ordering::Relaxed);
            // Отказ `NtSetEvent` здесь некуда сообщить (освобождение уже
            // состоялось); ждущих разбудит процесс держателя/сервера или
            // следующее освобождение.
            let _ = self.event.set();
        }
    }

    /// Тесты: сколько раз `notify` взвёл событие.
    #[cfg(test)]
    pub(crate) fn signals(&self) -> u64 {
        self.signals.load(Ordering::Relaxed)
    }

    /// Тесты: взведено ли событие сейчас.
    #[cfg(test)]
    pub(crate) fn is_set(&self) -> bool {
        self.event.wait(Some(Duration::ZERO)).unwrap_or(false)
    }

    /// Тесты: текущее `free_waiters`.
    #[cfg(test)]
    pub(crate) fn waiters(&self) -> u32 {
        self.word(STATE_FREE_WAITERS_OFFSET)
            .map_or(0, |waiters| waiters.load(Ordering::SeqCst))
    }
}

/// Освободить claim-слово `word` (CAS `token -> 0`) и уведомить ждущих
/// (R20). `true` -- освобождено этим вызовом. `token == 0` -- ничего не
/// освобождается (CAS `0 -> 0` не освобождение).
pub(crate) fn release_word_notify(
    word: &AtomicU32,
    token: u32,
    notifier: Option<&FreeNotifier>,
) -> bool {
    let released = token != CLAIM_FREE
        && word
            .compare_exchange(token, CLAIM_FREE, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
    if released && let Some(notifier) = notifier {
        notifier.notify();
    }
    released
}

/// ЕДИНСТВЕННОЕ освобождение `ControlBlock::slot_claim` в крейте (Task 12,
/// R20): CAS `token -> 0`, при удаче -- уведомление «claim освобождён».
/// Все пути (откат C6, C8, отзыв W, уход L, `connect_async`, Multi
/// `release_claim`; сервер -- S2, `mark_disconnected`, `abort_session`,
/// отъём протухшего захвата, снятие умершего держателя; диспетчер Multi --
/// дедлайн, уход, смерть, осиротевший слот) идут через него.
pub(crate) fn release_claim_notify(
    control: &ControlBlock,
    token: u32,
    notifier: Option<&FreeNotifier>,
) -> bool {
    release_word_notify(&control.slot_claim, token, notifier)
}

/// Освободить СВОЙ claim (откат, отзыв, уход; клиент): fix round 1 (решение
/// AE, INTEROP R18) -- сначала очистить свои поля владельца, затем
/// `release_claim_notify`. Иначе личность ушедшего, если его процесс потом
/// завершится, сошла бы за следующего держателя в окне C1-C2 (быстрый путь
/// R-O снял бы его claim). Follow-up (N4): очистка -- CAS от своей личности
/// (PID и время своего процесса, записанные на C2): если claim уже отняли и
/// следующий держатель записал свои поля, они целы. Остаток: следующий
/// держатель -- клиент того же процесса (та же личность); тогда его поля
/// обнулены -- только хуже наблюдение, отъём R-O по PID 0 невозможен.
pub(crate) fn release_own_claim(
    control: &ControlBlock,
    token: u32,
    notifier: Option<&FreeNotifier>,
) -> bool {
    if token != CLAIM_FREE && control.slot_claim.load(Ordering::Acquire) == token {
        control.forget_owner(std::process::id(), crate::win::current_process_start_time());
    }
    release_claim_notify(control, token, notifier)
}

/// Задержка разового повторного толчка протухшего захвата (решение R-N):
/// `RESERVE_TIMEOUT + 1 с` от первого толчка этого токена -- к повторному
/// толчку сервер уже отсчитал свой `RESERVE_TIMEOUT` и отнимает claim.
/// Тесты подменяют её для канала `name` (`set_renudge_delay`).
#[cfg(not(test))]
pub(crate) const fn renudge_delay(_name: &str) -> Duration {
    RENUDGE_DELAY
}

/// `RESERVE_TIMEOUT + 1 с` (см. `renudge_delay`).
const RENUDGE_DELAY: Duration = RESERVE_TIMEOUT.saturating_add(Duration::from_secs(1));

/// Тесты: `renudge_delay` с подменой по имени канала.
#[cfg(test)]
pub(crate) fn renudge_delay(name: &str) -> Duration {
    RENUDGE_OVERRIDE
        .lock_clean()
        .iter()
        .find(|(channel, _)| channel == name)
        .map_or(RENUDGE_DELAY, |(_, delay)| *delay)
}

/// Тесты: подмена задержки повторного толчка по имени канала.
#[cfg(test)]
static RENUDGE_OVERRIDE: std::sync::Mutex<Vec<(String, Duration)>> =
    std::sync::Mutex::new(Vec::new());

/// Тесты: задержка повторного толчка канала `name`.
#[cfg(test)]
pub(crate) fn set_renudge_delay(name: &str, delay: Duration) {
    RENUDGE_OVERRIDE.lock_clean().push((name.to_owned(), delay));
}

/// Что видит ждущий у занятого канала.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Busy {
    /// Ключ захвата, который стоит толкнуть (`CONNECT_REQ`): токен claim-а
    /// канала без сессии (`server_state != SERVER_READY`); Multi -- свёртка
    /// таких `(слот, токен)`. `0` -- толкать нечего (подключённая сессия:
    /// ни толчков, ни повторного толчка).
    pub(crate) key: u64,
    /// Личность держателя (`slot_owner_pid`, `owner_start`): его смерть --
    /// повод попытаться снова (single-client; Multi -- `None`).
    pub(crate) holder: Option<(u32, u64)>,
    /// Личность сервера из `ControlBlock`: его смерть -- `ServerGone`.
    pub(crate) server: Option<(u32, u64)>,
}

/// Исход попытки ждущего.
pub(crate) enum Attempt<T> {
    /// Получилось (захват/подключение).
    Done(T),
    /// Занято: ждать освобождения.
    Busy(Busy),
    /// Другая ошибка -- вызывающему.
    Failed(ShmError),
}

/// Чем кончилось [`FreeWaiter::wait_free`].
#[derive(Debug)]
pub(crate) enum FreeWait<T> {
    /// Попытка удалась.
    Claimed(T),
    /// Остановка (`running() == false`).
    Stopped,
    /// Процесс сервера завершился.
    ServerGone,
    /// Истёк дедлайн вызывающего.
    TimedOut,
    /// Попытка вернула другую ошибку.
    Failed(ShmError),
}

/// Параметры ожидания [`FreeWaiter::wait_free`].
pub(crate) struct WaitCtl<'a> {
    /// `wake` вызывающего -- индекс 0 (П1 Task 10).
    pub(crate) wake: Option<BorrowedHandle<'a>>,
    /// Работать ли дальше (сверяется на каждом круге и после `wake`).
    pub(crate) running: &'a dyn Fn() -> bool,
    /// `wake` без остановки (например, `send`): отчёт вызывающего.
    pub(crate) on_wake: &'a mut dyn FnMut(),
    /// Каждый возврат из ожидания в ядре (счётчики пробуждений).
    pub(crate) wakeup: &'a dyn Fn(),
    /// Дедлайн вызывающего (`None` -- без таймаута).
    pub(crate) deadline: Option<Instant>,
    /// Задержка разового повторного толчка (R-N); `None` -- без него (Multi:
    /// протухший захват снимает дедлайн диспетчера после первого толчка).
    pub(crate) renudge: Option<Duration>,
}

/// Наблюдаемый процесс по личности: открывается один раз на личность.
#[derive(Debug, Default)]
struct Watched(Option<Watch>);

/// Личность, её процесс и подтверждённая смерть.
#[derive(Debug)]
struct Watch {
    identity: (u32, u64),
    process: Option<ProcessWatch>,
    /// Процесс подтверждённо завершился (сработал handle или его уже не
    /// было при открытии, `win::is_process_gone`).
    dead: bool,
}

impl Watched {
    /// Новая личность -- открыть её процесс (только доверенную, решение
    /// W(2): без времени создания и свой PID не ждём).
    fn update(&mut self, identity: Option<(u32, u64)>) {
        match identity.filter(trusted) {
            None => self.0 = None,
            Some(identity) => {
                if self.0.as_ref().map(|watch| watch.identity) != Some(identity) {
                    let process = ProcessWatch::open_peer(identity.0, identity.1);
                    let dead = process.is_none() && is_process_gone(identity.0, identity.1);
                    self.0 = Some(Watch {
                        identity,
                        process,
                        dead,
                    });
                }
            }
        }
    }

    fn handle(&self) -> Option<BorrowedHandle<'_>> {
        self.0
            .as_ref()
            .and_then(|watch| watch.process.as_ref())
            .map(AsHandle::as_handle)
    }

    /// Процесс завершился: handle сигнален навсегда -- из набора долой
    /// (иначе каждый круг возвращался бы сразу), личность помним.
    fn retire(&mut self) {
        if let Some(watch) = self.0.as_mut() {
            watch.process = None;
            watch.dead = true;
        }
    }

    /// Процесс личности подтверждённо завершился.
    fn is_dead(&self) -> bool {
        self.0.as_ref().is_some_and(|watch| watch.dead)
    }
}

/// Повторный толчок подтверждения смерти держателя (round 3, решение AI):
/// сервер снимает claim умершего держателя (R-O) только на толчке не
/// раньше `OWNER_DEATH_CONFIRM` после первого такого наблюдения, поэтому
/// ждущий, толкнувший при подтверждённо умершем держателе, толкает ещё раз
/// через срок и запас на задержку обработки толчка сервером. Не успел
/// сервер (был занят дольше запаса) -- остаётся отъём по `RESERVE_TIMEOUT`
/// и разовый повторный толчок R-N.
pub(crate) const CONFIRM_RENUDGE_DELAY: Duration =
    OWNER_DEATH_CONFIRM.saturating_add(Duration::from_millis(250));

/// Тесты: точки вмешательства в круг ждущего (детерминированные
/// интерлинии `free_waiter_interleavings`).
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FreePoint {
    /// После чтения `free_gen`, до попытки.
    Generation,
    /// После неудачной попытки, до сброса события.
    Attempted,
    /// После сброса события, до перепроверки `free_gen`.
    Reset,
}

/// Таймер одного ожидания ждущего.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Timer {
    /// Дедлайн вызывающего.
    Deadline,
    /// Разовый повторный толчок (R-N).
    Renudge,
    /// Повторный толчок подтверждения смерти держателя (round 3).
    Confirm,
}

/// Объект набора ожидания ждущего.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    Wake,
    Free,
    Holder,
    Server,
}

/// Ждущий освобождения claim-а (R21). Пока жив -- учтён в `free_waiters`
/// (освобождающие сигналят событие); `Drop` -- `free_waiters - 1`.
pub(crate) struct FreeWaiter {
    notifier: Arc<FreeNotifier>,
    /// Ключ захвата, который уже толкнули (`Busy::key`).
    nudged: Option<u64>,
    /// Разовый повторный толчок (R-N) этого ключа.
    renudge_at: Option<Instant>,
    /// Повторный толчок подтверждения смерти держателя (round 3,
    /// `CONFIRM_RENUDGE_DELAY`).
    confirm_at: Option<Instant>,
    holder: Watched,
    server: Watched,
    #[cfg(test)]
    hooks: Vec<(FreePoint, Box<dyn FnOnce() + Send>)>,
    /// Тесты: потолок одного ожидания в ядре (детектор потерянного
    /// сигнала) и число ожиданий, упёршихся в него.
    #[cfg(test)]
    kernel_cap: Option<Duration>,
    #[cfg(test)]
    kernel_timeouts: u64,
}

impl std::fmt::Debug for FreeWaiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FreeWaiter")
            .field("nudged", &self.nudged)
            .field("renudge_at", &self.renudge_at)
            .field("confirm_at", &self.confirm_at)
            .finish_non_exhaustive()
    }
}

impl FreeWaiter {
    /// Встать в ждущие: `free_waiters + 1` (SeqCst) -- ДО первого чтения
    /// `free_gen` (Dekker с освобождающим).
    pub(crate) fn new(notifier: Arc<FreeNotifier>) -> Self {
        if let Some(waiters) = notifier.word(STATE_FREE_WAITERS_OFFSET) {
            waiters.fetch_add(1, Ordering::SeqCst);
        }
        Self {
            notifier,
            nudged: None,
            renudge_at: None,
            confirm_at: None,
            holder: Watched::default(),
            server: Watched::default(),
            #[cfg(test)]
            hooks: Vec::new(),
            #[cfg(test)]
            kernel_cap: None,
            #[cfg(test)]
            kernel_timeouts: 0,
        }
    }

    /// Тесты: одноразовый хук в точке `at`.
    #[cfg(test)]
    pub(crate) fn hook(&mut self, at: FreePoint, hook: impl FnOnce() + Send + 'static) {
        self.hooks.push((at, Box::new(hook)));
    }

    #[cfg(test)]
    fn run_hook(&mut self, at: FreePoint) {
        if let Some(index) = self.hooks.iter().position(|(point, _)| *point == at) {
            let (_, hook) = self.hooks.remove(index);
            hook();
        }
    }

    /// Тесты: потолок одного ожидания в ядре.
    #[cfg(test)]
    pub(crate) const fn cap_kernel_wait(&mut self, cap: Duration) {
        self.kernel_cap = Some(cap);
    }

    /// Тесты: ожидания, упёршиеся в потолок (потерянный сигнал).
    #[cfg(test)]
    pub(crate) const fn kernel_timeouts(&self) -> u64 {
        self.kernel_timeouts
    }

    /// Ждать, пока `attempt` не удастся (протокол R21, см. модуль):
    /// `g = free_gen` -> попытка -> (занято) толчок -> сброс `_free` ->
    /// барьер -> `free_gen != g`? вернуть сигнал и снова : сон на
    /// `[wake (0), _free (1), процесс держателя (2), процесс сервера (3)]`.
    ///
    /// `nudge` -- толчок `CONNECT_REQ` (раз на ключ `Busy::key`, плюс разовый
    /// повторный через `ctl.renudge`). Ошибка ОС сброса/взвода/ожидания --
    /// `Err` (вызывающий уходит в резерв).
    pub(crate) fn wait_free<T>(
        &mut self,
        attempt: &mut dyn FnMut() -> Attempt<T>,
        nudge: &mut dyn FnMut(),
        ctl: &mut WaitCtl<'_>,
    ) -> Result<FreeWait<T>> {
        loop {
            if !(ctl.running)() {
                return Ok(FreeWait::Stopped);
            }
            let generation = self.notifier.free_gen();
            #[cfg(test)]
            self.run_hook(FreePoint::Generation);
            let busy = match attempt() {
                Attempt::Done(value) => return Ok(FreeWait::Claimed(value)),
                Attempt::Failed(err) => return Ok(FreeWait::Failed(err)),
                Attempt::Busy(busy) => busy,
            };
            self.observe(&busy, nudge, ctl.renudge);
            #[cfg(test)]
            self.run_hook(FreePoint::Attempted);
            // Сбрасывает только ждущий и только перед своей перепроверкой.
            self.notifier.event.reset()?;
            #[cfg(test)]
            self.run_hook(FreePoint::Reset);
            // Сброс (системный вызов) -- до перечитывания поколения: барьер
            // запрещает поднять чтение выше сброса (как `clear_after` маяка).
            fence(Ordering::SeqCst);
            if self.notifier.free_gen() != generation {
                // Освобождение после нашей проверки: наш сброс мог стереть
                // его сигнал -- вернуть (другим ждущим) и попытаться снова.
                self.notifier.event.set()?;
                continue;
            }
            let now = Instant::now();
            if ctl.deadline.is_some_and(|at| now >= at) {
                return Ok(FreeWait::TimedOut);
            }
            if let Some(stop) = self.sleep(nudge, ctl, now)? {
                return Ok(stop);
            }
        }
    }

    /// Учесть занятость: процессы держателя и сервера; толчок -- раз на
    /// ключ (и разовый повторный через `renudge`); подключённая сессия
    /// (`key == 0`) -- ни толчков, ни таймаута. Толчок при подтверждённо
    /// умершем держателе -- и повторный через `CONFIRM_RENUDGE_DELAY`
    /// (round 3: сервер подтверждает смерть во времени).
    fn observe(&mut self, busy: &Busy, nudge: &mut dyn FnMut(), renudge: Option<Duration>) {
        self.holder.update(busy.holder);
        self.server.update(busy.server);
        if busy.key == 0 {
            self.nudged = None;
            self.renudge_at = None;
            self.confirm_at = None;
            return;
        }
        if self.nudged != Some(busy.key) {
            nudge();
            let now = Instant::now();
            self.nudged = Some(busy.key);
            self.renudge_at = renudge.and_then(|delay| now.checked_add(delay));
            self.confirm_at = if self.holder.is_dead() {
                now.checked_add(CONFIRM_RENUDGE_DELAY)
            } else {
                None
            };
        }
    }

    /// Одно ожидание набора. `Some(исход)` -- выйти; `None` -- новый круг.
    fn sleep<T>(
        &mut self,
        nudge: &mut dyn FnMut(),
        ctl: &mut WaitCtl<'_>,
        now: Instant,
    ) -> Result<Option<FreeWait<T>>> {
        // Таймер ожидания: дедлайн вызывающего или разовый повторный толчок
        // (ближайший; равные -- дедлайн). Истёкшее ожидание засчитывается
        // ИМЕННО этому таймеру, без повторной сверки `Instant`: ядро может
        // вернуть таймаут чуть раньше отметки `Instant` (разрешение таймера),
        // и сверка дала бы лишний короткий сон -- лишнее пробуждение.
        // Повторные толчки -- строго раньше дедлайна, иначе он важнее.
        let timer = [
            self.confirm_at.map(|at| (at, Timer::Confirm)),
            self.renudge_at.map(|at| (at, Timer::Renudge)),
        ]
        .into_iter()
        .flatten()
        .min_by_key(|(at, _)| *at)
        .filter(|(at, _)| ctl.deadline.is_none_or(|deadline| *at < deadline))
        .or_else(|| ctl.deadline.map(|at| (at, Timer::Deadline)));
        let timeout = timer.map(|(at, _)| at.saturating_duration_since(now));
        #[cfg(test)]
        let (timeout, capped) = match (timeout, self.kernel_cap) {
            (Some(t), Some(cap)) if cap < t => (Some(cap), true),
            (None, Some(cap)) => (Some(cap), true),
            (t, _) => (t, false),
        };
        let mut slots: Vec<Slot> = Vec::with_capacity(4);
        let mut handles: Vec<BorrowedHandle<'_>> = Vec::with_capacity(4);
        if let Some(wake) = ctl.wake {
            slots.push(Slot::Wake);
            handles.push(wake);
        }
        slots.push(Slot::Free);
        handles.push(self.notifier.event.as_handle());
        if let Some(holder) = self.holder.handle() {
            slots.push(Slot::Holder);
            handles.push(holder);
        }
        if let Some(server) = self.server.handle() {
            slots.push(Slot::Server);
            handles.push(server);
        }
        let fired = wait_any(&handles, timeout);
        (ctl.wakeup)();
        let mut slot = match fired? {
            Some(index) => slots.get(index).copied(),
            None => None,
        };
        if slot == Some(Slot::Wake) {
            if !(ctl.running)() {
                return Ok(Some(FreeWait::Stopped));
            }
            (ctl.on_wake)();
            // П2 (Task 10): `wake` без остановки (поток `send`) не голодит
            // остальные объекты -- одно нулевое ожидание без `wake`.
            slot = wait_any(&handles[1..], Some(Duration::ZERO))?
                .and_then(|index| slots.get(index + 1).copied());
            if slot.is_none() {
                return Ok(None);
            }
        }
        match slot {
            Some(Slot::Server) => Ok(Some(FreeWait::ServerGone)),
            Some(Slot::Holder) => {
                // Держатель умер: снова попытка и толчок того же ключа
                // (сервер подтвердит смерть и освободит claim сразу, R18).
                self.holder.retire();
                self.nudged = None;
                Ok(None)
            }
            Some(Slot::Free | Slot::Wake) => Ok(None),
            None => {
                #[cfg(test)]
                if capped {
                    // Ожидание упёрлось в тестовый потолок, а не в таймер.
                    self.kernel_timeouts += 1;
                    return Ok(None);
                }
                match timer.map(|(_, timer)| timer) {
                    Some(Timer::Renudge) => {
                        // Разовый: ключ остаётся «толкнутым», новый таймер не
                        // ставится.
                        nudge();
                        self.renudge_at = None;
                        Ok(None)
                    }
                    Some(Timer::Confirm) => {
                        // Разовый на толчок: сервер подтвердил смерть и
                        // снимет claim (освобождение разбудит `_free`).
                        nudge();
                        self.confirm_at = None;
                        Ok(None)
                    }
                    Some(Timer::Deadline) => Ok(Some(FreeWait::TimedOut)),
                    // Нулевое ожидание после `wake` (П2) ничего не застало.
                    None => Ok(None),
                }
            }
        }
    }
}

impl Drop for FreeWaiter {
    fn drop(&mut self) {
        // Упавший ждущий оставляет счётчик > 0 -- цена только в лишних
        // сигналах освобождающих.
        if let Some(waiters) = self.notifier.word(STATE_FREE_WAITERS_OFFSET) {
            waiters.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// Занятость single-client канала `name` (ждущие `AutoClient` и
/// `wait_connectable`): `None` -- канал свободен; иначе -- что видит
/// ждущий.
///
/// Заголовок секции открывается на каждую попытку и закрывается сразу (fix
/// round 1, m4): удерживаемое на всё ожидание отображение держало бы имя
/// секции упавшего сервера, и перезапущенный сервер не смог бы создать
/// канал заново, пока ждущий спит.
///
/// Занят и канал без claim-а, но с сессией или заявкой (`client_state !=
/// IDLE`: сессия-призрак, клиент без claim, враждебный писатель; fix round
/// 1, m2): `connect` ответил бы `AlreadyConnected`, а «свободен» вернул бы
/// вызывающего в горячий цикл. Освобождения claim-а у такой занятости
/// нет -- её снимает страховочная проверка `discovery_fallback`.
pub(crate) fn channel_busy(root: Option<&PrivateNamespace>, name: &str) -> Result<Option<Busy>> {
    let mapping = Mapping::open_header(ObjectPath::mapping(root, name))?;
    let control = SharedView::peek_control(&mapping).ok_or(ShmError::NotReady)?;
    let claim = control.slot_claim.load(Ordering::Acquire);
    let server = Some((
        control.server_pid.load(Ordering::Acquire),
        control.server_start_time(),
    ));
    if claim == CLAIM_FREE {
        let state = control.client_state.load(Ordering::Acquire);
        if state == HANDSHAKE_IDLE {
            return Ok(None);
        }
        // Follow-up (решение AG, N2): заявка без держателя claim-а (умерший
        // после отъёма, пир без claim) -- её снимает только сервер (S2) и
        // только на `CONNECT_REQ`: ненулевой ключ (вне диапазона токенов,
        // по самоописанию заявки) -- ждущий толкнёт один раз на заявку.
        // Сессия без claim-а (`SERVER_READY`) -- толкать нечего.
        let key = if state == HANDSHAKE_CLIENT_HELLO
            && control.server_state.load(Ordering::Acquire) != HANDSHAKE_SERVER_READY
        {
            (1_u64 << 32) | u64::from(control.client_pid.load(Ordering::Acquire))
        } else {
            0
        };
        return Ok(Some(Busy {
            key,
            holder: None,
            server,
        }));
    }
    let served = control.server_state.load(Ordering::Acquire) == HANDSHAKE_SERVER_READY;
    let owner = control.slot_owner_pid.load(Ordering::Acquire);
    Ok(Some(Busy {
        key: if served { 0 } else { u64::from(claim) },
        holder: Some((owner, control.owner_start_time())),
        server,
    }))
}

/// Толчок сервера канала `name` (`CONNECT_REQ` без заявки): он назначит
/// отсчёт протухшему захвату или сразу снимет claim умершего держателя.
/// Событие открывается на толчок (редкость: раз на захват), а не держится.
pub(crate) fn nudge_channel(root: Option<&PrivateNamespace>, name: &str) {
    if let Ok(event) = EventHandle::open_auto_reset(ObjectPath::event(
        root,
        name,
        Direction::ClientToServer,
        EVENT_CONNECT_REQ_SUFFIX,
    )) {
        let _ = event.set();
    }
}

/// `SharedClient::wait_connectable*`: ждать, пока канал `name` не окажется
/// свободным (`Ok(true)`; захват не выполняется), не дольше `timeout`
/// (`Ok(false)`).
///
/// Fix round 1 (решение AE, I2): каждое ожидание уведомления -- не дольше
/// `fallback` (`DiscoveryOptions::fallback`, по умолчанию 10 с; `None` --
/// только события), затем проверка канала: пир без R20, освободивший claim
/// без уведомления, и занятость без claim-а (m2) находятся в её пределах.
pub(crate) fn wait_connectable(
    root: Option<&PrivateNamespace>,
    name: &str,
    timeout: Option<Duration>,
    fallback: Option<Duration>,
) -> Result<bool> {
    validate_fallback(fallback)?;
    let deadline = crate::ring::deadline_of(timeout);
    let left = || deadline.map(|at| at.saturating_duration_since(Instant::now()));
    // Канала нет -- ошибка открытия сразу.
    if channel_busy(root, name)?.is_none() {
        return Ok(true);
    }
    let Ok(notifier) = FreeNotifier::open(root, name) else {
        // Пир без R20 (или `_free` недоступен): резервная проверка раз в
        // `PRESENCE_RETRY_DELAY` (решение R-M).
        loop {
            if channel_busy(root, name)?.is_none() {
                return Ok(true);
            }
            let left = left();
            if left.is_some_and(|left| left.is_zero()) {
                return Ok(false);
            }
            std::thread::sleep(
                min_timeout(Some(PRESENCE_RETRY_DELAY), left).unwrap_or(PRESENCE_RETRY_DELAY),
            );
        }
    };
    let mut waiter = FreeWaiter::new(Arc::new(notifier));
    loop {
        let left = left();
        if left.is_some_and(|left| left.is_zero()) {
            return Ok(false);
        }
        let waited = waiter.wait_free(
            &mut || match channel_busy(root, name) {
                Ok(None) => Attempt::Done(()),
                Ok(Some(busy)) => Attempt::Busy(busy),
                Err(err) => Attempt::Failed(err),
            },
            &mut || nudge_channel(root, name),
            &mut WaitCtl {
                wake: None,
                running: &|| true,
                on_wake: &mut || {},
                wakeup: &|| {},
                deadline: crate::ring::deadline_of(min_timeout(left, fallback)),
                renudge: Some(renudge_delay(name)),
            },
        )?;
        // Истёк ли таймаут страховки (тогда -- новая проверка), а не
        // вызывающего (тогда -- `false` сразу: ядро может вернуть таймаут
        // чуть раньше отметки `Instant`, и лишняя проверка в остаток была
        // бы случайной).
        let capped = fallback.is_some_and(|fallback| left.is_none_or(|left| fallback < left));
        match waited {
            FreeWait::Claimed(()) => return Ok(true),
            FreeWait::TimedOut | FreeWait::Stopped if !capped => return Ok(false),
            // Страховочная проверка (`fallback`) -- новый круг.
            FreeWait::TimedOut | FreeWait::Stopped => {}
            FreeWait::ServerGone => return Err(ShmError::PeerDied),
            FreeWait::Failed(err) => return Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    //! M4 ревью Task 11: исходы `PresenceWatch::wait` по ветвям.

    use super::*;
    use crate::test_support::{LiveChild, dead_process};
    use crate::win::EventHandle;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

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

    // ─── Task 12: уведомление «claim освобождён» (R20/R21) ──────────────────

    /// Уведомитель тестового канала (создаёт «сервер»).
    fn notifier(tag: &str) -> Arc<FreeNotifier> {
        let name = unique(tag);
        Arc::new(
            FreeNotifier::create(
                None,
                &name,
                &ObjectSecurity::default().attributes().unwrap(),
            )
            .unwrap(),
        )
    }

    /// Попытка ждущего над claim-словом: свободно -- захват своим
    /// токеном, иначе «занято» (толкать нечего).
    fn claim_attempt(word: &AtomicU32, token: u32) -> Attempt<()> {
        match word.compare_exchange(CLAIM_FREE, token, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => Attempt::Done(()),
            Err(_) => Attempt::Busy(Busy::default()),
        }
    }

    /// Ожидание с дедлайном и счётчиком пробуждений.
    fn wait_once(
        waiter: &mut FreeWaiter,
        word: &AtomicU32,
        token: u32,
        deadline: Duration,
        wakeups: &AtomicU32,
        attempts: &mut dyn FnMut(),
    ) -> FreeWait<()> {
        waiter
            .wait_free(
                &mut || {
                    attempts();
                    claim_attempt(word, token)
                },
                &mut || {},
                &mut WaitCtl {
                    wake: None,
                    running: &|| true,
                    on_wake: &mut || {},
                    wakeup: &|| {
                        wakeups.fetch_add(1, Ordering::Relaxed);
                    },
                    deadline: Instant::now().checked_add(deadline),
                    renudge: None,
                },
            )
            .unwrap()
    }

    const HOLDER: u32 = 0xA000_0001;
    const WAITER: u32 = 0xB000_0001;

    /// Три интерлинии протокола ждущего (доказательство R21 в модуле):
    /// (а) освобождение между чтением `free_gen` и проверкой claim-а --
    /// захват без сна; (б) освобождение между проверкой и сбросом -- сброс
    /// стёр сигнал, перепроверка видит новое поколение, событие возвращено
    /// (взведено к следующей попытке), ждущий не спит; (в) освобождение
    /// после перепроверки -- ждущий спит и просыпается не позже 50 мс.
    #[test]
    fn free_waiter_interleavings() {
        // (а)
        let free = notifier("IL_A");
        let word = Arc::new(AtomicU32::new(HOLDER));
        let mut waiter = FreeWaiter::new(Arc::clone(&free));
        let (w, f) = (Arc::clone(&word), Arc::clone(&free));
        waiter.hook(FreePoint::Generation, move || {
            assert!(release_word_notify(&w, HOLDER, Some(&f)));
        });
        let wakeups = AtomicU32::new(0);
        let waited = wait_once(
            &mut waiter,
            &word,
            WAITER,
            Duration::from_secs(2),
            &wakeups,
            &mut || {},
        );
        assert!(matches!(waited, FreeWait::Claimed(())), "{waited:?}");
        assert_eq!(wakeups.load(Ordering::Relaxed), 0, "(а): спал");
        drop(waiter);

        // (б)
        let free = notifier("IL_B");
        let word = Arc::new(AtomicU32::new(HOLDER));
        let mut waiter = FreeWaiter::new(Arc::clone(&free));
        let (w, f) = (Arc::clone(&word), Arc::clone(&free));
        waiter.hook(FreePoint::Attempted, move || {
            assert!(release_word_notify(&w, HOLDER, Some(&f)));
            assert!(f.is_set(), "(б): освобождающий взвёл событие");
        });
        let f = Arc::clone(&free);
        waiter.hook(FreePoint::Reset, move || {
            assert!(!f.is_set(), "(б): сброс стёр сигнал");
        });
        let wakeups = AtomicU32::new(0);
        let mut calls = 0;
        let mut set_at_retry = None;
        let f = Arc::clone(&free);
        let waited = wait_once(
            &mut waiter,
            &word,
            WAITER,
            Duration::from_secs(2),
            &wakeups,
            &mut || {
                calls += 1;
                if calls == 2 {
                    set_at_retry = Some(f.is_set());
                }
            },
        );
        assert!(matches!(waited, FreeWait::Claimed(())), "{waited:?}");
        assert_eq!(calls, 2, "(б): ждущий в цикле -- вторая попытка");
        assert_eq!(set_at_retry, Some(true), "(б): чужой сигнал возвращён");
        assert_eq!(wakeups.load(Ordering::Relaxed), 0, "(б): спал");
        drop(waiter);

        // (в)
        let free = notifier("IL_C");
        let word = Arc::new(AtomicU32::new(HOLDER));
        let mut waiter = FreeWaiter::new(Arc::clone(&free));
        let (w, f) = (Arc::clone(&word), Arc::clone(&free));
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            let at = Instant::now();
            assert!(release_word_notify(&w, HOLDER, Some(&f)));
            at
        });
        let wakeups = AtomicU32::new(0);
        let waited = wait_once(
            &mut waiter,
            &word,
            WAITER,
            Duration::from_secs(5),
            &wakeups,
            &mut || {},
        );
        let woke = Instant::now();
        assert!(matches!(waited, FreeWait::Claimed(())), "{waited:?}");
        let released = releaser.join().unwrap();
        let took = woke.saturating_duration_since(released);
        // m6: событие против дедлайна 5 с -- запас под нагрузкой.
        assert!(took <= Duration::from_millis(250), "(в): {took:?}");
        assert_eq!(wakeups.load(Ordering::Relaxed), 1, "(в): одно пробуждение");
    }

    /// Без ждущих освобождение -- ни одного системного вызова (`NtSetEvent`
    /// не зовётся); с одним ждущим -- сигнал на каждое освобождение.
    #[test]
    fn release_without_waiters_makes_no_syscall() {
        let free = notifier("NOSYS");
        let word = AtomicU32::new(CLAIM_FREE);
        let before = free.free_gen();
        for _ in 0..1000 {
            word.store(HOLDER, Ordering::Release);
            assert!(release_word_notify(&word, HOLDER, Some(&free)));
        }
        assert_eq!(free.waiters(), 0);
        assert_eq!(free.signals(), 0, "сигналы без ждущих");
        assert_eq!(free.free_gen().wrapping_sub(before), 1000, "поколение");
        // Не наш токен -- не освобождение: ни поколения, ни сигнала.
        word.store(HOLDER, Ordering::Release);
        assert!(!release_word_notify(&word, WAITER, Some(&free)));
        assert_eq!(free.free_gen().wrapping_sub(before), 1000);

        let waiter = FreeWaiter::new(Arc::clone(&free));
        assert_eq!(free.waiters(), 1);
        for _ in 0..3 {
            word.store(HOLDER, Ordering::Release);
            assert!(release_word_notify(&word, HOLDER, Some(&free)));
        }
        assert_eq!(free.signals(), 3, "сигналы при ждущем");
        drop(waiter);
        assert_eq!(free.waiters(), 0, "Drop ждущего");
    }

    /// Подержать claim ~20 мкс (спин): ждущие успевают уснуть в ядре, а не
    /// только крутиться на сменившемся поколении.
    fn hold() {
        let until = Instant::now() + Duration::from_micros(20);
        while Instant::now() < until {
            std::hint::spin_loop();
        }
    }

    /// 4 ждущих потока и 2 держателя гоняют одно claim-слово: 20 000
    /// циклов «захват -> освобождение» держателей, каждый ждущий захватывает
    /// `K` раз. Каждое ожидание в ядре ограничено 1 с -- детектор потерянного
    /// сигнала (держатели освобождают постоянно, а после них -- ждущие друг
    /// другу): ни одного такого таймаута; все ждущие завершаются не позже
    /// 2 с после последнего освобождения держателей.
    #[test]
    fn free_waiters_never_miss_release_stress() {
        const HOLDER_CYCLES: u32 = 20_000;
        const K: u32 = 1_000;
        let free = notifier("STRESS");
        let word = Arc::new(AtomicU32::new(CLAIM_FREE));
        let cycles = Arc::new(AtomicU32::new(0));
        let holders_done = Arc::new(std::sync::Mutex::new(None::<Instant>));
        let mut threads = Vec::new();
        for h in 0..2u32 {
            let (word, free, cycles, done) = (
                Arc::clone(&word),
                Arc::clone(&free),
                Arc::clone(&cycles),
                Arc::clone(&holders_done),
            );
            threads.push(std::thread::spawn(move || {
                let token = HOLDER + h;
                while cycles.load(Ordering::Acquire) < HOLDER_CYCLES {
                    if word
                        .compare_exchange(CLAIM_FREE, token, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        hold();
                        assert!(release_word_notify(&word, token, Some(&free)));
                        cycles.fetch_add(1, Ordering::AcqRel);
                    } else {
                        std::thread::yield_now();
                    }
                }
                *done.lock().unwrap() = Some(Instant::now());
                (0u64, 0u32)
            }));
        }
        for w in 0..4u32 {
            let (word, free) = (Arc::clone(&word), Arc::clone(&free));
            threads.push(std::thread::spawn(move || {
                let token = WAITER + w;
                let mut waiter = FreeWaiter::new(Arc::clone(&free));
                waiter.cap_kernel_wait(Duration::from_secs(1));
                let wakeups = AtomicU32::new(0);
                for _ in 0..K {
                    let waited = wait_once(
                        &mut waiter,
                        &word,
                        token,
                        Duration::from_secs(120),
                        &wakeups,
                        &mut || {},
                    );
                    assert!(matches!(waited, FreeWait::Claimed(())), "{waited:?}");
                    hold();
                    assert!(release_word_notify(&word, token, Some(&free)));
                }
                (waiter.kernel_timeouts(), wakeups.load(Ordering::Relaxed))
            }));
        }
        let (mut timeouts, mut slept) = (0, 0);
        for thread in threads {
            let (t, s) = thread.join().unwrap();
            timeouts += t;
            slept += s;
        }
        // Тест проверяет путь сна, а не только круги по поколению.
        assert!(slept >= 100, "ожиданий в ядре: {slept}");
        let finished = Instant::now();
        let last_release = holders_done.lock().unwrap().unwrap();
        assert_eq!(timeouts, 0, "потерянные сигналы (ожидание до потолка 1 с)");
        let tail = finished.saturating_duration_since(last_release);
        assert!(tail <= Duration::from_secs(2), "хвост ждущих: {tail:?}");
        assert_eq!(free.waiters(), 0);
    }

    /// Поток сигналов `_free` (пир, взводящий событие и поколение без
    /// конца) не голодит остановку: `wake` -- индекс 0, остановка видна
    /// не позже 100 мс.
    #[test]
    fn free_flood_does_not_starve_stop() {
        let free = notifier("FLOOD");
        let word = AtomicU32::new(HOLDER);
        let running = Arc::new(AtomicBool::new(true));
        let flooding = Arc::new(AtomicBool::new(true));
        let flooder = {
            let (free, flooding) = (Arc::clone(&free), Arc::clone(&flooding));
            std::thread::spawn(move || {
                while flooding.load(Ordering::Acquire) {
                    free.notify();
                }
            })
        };
        let wake = Arc::new(EventHandle::create_unnamed(false).unwrap());
        let stopper = {
            let (running, wake) = (Arc::clone(&running), Arc::clone(&wake));
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                running.store(false, Ordering::Release);
                wake.set().unwrap();
                Instant::now()
            })
        };
        let mut waiter = FreeWaiter::new(Arc::clone(&free));
        let waited = waiter
            .wait_free(
                &mut || claim_attempt(&word, WAITER),
                &mut || {},
                &mut WaitCtl {
                    wake: Some(wake.as_handle()),
                    running: &|| running.load(Ordering::Acquire),
                    on_wake: &mut || {},
                    wakeup: &|| {},
                    deadline: Instant::now().checked_add(Duration::from_secs(10)),
                    renudge: None,
                },
            )
            .unwrap();
        let returned = Instant::now();
        flooding.store(false, Ordering::Release);
        flooder.join().unwrap();
        let stopped = stopper.join().unwrap();
        assert!(matches!(waited, FreeWait::Stopped), "{waited:?}");
        let took = returned.saturating_duration_since(stopped);
        assert!(took <= Duration::from_millis(100), "{took:?}");
    }

    /// Fix round 1 (m7): `raise` и `lower` маяка сервера уведомляют ждущих
    /// (`free_gen + 1`, при ждущем -- событие взведено).
    #[test]
    fn raise_and_lower_notify_free() {
        let name = unique("RAISELOWER");
        let server = crate::server::SharedServer::start(&name).unwrap();
        let free = Arc::new(FreeNotifier::open(None, &name).unwrap());
        let waiter = FreeWaiter::new(Arc::clone(&free));
        free.event.reset().unwrap();
        let g0 = free.free_gen();
        drop(server);
        assert_eq!(free.free_gen().wrapping_sub(g0), 1, "lower без уведомления");
        assert!(free.is_set(), "lower не взвёл событие при ждущем");
        free.event.reset().unwrap();
        // Объекты держит наш уведомитель: новый сервер того же имени.
        let server = crate::server::SharedServer::start(&name).unwrap();
        assert_eq!(free.free_gen().wrapping_sub(g0), 2, "raise без уведомления");
        assert!(free.is_set(), "raise не взвёл событие при ждущем");
        drop(waiter);
        drop(server);
    }

    /// Разовый повторный толчок (R-N): захват без сессии толкается один
    /// раз при первом наблюдении и один раз -- через `renudge`; новый
    /// таймер на тот же ключ не ставится. Подключённая сессия (`key == 0`)
    /// -- ни одного толчка.
    #[test]
    fn stale_claim_is_renudged_once() {
        let free = notifier("RENUDGE");
        let mut waiter = FreeWaiter::new(Arc::clone(&free));
        let nudges = AtomicU32::new(0);
        let wakeups = AtomicU32::new(0);
        let run = |waiter: &mut FreeWaiter, key: u64, limit: Duration| {
            waiter
                .wait_free::<()>(
                    &mut || {
                        Attempt::Busy(Busy {
                            key,
                            ..Busy::default()
                        })
                    },
                    &mut || {
                        nudges.fetch_add(1, Ordering::Relaxed);
                    },
                    &mut WaitCtl {
                        wake: None,
                        running: &|| true,
                        on_wake: &mut || {},
                        wakeup: &|| {
                            wakeups.fetch_add(1, Ordering::Relaxed);
                        },
                        deadline: Instant::now().checked_add(limit),
                        renudge: Some(Duration::from_millis(100)),
                    },
                )
                .unwrap()
        };
        let waited = run(&mut waiter, 7, Duration::from_millis(600));
        assert!(matches!(waited, FreeWait::TimedOut), "{waited:?}");
        assert_eq!(nudges.load(Ordering::Relaxed), 2, "толчок + один повторный");
        // Сон: повторный толчок (100 мс) и дедлайн -- два возврата.
        assert_eq!(wakeups.load(Ordering::Relaxed), 2);

        let mut waiter = FreeWaiter::new(Arc::clone(&free));
        nudges.store(0, Ordering::Relaxed);
        let waited = run(&mut waiter, 0, Duration::from_millis(300));
        assert!(matches!(waited, FreeWait::TimedOut), "{waited:?}");
        assert_eq!(nudges.load(Ordering::Relaxed), 0, "сессия -- без толчков");
    }

    /// Round 3 (решение AI): держатель подтверждённо умер -- после толчка
    /// ещё один через `CONFIRM_RENUDGE_DELAY` (сервер снимает claim R-O
    /// только на толчке после подтверждения смерти во времени); живой
    /// держатель -- один толчок.
    #[test]
    fn dead_holder_is_renudged_after_confirmation() {
        let free = notifier("CONFIRM");
        let nudges = AtomicU32::new(0);
        let run = |holder: (u32, u64)| {
            nudges.store(0, Ordering::Relaxed);
            let mut waiter = FreeWaiter::new(Arc::clone(&free));
            let started = Instant::now();
            let waited = waiter
                .wait_free::<()>(
                    &mut || {
                        Attempt::Busy(Busy {
                            key: 9,
                            holder: Some(holder),
                            server: None,
                        })
                    },
                    &mut || {
                        nudges.fetch_add(1, Ordering::Relaxed);
                    },
                    &mut WaitCtl {
                        wake: None,
                        running: &|| true,
                        on_wake: &mut || {},
                        wakeup: &|| {},
                        deadline: started.checked_add(CONFIRM_RENUDGE_DELAY * 2),
                        renudge: None,
                    },
                )
                .unwrap();
            assert!(matches!(waited, FreeWait::TimedOut), "{waited:?}");
            nudges.load(Ordering::Relaxed)
        };
        assert_eq!(run(dead_process()), 2, "толчок + подтверждение");
        let live = LiveChild::spawn();
        assert_eq!(run((live.pid, live.start)), 1, "живой -- один толчок");
        drop(live);
    }
}
