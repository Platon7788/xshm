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
//! **0.10 (INTEROP R19)**: маяк своего имени держит САМ каждый именованный
//! сервер — `SharedServer::start*`, `AutoServer`, лобби `DispatchServer` и
//! база `MultiServer` (`<база>_multi_beacon`, не слоты), в том же пространстве
//! имён и с той же политикой, что канал; `AutoClient`/`MultiClient` и
//! [`SharedClient::wait_for_server`](crate::SharedClient::wait_for_server)
//! ждут на нём появления сервера без опроса. Ручной `raise` приложения
//! больше не нужен (и безвреден); `Beacon` остаётся для своих сценариев
//! (ожидание нескольких серверов в одном [`wait_any`]).
//!
//! **Сервер упал, не сбросив маяк**: маяк остаётся взведённым. Клиент,
//! которому при взведённом маяке не удалось подключиться, потому что лобби
//! нет, вызывает [`Beacon::clear_if_stale`] (НЕ [`Beacon::lower`]): рядом с
//! событием живёт секция состояния `<имя>_beacon_state` с поколением и
//! личностью (PID + время создания) взводившего сервера. Живой сервер маяк
//! не теряет; маяк упавшего сбрасывается, а взвод нового сервера,
//! случившийся между проверкой и сбросом, замечается по сменившемуся
//! поколению и возвращается. До 0.10 клиент делал `lower` напрямую: при двух
//! клиентах один гасил маяк, только что взведённый новым сервером, и второй
//! засыпал навсегда (находка 11).
//!
//! Кроме именованного маяка:
//! - [`Beacon::unnamed`] — безымянное событие того же вида для сигналов
//!   внутри процесса (например, «остановись» рядом с маяком в
//!   [`Beacon::wait_any`]); по имени его не открыть и не занять заранее;
//! - [`ProcessExit`] — удерживаемый handle процесса (сигнален, когда процесс
//!   завершился); вместе с маяками ждётся через [`wait_any`].

use std::os::windows::io::{AsHandle, BorrowedHandle};
use std::time::Duration;

use crate::error::{Result, ShmError};
use crate::namespace::PrivateNamespace;
use crate::naming::ObjectPath;
use crate::security::{ObjectSecurity, SecurityAttributes};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering, fence};

use crate::win::{EventHandle, Mapping, ProcessWatch, current_process_start_time, is_process_gone};

mod sealed {
    /// Запечатывающий трейт: неименуем снаружи намеренно -- иначе
    /// `Waitable` можно было бы реализовать вне крейта.
    #[expect(
        unnameable_types,
        reason = "паттерн sealed: трейт достижим через `Waitable`, но не именуем"
    )]
    pub trait Sealed {}
}

/// Объект, который можно ждать в [`wait_any`]: [`Beacon`], [`ProcessExit`].
/// Запечатан — реализации только внутри `xshm`; дескриптор для ожидания
/// отдаёт [`AsHandle`] (заимствование живёт не дольше объекта).
#[diagnostic::on_unimplemented(
    message = "`{Self}` нельзя ждать в `xshm::wait_any`",
    label = "не реализует `xshm::Waitable`",
    note = "ждать можно только `xshm::Beacon` и `xshm::ProcessExit` (трейт запечатан)"
)]
pub trait Waitable: AsHandle + sealed::Sealed {}

/// Ждать, пока сработает любой из объектов (`None` — без таймаута).
/// `Some(i)` — индекс сработавшего (при одновременном срабатывании —
/// наименьший), `None` — таймаут или пустой список.
///
/// # Errors
///
/// Ошибка ОС.
pub fn wait_any(objects: &[&dyn Waitable], timeout: Option<Duration>) -> Result<Option<usize>> {
    let handles: Vec<BorrowedHandle<'_>> = objects.iter().map(|o| o.as_handle()).collect();
    crate::win::wait_any(&handles, timeout)
}

/// Размер секции состояния маяка (одна страница).
pub(crate) const STATE_SIZE: usize = 4096;
/// `magic` состояния маяка: `'XBCN'` (байты `58 42 43 4E`, `u32` LE).
const STATE_MAGIC: u32 = 0x4E43_4258;
/// Смещения полей состояния (`u32` LE, атомарно).
const STATE_MAGIC_OFFSET: usize = 0;
const STATE_GENERATION_OFFSET: usize = 4;
const STATE_SERVER_PID_OFFSET: usize = 8;
const STATE_SERVER_START_LO_OFFSET: usize = 12;
const STATE_SERVER_START_HI_OFFSET: usize = 16;
/// 0.10, Task 12 (INTEROP R20/R21): поколение освобождений claim-а канала
/// (`free_gen`, только `fetch_add` SeqCst / `lock xadd`) и число спящих
/// ждущих (`free_waiters`). Пишет их `presence::FreeNotifier`, `magic` не
/// требуется: это счётчики, а не личность сервера.
pub(crate) const STATE_FREE_GEN_OFFSET: usize = 20;
pub(crate) const STATE_FREE_WAITERS_OFFSET: usize = 24;

/// Маяк сервера (см. модуль).
#[derive(Debug)]
pub struct Beacon {
    event: EventHandle,
    /// Состояние маяка (0.10): секция `<имя>_beacon_state` или причина,
    /// по которой её нет.
    state: BeaconState,
    /// Частное пространство имён маяка (0.10, [`Beacon::open_in`]): держим,
    /// пока жив маяк, -- иначе с последним handle пространство исчезло бы,
    /// и пир не нашёл бы маяк.
    _namespace: Option<Arc<PrivateNamespace>>,
}

/// Состояние маяка.
#[derive(Debug)]
enum BeaconState {
    /// Безымянный маяк: сервера нет, сбрасывать «за упавшего» нечего.
    Unnamed,
    /// Именованный маяк без секции состояния: её не дали создать или
    /// открыть права (`STATUS_PRIVILEGE_NOT_HELD`/`STATUS_ACCESS_DENIED`,
    /// например `Global\` из сессии != 0 без `SeCreateGlobalPrivilege`).
    /// Поведение до 0.10: `clear_if_stale` сбрасывает без проверки сервера.
    Legacy,
    /// Секция `<имя>_beacon_state`: поколение и личность сервера,
    /// взводившего маяк последним.
    Shared(Mapping),
}

/// `NTSTATUS` `STATUS_PRIVILEGE_NOT_HELD`.
const STATUS_PRIVILEGE_NOT_HELD: u32 = 0xC000_0061;
/// `NTSTATUS` `STATUS_ACCESS_DENIED`.
const STATUS_ACCESS_DENIED: u32 = 0xC000_0022;
/// `NTSTATUS` `STATUS_OBJECT_NAME_NOT_FOUND`.
const STATUS_OBJECT_NAME_NOT_FOUND: u32 = 0xC000_0034;

/// Ошибка открытия/создания секции состояния, при которой маяк деградирует
/// до режима без состояния (`BeaconState::Legacy`), а не отказывает: только
/// нехватка привилегии или прав. Подмена объекта (другой тип, `SEC_RESERVE`,
/// маленькая секция) -- по-прежнему ошибка `open`.
const fn state_fallback(error: &ShmError) -> bool {
    match error {
        // 0.10: `STATUS_ACCESS_DENIED` приходит как `AccessDenied`
        // (`status_to_error`); код в `WindowsError` -- на всякий случай.
        ShmError::AccessDenied => true,
        ShmError::WindowsError { code, .. } => {
            *code == STATUS_PRIVILEGE_NOT_HELD || *code == STATUS_ACCESS_DENIED
        }
        _ => false,
    }
}

/// Секция состояния маяка `name`: сначала существующая (открытие не
/// требует `SeCreateGlobalPrivilege`), и только если её нет и задан
/// `security` -- создать с ним (тот же дескриптор, что у события маяка).
/// Меньше страницы -- `Corrupted`. Общая для маяка и уведомления «claim
/// освобождён» (`presence::FreeNotifier`, Task 12).
pub(crate) fn open_state_section(
    namespace: Option<&PrivateNamespace>,
    name: &str,
    security: Option<&SecurityAttributes>,
) -> Result<Mapping> {
    match Mapping::open(beacon_state_path(namespace, name)) {
        Ok(mapping) if mapping.size() >= STATE_SIZE => Ok(mapping),
        Ok(_) => Err(ShmError::Corrupted),
        Err(ShmError::WindowsError { code, .. }) if code == STATUS_OBJECT_NAME_NOT_FOUND => {
            match security {
                Some(security) => Mapping::open_or_create(
                    beacon_state_path(namespace, name),
                    STATE_SIZE,
                    security,
                ),
                None => Err(ShmError::WindowsError {
                    code,
                    context: "NtOpenSection(beacon state)",
                }),
            }
        }
        Err(other) => Err(other),
    }
}

/// Слово секции состояния `state` по смещению `offset`; `None` -- смещение
/// не кратно 4 или вне отображения.
pub(crate) fn state_word(state: &Mapping, offset: usize) -> Option<&AtomicU32> {
    if !offset.is_multiple_of(4) || offset.checked_add(4)? > state.size() {
        return None;
    }
    let ptr = state.as_ptr().wrapping_add(offset).cast::<u32>();
    // SAFETY: `ptr` -- внутри отображения (`offset + 4 <= size`) и выровнен
    // на 4 (view выровнен на страницу, `offset` кратно 4); отображение живёт,
    // пока жив `state`, то есть дольше возвращаемой ссылки (она заимствует
    // `state`). Доступ к памяти, общей с другими процессами, -- только
    // атомарный.
    Some(unsafe { AtomicU32::from_ptr(ptr) })
}

/// Открыть секцию состояния: сначала существующую, и только если её нет --
/// создать с дескриптором `security` (`open_state_section`).
fn open_state(
    namespace: Option<&PrivateNamespace>,
    name: &str,
    security: &SecurityAttributes,
) -> Result<BeaconState> {
    let attempt = open_state_section(namespace, name, Some(security));
    match attempt {
        Ok(mapping) => Ok(BeaconState::Shared(mapping)),
        Err(error) if state_fallback(&error) => Ok(BeaconState::Legacy),
        Err(error) => Err(error),
    }
}

impl sealed::Sealed for Beacon {}

impl Waitable for Beacon {}

impl AsHandle for Beacon {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.event.as_handle()
    }
}

/// Событие маяка `<base>_beacon` (namespace -- как у канала/лобби `base`;
/// 0.10: или относительное имя в частном пространстве).
fn beacon_path<'a>(namespace: Option<&'a PrivateNamespace>, base: &str) -> ObjectPath<'a> {
    ObjectPath::scoped(namespace, base, "_beacon")
}

/// Секция состояния маяка `<base>_beacon_state` (там же, где событие).
pub(crate) fn beacon_state_path<'a>(
    namespace: Option<&'a PrivateNamespace>,
    base: &str,
) -> ObjectPath<'a> {
    ObjectPath::scoped(namespace, base, "_beacon_state")
}

/// Имя события маяка вне частного пространства (тесты).
#[cfg(test)]
fn beacon_name(base: &str) -> String {
    beacon_path(None, base).name().to_owned()
}

/// Имя секции состояния маяка вне частного пространства (тесты).
#[cfg(test)]
fn beacon_state_name(base: &str) -> String {
    beacon_state_path(None, base).name().to_owned()
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
    /// ограничивающим DACL (`EVENT_ALL_ACCESS` не выдан -- 0.10:
    /// [`ShmError::AccessDenied`]): подменённый маяк не открывается.
    /// Политика создаваемых объектов -- по умолчанию (владелец + SYSTEM),
    /// другая -- [`Beacon::open_with`].
    ///
    /// 0.10: рядом с событием -- секция состояния `<имя>_beacon_state`
    /// (`SEC_COMMIT`, 4096 байт): сначала открывается существующая, и только
    /// если её нет -- создаётся (`OBJ_OPENIF`). Секция, занятая заранее
    /// объектом другого типа, `SEC_RESERVE`/файлом/образом или меньше
    /// страницы, -- ошибка (`Corrupted` или код ОС), как и подменённое
    /// событие.
    ///
    /// **Без состояния**: если секцию не дают создать или открыть права
    /// (`STATUS_PRIVILEGE_NOT_HELD` -- например, `Global\` из сессии != 0 без
    /// `SeCreateGlobalPrivilege`, пока сервер-служба её не создал;
    /// `STATUS_ACCESS_DENIED`), маяк открывается без неё и работает как до
    /// 0.10: `raise` только взводит событие, [`Beacon::clear_if_stale`]
    /// сбрасывает его без проверки сервера и поколения -- гонка нескольких
    /// клиентов с новым сервером (находка 11) в этом режиме остаётся.
    /// Решение принимается **один раз, в `open`**: маяк, открытый без
    /// состояния, не подхватывает секцию `<имя>_beacon_state`, созданную
    /// позже (например, службой с нужной привилегией), и остаётся без
    /// состояния до конца жизни объекта -- чтобы перейти на секцию,
    /// откройте маяк заново.
    pub fn open(name: &str) -> Result<Self> {
        Self::open_with(name, &ObjectSecurity::default())
    }

    /// [`Beacon::open`] с политикой безопасности `security` (0.10) для
    /// объектов, которые создаёт этот вызов: события `<имя>_beacon` и секции
    /// `<имя>_beacon_state`. Уже существующие объекты открываются по своему
    /// DACL (политику задал их создатель). [`Beacon::open`] -- политика по
    /// умолчанию ([`ObjectSecurity::CurrentUser`]): маяк службы SYSTEM на
    /// `Global\` для процессов пользователя -- [`ObjectSecurity::Everyone`]
    /// или свой дескриптор у той стороны, что создаёт маяк первой (обычно
    /// у обеих одинаковый).
    ///
    /// # Errors
    ///
    /// Как у [`Beacon::open`]; `AccessDenied` -- маяк создан другим
    /// пользователем, DACL которого этот процесс не включает (секция
    /// состояния с отказом в доступе -- не ошибка, а маяк без состояния);
    /// `InvalidConfig` -- некорректный `ObjectSecurity::Descriptor`.
    pub fn open_with(name: &str, security: &ObjectSecurity) -> Result<Self> {
        Self::open_scoped(None, name, security)
    }

    /// [`Beacon::open_with`] в частном пространстве имён `ns` (0.10, см.
    /// [`PrivateNamespace`]): событие `<имя>_beacon` и секция
    /// `<имя>_beacon_state` -- относительные имена в пространстве, их не
    /// видит и не может занять процесс вне границы. Имя -- без
    /// `Local\`/`Global\` (иначе `InvalidConfig`). Маяк держит ссылку на
    /// пространство, пока жив.
    ///
    /// # Errors
    ///
    /// Как у [`Beacon::open_with`].
    pub fn open_in(
        ns: &Arc<PrivateNamespace>,
        name: &str,
        security: &ObjectSecurity,
    ) -> Result<Self> {
        Self::open_scoped(Some(ns), name, security)
    }

    fn open_scoped(
        namespace: Option<&Arc<PrivateNamespace>>,
        name: &str,
        security: &ObjectSecurity,
    ) -> Result<Self> {
        Self::open_at(
            namespace.map(Arc::as_ref),
            namespace.cloned(),
            name,
            &security.attributes()?,
        )
    }

    /// Открыть (или создать) маяк `name` в пространстве `root` с готовым
    /// дескриптором `security` (0.10, маяк присутствия сервера и клиента --
    /// `presence`). `keep` -- ссылка на пространство, которую маяк держит,
    /// пока жив (`None` -- пространство держит вызывающий всё время жизни
    /// маяка, например на время синхронного ожидания).
    pub(crate) fn open_at(
        root: Option<&PrivateNamespace>,
        keep: Option<Arc<PrivateNamespace>>,
        name: &str,
        security: &SecurityAttributes,
    ) -> Result<Self> {
        let event = EventHandle::open_or_create_notification(beacon_path(root, name), security)?;
        let beacon = Self {
            event,
            state: open_state(root, name, security)?,
            _namespace: keep,
        };
        // Новая секция -- нули: `magic` ставит первый открывший (CAS 0 ->
        // MAGIC). Не ноль и не MAGIC -- порча: CAS не проходит, состоянию не
        // доверяем (см. `snapshot`), но и не «чиним» его.
        if let Some(magic) = beacon.state_word(STATE_MAGIC_OFFSET) {
            let _ = magic.compare_exchange(0, STATE_MAGIC, Ordering::AcqRel, Ordering::Acquire);
        }
        Ok(beacon)
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
            state: BeaconState::Unnamed,
            _namespace: None,
        })
    }

    /// Сервер работает: разбудить всех ждущих и оставить маяк взведённым.
    ///
    /// 0.10: сначала публикует в состоянии маяка свою личность (PID и время
    /// создания процесса) и увеличивает `generation`, затем `NtSetEvent` --
    /// по этому поколению клиент в [`Beacon::clear_if_stale`] замечает
    /// взвод, случившийся между его проверкой и сбросом. Испорченное
    /// состояние (`magic` неверный) не мешает взводу: публикация
    /// пропускается, событие взводится.
    ///
    /// # Errors
    ///
    /// Ошибка ОС.
    pub fn raise(&self) -> Result<()> {
        self.publish(std::process::id(), current_process_start_time());
        self.event.set()
    }

    /// Сервер уходит: сбросить маяк. **Только для сервера**: клиент, не
    /// нашедший лобби при взведённом маяке, вызывает
    /// [`Beacon::clear_if_stale`] — прямой `lower` клиента может погасить
    /// маяк нового сервера, поднявшегося за мгновение до сброса.
    ///
    /// 0.10: до `NtResetEvent` обнуляет личность сервера в состоянии маяка
    /// (`server_pid`, затем половины времени). Иначе клиент, вернувший маяк
    /// по сменившемуся поколению уже после этого `lower`, оставил бы маяк
    /// взведённым без лобби, а следующие клиенты видели бы «сервер жив» до
    /// выхода его процесса и крутились бы в опросе; с PID `0` следующий
    /// [`Beacon::clear_if_stale`] маяк сбрасывает.
    ///
    /// # Errors
    ///
    /// Ошибка ОС.
    pub fn lower(&self) -> Result<()> {
        if let (Some(pid_word), Some(lo), Some(hi)) = (
            self.trusted_word(STATE_SERVER_PID_OFFSET),
            self.trusted_word(STATE_SERVER_START_LO_OFFSET),
            self.trusted_word(STATE_SERVER_START_HI_OFFSET),
        ) {
            // PID первым: клиент читает его раньше времени, и PID 0 сразу
            // уводит его на путь «сервера нет» (время не используется).
            pid_word.store(0, Ordering::Release);
            lo.store(0, Ordering::Release);
            hi.store(0, Ordering::Release);
        }
        self.event.reset()
    }

    /// Клиент: маяк взведён, а лобби нет — погасить маяк за упавший сервер,
    /// не потеряв взвод нового.
    ///
    /// Читает `generation` и личность сервера из состояния маяка. Процесс
    /// сервера не подтверждённо мёртв (жив или нет прав проверить) —
    /// `Ok(false)`, маяк не трогается: лобби этого сервера появится или он
    /// сам сбросит маяк. Иначе — `NtResetEvent`, затем повторное чтение
    /// `generation`: изменилось (новый сервер успел взвести маяк) —
    /// `NtSetEvent` обратно. `Ok(true)` — маяк был «за упавший сервер» и
    /// обработан; клиент перепроверяет лобби и снова ждёт маяк.
    ///
    /// PID `0` (сервер не публиковал состояние: секция создана заново после
    /// ухода всех держателей, сервер штатно ушёл через [`Beacon::lower`] или
    /// пир её не пишет) — проверить нечего, поведение прежнее: сброс с
    /// перепроверкой поколения. Состояние с неверным `magic` и безымянный
    /// маяк — `Ok(false)` без сброса. Маяк без секции состояния (см.
    /// [`Beacon::open`]) — сброс без проверок, `Ok(true)`.
    ///
    /// Мусор в личности сервера при верном `magic` (PID несуществующего
    /// процесса, чужое время создания) допускает сброс: любой процесс с
    /// доступом к маяку (тот же пользователь при политике по умолчанию,
    /// любой при `Everyone`) и так может сбросить его напрямую.
    ///
    /// # Errors
    ///
    /// Ошибка ОС.
    pub fn clear_if_stale(&self) -> Result<bool> {
        match &self.state {
            BeaconState::Unnamed => Ok(false),
            BeaconState::Legacy => {
                self.event.reset()?;
                Ok(true)
            }
            BeaconState::Shared(_) => match self.snapshot() {
                Some(snapshot) => self.clear_after(snapshot),
                None => Ok(false),
            },
        }
    }

    /// Поколение взводов из состояния маяка (0.10, `presence`): меняется при
    /// каждом [`Beacon::raise`]. `None` -- маяк без состояния или состояние
    /// испорчено.
    pub(crate) fn generation(&self) -> Option<u32> {
        self.trusted_word(STATE_GENERATION_OFFSET)
            .map(|generation| generation.load(Ordering::SeqCst))
    }

    /// Личность сервера, взводившего маяк последним (PID и время создания
    /// процесса; 0.10, `presence`). `None` -- маяк без состояния, состояние
    /// испорчено или сервер ушёл штатно ([`Beacon::lower`] обнулил PID).
    pub(crate) fn server_identity(&self) -> Option<(u32, u64)> {
        self.snapshot()
            .filter(|snapshot| snapshot.server_pid != 0)
            .map(|snapshot| (snapshot.server_pid, snapshot.server_start))
    }

    /// Тесты: взвести маяк от имени процесса `(pid, start)` -- «сервер
    /// взвёл и упал».
    #[cfg(test)]
    pub(crate) fn raise_as(&self, pid: u32, start: u64) -> Result<()> {
        self.publish(pid, start);
        self.event.set()
    }

    /// Слово состояния по смещению; `None` -- безымянный маяк или смещение
    /// вне отображения (не бывает: view не меньше `STATE_SIZE`).
    fn state_word(&self, offset: usize) -> Option<&AtomicU32> {
        let BeaconState::Shared(state) = &self.state else {
            return None;
        };
        state_word(state, offset)
    }

    /// Поле доверенного состояния (`magic` верный), иначе `None`.
    fn trusted_word(&self, offset: usize) -> Option<&AtomicU32> {
        let magic = self.state_word(STATE_MAGIC_OFFSET)?;
        if magic.load(Ordering::Acquire) != STATE_MAGIC {
            return None;
        }
        self.state_word(offset)
    }

    /// Сервер: записать личность и увеличить поколение. Порядок: половины
    /// времени, затем PID (публикующее поле, как в `ControlBlock`), затем
    /// `generation` -- клиент, прочитавший новое поколение (`snapshot`),
    /// видит и личность того же или более позднего взвода.
    fn publish(&self, pid: u32, start: u64) {
        let (Some(lo), Some(hi), Some(pid_word), Some(generation)) = (
            self.trusted_word(STATE_SERVER_START_LO_OFFSET),
            self.trusted_word(STATE_SERVER_START_HI_OFFSET),
            self.trusted_word(STATE_SERVER_PID_OFFSET),
            self.trusted_word(STATE_GENERATION_OFFSET),
        ) else {
            return;
        };
        let [l0, l1, l2, l3, h0, h1, h2, h3] = start.to_le_bytes();
        lo.store(u32::from_le_bytes([l0, l1, l2, l3]), Ordering::Release);
        hi.store(u32::from_le_bytes([h0, h1, h2, h3]), Ordering::Release);
        pid_word.store(pid, Ordering::Release);
        // SeqCst: `generation` -- точка линеаризации против перечитывания
        // клиентом после `NtResetEvent` (`clear_after`); `NtSetEvent` идёт
        // строго после.
        generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Снимок состояния: поколение первым, затем личность сервера. `None` --
    /// безымянный маяк или испорченное состояние.
    fn snapshot(&self) -> Option<StateSnapshot> {
        let generation = self
            .trusted_word(STATE_GENERATION_OFFSET)?
            .load(Ordering::SeqCst);
        let server_pid = self
            .trusted_word(STATE_SERVER_PID_OFFSET)?
            .load(Ordering::Acquire);
        let lo = self
            .trusted_word(STATE_SERVER_START_LO_OFFSET)?
            .load(Ordering::Acquire);
        let hi = self
            .trusted_word(STATE_SERVER_START_HI_OFFSET)?
            .load(Ordering::Acquire);
        Some(StateSnapshot {
            generation,
            server_pid,
            server_start: (u64::from(hi) << 32) | u64::from(lo),
        })
    }

    /// Вторая половина `clear_if_stale` по готовому снимку (тест
    /// интерлинии вставляет взвод нового сервера между ними).
    fn clear_after(&self, snapshot: StateSnapshot) -> Result<bool> {
        if snapshot.server_pid != 0 && !is_process_gone(snapshot.server_pid, snapshot.server_start)
        {
            return Ok(false);
        }
        let Some(generation) = self.trusted_word(STATE_GENERATION_OFFSET) else {
            return Ok(false);
        };
        self.event.reset()?;
        // Сброс (системный вызов) -- до перечитывания поколения: новый
        // сервер увеличивает `generation` до `NtSetEvent`, поэтому либо его
        // инкремент виден здесь (вернём маяк сами), либо его `NtSetEvent`
        // идёт после нашего сброса. SeqCst-барьер запрещает поднять чтение
        // выше сброса.
        fence(Ordering::SeqCst);
        if generation.load(Ordering::SeqCst) != snapshot.generation {
            self.event.set()?;
        }
        Ok(true)
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
        let handles: Vec<BorrowedHandle<'_>> = beacons.iter().map(|b| b.as_handle()).collect();
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

/// Снимок состояния маяка.
#[derive(Debug, Clone, Copy)]
struct StateSnapshot {
    generation: u32,
    server_pid: u32,
    server_start: u64,
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

impl Waitable for ProcessExit {}

impl AsHandle for ProcessExit {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.watch.as_handle()
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
    use std::sync::atomic::AtomicUsize;

    /// Политика по умолчанию для объектов тестов.
    fn sec() -> SecurityAttributes {
        ObjectSecurity::default().attributes().unwrap()
    }

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

    use crate::test_support::dead_process;

    /// Находка 11: детерминированная интерлиния «клиент гасит маяк нового
    /// сервера». Старый сервер взвёл маяк и упал; клиент A прочитал
    /// состояние (сервер мёртв); новый сервер поднялся и взвёл маяк; A
    /// сбрасывает маяк «за упавший» -- и обязан заметить новое поколение и
    /// вернуть маяк. Иначе клиент B, пришедший позже, спит вечно.
    #[test]
    fn stale_clear_racing_new_raise_keeps_beacon_raised() {
        let name = unique("race");
        let (dead_pid, dead_start) = dead_process();
        // Клиент A держит маяк с самого начала: иначе с последним handle
        // исчезли бы и событие, и состояние.
        let client_a = Beacon::open(&name).unwrap();
        let old_server = Beacon::open(&name).unwrap();
        old_server.publish(dead_pid, dead_start);
        old_server.event.set().unwrap();
        drop(old_server); // «упал», не сбросив маяк

        assert!(client_a.is_raised().unwrap());
        let snapshot = client_a.snapshot().expect("состояние маяка");

        let new_server = Beacon::open(&name).unwrap();
        new_server.raise().unwrap();

        assert!(
            client_a.clear_after(snapshot).unwrap(),
            "сервер снимка мёртв"
        );
        assert!(
            client_a.is_raised().unwrap(),
            "маяк нового сервера погашен -- клиент B уснёт навсегда"
        );
        let client_b = Beacon::open(&name).unwrap();
        assert!(client_b.wait(Some(Duration::ZERO)).unwrap());
        drop(new_server);
    }

    /// Сервер жив -- `clear_if_stale` маяк не трогает.
    #[test]
    fn clear_if_stale_keeps_live_server() {
        let name = unique("live");
        let server = Beacon::open(&name).unwrap();
        server.raise().unwrap();
        let client = Beacon::open(&name).unwrap();
        assert!(!client.clear_if_stale().unwrap(), "сервер жив");
        assert!(client.is_raised().unwrap());
    }

    /// Сервер упал (процесса нет) -- клиент гасит маяк.
    #[test]
    fn clear_if_stale_lowers_beacon_of_dead_server() {
        let name = unique("dead");
        let (dead_pid, dead_start) = dead_process();
        let server = Beacon::open(&name).unwrap();
        server.publish(dead_pid, dead_start);
        server.event.set().unwrap();
        let client = Beacon::open(&name).unwrap();
        assert!(client.clear_if_stale().unwrap());
        assert!(!client.is_raised().unwrap());
    }

    /// Порча состояния (`magic` неверный, мусор в полях): без паники, маяк
    /// не трогается -- доверять нечему.
    #[test]
    fn corrupted_state_never_lowers_beacon() {
        let name = unique("corrupt");
        let (dead_pid, dead_start) = dead_process();
        let server = Beacon::open(&name).unwrap();
        server.publish(dead_pid, dead_start);
        server.event.set().unwrap();
        let client = Beacon::open(&name).unwrap();
        client
            .state_word(STATE_MAGIC_OFFSET)
            .unwrap()
            .store(0xDEAD_BEEF, Ordering::Release);
        assert!(!client.clear_if_stale().unwrap(), "неверный magic");
        assert!(client.is_raised().unwrap());
        // Сервер с испорченным состоянием всё равно взводит маяк.
        server.lower().unwrap();
        server.raise().unwrap();
        assert!(client.is_raised().unwrap());
    }

    /// Ревью T6 (важное 2): клиент A снял снимок при мёртвом S1; новый S2
    /// взвёл маяк и штатно ушёл (`lower`), но его процесс жив; A сбросил
    /// маяк и по сменившемуся поколению вернул его -- маяк взведён, лобби
    /// нет. Следующий `clear_if_stale` обязан его сбросить (`lower` обнулил
    /// личность сервера -> путь PID 0), а не отвечать «сервер жив» до выхода
    /// процесса S2 (клиенты крутились бы в опросе).
    #[test]
    fn lowered_server_does_not_pin_beacon_raised() {
        let name = unique("lowered");
        let (dead_pid, dead_start) = dead_process();
        let client_a = Beacon::open(&name).unwrap();
        let old_server = Beacon::open(&name).unwrap();
        old_server.publish(dead_pid, dead_start);
        old_server.event.set().unwrap();
        drop(old_server);

        let snapshot = client_a.snapshot().expect("состояние маяка");
        let new_server = Beacon::open(&name).unwrap();
        new_server.raise().unwrap();
        new_server.lower().unwrap(); // процесс S2 (этот тест) жив

        assert!(client_a.clear_after(snapshot).unwrap());
        assert!(client_a.is_raised().unwrap(), "A вернул маяк по поколению");

        let client_b = Beacon::open(&name).unwrap();
        assert!(
            client_b.clear_if_stale().unwrap(),
            "сервер ушёл штатно -- маяк без сервера сбрасывается"
        );
        assert!(!client_b.is_raised().unwrap());
        drop(new_server);
    }

    /// Ревью T6 (мелкое 8): верный `magic`, мусор в личности сервера --
    /// без паники; документировано, что такой мусор допускает сброс (любой
    /// процесс с доступом к маяку и так может сбросить его).
    #[test]
    fn garbage_identity_with_valid_magic_allows_reset_without_panic() {
        let name = unique("garbage");
        let server = Beacon::open(&name).unwrap();
        let client = Beacon::open(&name).unwrap();
        // Свой (живой) PID, но чужое время создания -- «PID переиспользован».
        let real = current_process_start_time();
        server.publish(std::process::id(), real ^ 0x5A5A_5A5A);
        server.event.set().unwrap();
        assert!(client.clear_if_stale().unwrap());
        assert!(!client.is_raised().unwrap());
        // Произвольный мусор: результат любой, но без паники и без ошибки.
        server.publish(0xFFFF_FFF0, u64::MAX);
        server.event.set().unwrap();
        assert!(client.clear_if_stale().is_ok());
    }

    /// Ревью T6 (мелкое 8): `<имя>_beacon_state` занят событием (объект
    /// другого типа) -- `open` отвергает маяк (`STATUS_OBJECT_TYPE_MISMATCH`
    /// не входит в деградацию до режима без состояния).
    #[test]
    fn open_rejects_state_squatted_by_other_object_type() {
        let name = unique("state-squat");
        let _squatter = EventHandle::create(beacon_state_name(&name), &sec()).unwrap();
        match Beacon::open(&name) {
            Err(ShmError::WindowsError { code, .. }) => {
                assert_eq!(code, 0xC000_0024, "STATUS_OBJECT_TYPE_MISMATCH");
            }
            other => panic!("ожидался отказ по типу объекта, получено {other:?}"),
        }
    }

    /// Ревью T6 (важное 3): деградация до режима без состояния -- только
    /// на нехватку привилегии/прав (несоздаваемый `Global\` из сессии != 0),
    /// всё остальное -- ошибка `open`.
    #[test]
    fn state_fallback_only_on_privilege_or_access_denied() {
        let err = |code: u32| ShmError::WindowsError {
            code,
            context: "test",
        };
        assert!(
            state_fallback(&err(0xC000_0061)),
            "STATUS_PRIVILEGE_NOT_HELD"
        );
        assert!(state_fallback(&err(0xC000_0022)), "STATUS_ACCESS_DENIED");
        // 0.10: отказ в доступе приходит отдельным вариантом.
        assert!(state_fallback(&ShmError::AccessDenied), "AccessDenied");
        assert!(
            !state_fallback(&err(0xC000_0024)),
            "STATUS_OBJECT_TYPE_MISMATCH"
        );
        assert!(
            !state_fallback(&err(0xC000_0034)),
            "STATUS_OBJECT_NAME_NOT_FOUND"
        );
        assert!(!state_fallback(&ShmError::Corrupted));
    }

    /// Маяк без состояния (деградация) ведёт себя как до 0.10: `raise`
    /// взводит, `clear_if_stale` сбрасывает без проверки сервера.
    #[test]
    fn stateless_named_beacon_uses_legacy_clear() {
        let name = unique("legacy");
        let server = Beacon::open(&name).unwrap();
        let client = Beacon {
            event: EventHandle::open_or_create_notification(beacon_name(&name), &sec()).unwrap(),
            state: BeaconState::Legacy,
            _namespace: None,
        };
        server.raise().unwrap();
        assert!(
            client.clear_if_stale().unwrap(),
            "legacy: сброс без проверки"
        );
        assert!(!server.is_raised().unwrap());
        client.raise().unwrap();
        assert!(server.is_raised().unwrap());
        // Безымянный маяк сервера не имеет -- сбрасывать нечего.
        let unnamed = Beacon::unnamed().unwrap();
        unnamed.raise().unwrap();
        assert!(!unnamed.clear_if_stale().unwrap());
        assert!(unnamed.is_raised().unwrap());
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
        let _squatter = EventHandle::create(beacon_name(&name), &sec()).unwrap();
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
        // Процесс уже завершён: `wait` только забирает код выхода.
        drop(child.wait());
    }
}
