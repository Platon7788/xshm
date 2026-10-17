use std::cell::Cell;
use std::future::Future;
use std::marker::PhantomData;
use std::os::windows::io::{AsHandle, BorrowedHandle};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use crate::asynch::{ANONYMOUS, Endpoint, OP, RecvFuture, SendFuture, WaitSet};
use crate::constants::{
    CLAIM_FREE, HANDSHAKE_CLIENT_HELLO, HANDSHAKE_IDLE, HANDSHAKE_SERVER_READY,
};
use crate::error::{Result, ShmError};
use crate::events::SharedEvents;
use crate::layout::ChannelLayout;
use crate::namespace::PrivateNamespace;
use crate::naming::ObjectPath;
use crate::presence::{Presence, ServerPresence};
use crate::ring::{
    DisconnectWatch, DrainBound, FreeSpace, RingBuffer, SpinPolicy, UnreadProbe, WriteOutcome,
    deadline_of, spin_until,
};
use crate::security::{ObjectSecurity, SecurityAttributes};
use crate::shared::SharedView;
use crate::win::{EventHandle, Mapping, ProcessWatch, wait_any};

/// Тесты (Task 22): точка рукопожатия сервера, в которой срабатывает
/// `HANDSHAKE_HOOK`.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HandshakePoint {
    /// Между публикацией `generation`/`SERVER_READY` (S3, S4) и фиксацией S5.
    BeforeCommit,
    /// Между фиксацией S5 и сигналом `S2C_CONNECT` (S6, S7).
    AfterCommit,
    /// S2: заявка признана заявкой умершего, перед перепроверкой claim и CAS.
    BeforeDeadWithdraw,
    /// S2: CAS `CLIENT_HELLO -> IDLE` удался, claim умершего ещё не снят.
    AfterDeadWithdraw,
}

/// Тесты: хук рукопожатия -- получает отображение сервера.
#[cfg(test)]
pub(crate) type HandshakeHook = Box<dyn FnOnce(&SharedView)>;

#[cfg(test)]
thread_local! {
    /// Тесты (Task 22): одноразовый хук в точке `HandshakePoint` внутри
    /// `complete_handshake` -- играет роль клиента, успевшего в этом окне
    /// (G1, G2 дизайна). В release ни кода, ни TLS нет.
    pub(crate) static HANDSHAKE_HOOK: core::cell::RefCell<Option<(HandshakePoint, HandshakeHook)>> =
        const { core::cell::RefCell::new(None) };
}

#[cfg(test)]
thread_local! {
    /// Тесты: следующий сигнал `S2C_CONNECT` (S7) этого потока завершится
    /// ошибкой, как отказ `NtSetEvent` (одноразово).
    pub(crate) static FAIL_CONNECT_ACK: Cell<bool> = const { Cell::new(false) };
}

/// Тесты (Task 7): одноразовый отказ S7 канала, чей `ControlBlock` лежит по
/// этому адресу, -- на любом потоке (рукопожатие слота Multi идёт на
/// потоке-диспетчере, TLS `FAIL_CONNECT_ACK` туда не достаёт). `0` -- нет.
#[cfg(test)]
static FAIL_CONNECT_ACK_AT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Тесты: следующий сигнал `S2C_CONNECT` (S7) канала `view` -- на любом
/// потоке -- завершится ошибкой, как отказ `NtSetEvent` (одноразово).
#[cfg(test)]
pub(crate) fn fail_connect_ack_once(view: &SharedView) {
    FAIL_CONNECT_ACK_AT.store(
        core::ptr::from_ref(view.control_block()).addr(),
        Ordering::Release,
    );
}

/// Тесты (Task 10): метка «противника, всегда выигрывающего гонку» в
/// `ControlBlock::slot_count` канала (поле -- только подсказка Multi;
/// single-client и лобби его не читают). Пока метка стоит, цикл,
/// поглотивший пустой сигнал пира, тут же взводит его снова
/// (`adversary_resignal`) -- худший порядок потока пустых сигналов,
/// воспроизводимый без гонки потоков. Метка в самой памяти канала:
/// работает на любом потоке и не задевает соседние тесты.
#[cfg(test)]
pub(crate) const ADVERSARY_MARK: u32 = 0xAD5E_A5E1;

/// Тесты (Task 10): взвести `event` снова, если в канале `view` стоит
/// `ADVERSARY_MARK`.
#[cfg(test)]
pub(crate) fn adversary_resignal(view: &SharedView, event: &EventHandle) {
    if view.control_block().slot_count.load(Ordering::Acquire) == ADVERSARY_MARK {
        let _ = event.set();
    }
}

/// S7: сигнал `S2C_CONNECT` (в тестах -- с инъекцией отказа).
#[cfg_attr(
    not(test),
    expect(unused_variables, reason = "`view` нужен только тестовой инъекции")
)]
fn signal_connect_ack(view: &SharedView, events: &SharedEvents) -> Result<()> {
    #[cfg(test)]
    if FAIL_CONNECT_ACK.with(Cell::take)
        || FAIL_CONNECT_ACK_AT
            .compare_exchange(
                core::ptr::from_ref(view.control_block()).addr(),
                0,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    {
        return Err(ShmError::WindowsError {
            code: 0xC000_0001,
            context: "NtSetEvent (injected)",
        });
    }
    events.connect_ack.set()
}

/// Вызвать тестовый хук рукопожатия, если он назначен на точку `at`.
#[cfg(test)]
fn handshake_hook(view: &SharedView, at: HandshakePoint) {
    let hook = HANDSHAKE_HOOK.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_ref().is_some_and(|(point, _)| *point == at) {
            slot.take().map(|(_, hook)| hook)
        } else {
            None
        }
    });
    if let Some(hook) = hook {
        hook(view);
    }
}

/// Держатель claim-а подтверждённо умер, и это его заявка (S2/S6 ревизии
/// 3): PID заявки `pid` ненулевой и совпадает с PID владельца claim-а
/// (`slot_owner_pid`; иначе PID прочитан у прежнего клиента -- ABA G2), а
/// процесс подтверждённо ушёл (`is_process_gone`) и по времени заявки, и
/// по времени владельца.
///
/// Отклонение от дизайна §2.2 S2: дизайн сверял только время заявки
/// (`client_start`). Его клиент заявляет сам, и живой клиент с неверным
/// временем (подмена, `FORGED_START_TIME` в тестах) снимался бы как
/// умерший -- вместо прежнего поведения «подключён без наблюдения»
/// (`peer_tests::client_with_forged_start_time_is_reported_dead`). Время
/// владельца пишет держатель claim-а из настоящего времени своего процесса
/// (C2) -- по нему живой процесс «умершим» не станет.
fn claim_holder_gone(control: &crate::layout::ControlBlock, pid: u32, start: u64) -> bool {
    pid != 0
        && control.slot_owner_pid.load(Ordering::Acquire) == pid
        && crate::win::is_process_gone(pid, start)
        && crate::win::is_process_gone(pid, control.owner_start_time())
}

/// «Задержавшийся владелец» (Task 7): handle процесса клиента сессии,
/// закрытой сервером, и токен claim-а этой сессии. Клиент, не заметивший
/// `DISCONNECT` и умерший, сам claim не снимет -- по подтверждённой смерти
/// его снимает сервер (`release_if_holder_dead`).
pub(crate) type Lingering = (ProcessWatch, u32);

/// Снять claim подтверждённо умершего держателя (Task 7, INTEROP «Изменения
/// 0.10» п. 32): процесс `watch` завершился (handle сигнален; личность
/// процесса сверена по времени создания при открытии -- `open_peer`), он же
/// владелец claim-а (`slot_owner_pid == pid`), а claim -- всё ещё токен его
/// сессии `token`, -- одним CAS `token -> 0`. `true` -- claim снят.
///
/// Без ABA: токены уникальны в процессе (`claim_token`), новый держатель
/// мог взять claim только после его освобождения (тогда CAS не совпадёт), а
/// живой процесс не бывает «умершим» -- сигнальный handle окончателен.
pub(crate) fn release_if_holder_dead(
    control: &crate::layout::ControlBlock,
    watch: &ProcessWatch,
    token: u32,
) -> bool {
    token != CLAIM_FREE
        && watch.has_exited()
        && control.slot_owner_pid.load(Ordering::Acquire) == watch.pid()
        && control
            .slot_claim
            .compare_exchange(token, CLAIM_FREE, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
}

/// Протухший захват single-client канала (ревизия 3, §2.3 дизайна, R18):
/// вызывается только по `CONNECT_REQ` без заявки (толчок проигравшего
/// клиента, C1) у неподключённого сервера -- таймера нет, ожидания остаются
/// без таймаута («ноль пробуждений в простое»).
///
/// Claim без заявки впервые увиден -- запомнить токен и момент; тот же
/// токен виден не раньше `reserve` спустя, а заявки так и нет, -- отнять
/// CAS-ом (держатель умер между захватом и `CLIENT_HELLO` или, «не
/// заметив» отключения сервером, держит канал). Новый токен -- новый отсчёт.
///
/// Task 7 (INTEROP «Изменения 0.10» п. 32): сначала -- «задержавшийся
/// владелец» `lingering` (клиент сессии, закрытой сервером): подтверждённо
/// умер и claim всё ещё его -- снять сразу (`release_if_holder_dead`), не
/// дожидаясь `reserve`; claim уже не его токен (клиент ушёл сам, L) --
/// наблюдение больше не нужно.
fn observe_idle_claim(
    view: &SharedView,
    seen: &mut Option<(u32, Instant)>,
    lingering: &mut Option<Lingering>,
    reserve: Duration,
) {
    let control = view.control_block();
    if let Some((watch, token)) = lingering.as_ref()
        && (control.slot_claim.load(Ordering::Acquire) != *token
            || release_if_holder_dead(control, watch, *token))
    {
        *lingering = None;
    }
    let claim = control.slot_claim.load(Ordering::Acquire);
    if claim == CLAIM_FREE {
        *seen = None;
        return;
    }
    let now = Instant::now();
    match *seen {
        Some((token, at)) if token == claim => {
            if now.saturating_duration_since(at) >= reserve
                && control.client_state.load(Ordering::Acquire) != HANDSHAKE_CLIENT_HELLO
            {
                // CAS: если claim успел смениться, это уже другой захват.
                let _ = control.slot_claim.compare_exchange(
                    claim,
                    CLAIM_FREE,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                *seen = None;
            }
        }
        _ => *seen = Some((claim, now)),
    }
}

/// Индекс `CONNECT_REQ` в наборе `wait_for_client_or` (`[wake, CONNECT_REQ]`,
/// Task 10, INTEROP §e.1).
const ACCEPT_CONNECT_REQ: usize = 1;

/// Дедлайн ожидания клиента: повторные ожидания после ложного
/// `CONNECT_REQ` получают остаток исходного таймаута, а не новый целиком.
struct Deadline(Option<Instant>);

impl Deadline {
    /// `None` -- без таймаута; таймаут, не представимый как `Instant`
    /// (переполнение), -- тоже без таймаута (ждать «вечно» и так).
    fn new(timeout: Option<Duration>) -> Self {
        Self(timeout.and_then(|t| Instant::now().checked_add(t)))
    }

    /// Остаток до дедлайна для очередного ожидания (`None` -- без таймаута).
    /// Истёкший дедлайн даёт ноль: ожидание вернётся сразу (опрос).
    fn remaining(&self) -> Option<Duration> {
        self.0
            .map(|at| at.saturating_duration_since(Instant::now()))
    }
}

/// Параметры канала для [`SharedServer::start_with`]: раскладка, политика
/// безопасности объектов и частное пространство имён.
///
/// `#[non_exhaustive]`: строится от `ChannelConfig::default()` (раскладка
/// `ChannelLayout::DEFAULT`, политика `ObjectSecurity::CurrentUser`, без
/// пространства) и методами [`ChannelConfig::layout`]/
/// [`ChannelConfig::security`]/[`ChannelConfig::namespace`]; новые
/// параметры добавляются без поломки вызывающих.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ChannelConfig {
    /// Раскладка канала (размеры колец, лимиты по направлениям).
    pub layout: ChannelLayout,
    /// Кто получает доступ к секции и событиям канала.
    pub security: ObjectSecurity,
    /// Частное пространство имён канала (0.10): `Some` -- секция и события
    /// создаются в нём под относительными именами (имя канала -- без
    /// `Local\`/`Global\`, иначе `InvalidConfig`), сервер держит ссылку на
    /// пространство, пока жив. `None` (по умолчанию) -- общий
    /// `\BaseNamedObjects`, как до 0.10.
    pub namespace: Option<Arc<PrivateNamespace>>,
}

impl ChannelConfig {
    /// Параметры с раскладкой `layout` и политикой по умолчанию.
    #[must_use]
    pub fn new(layout: ChannelLayout) -> Self {
        Self {
            layout,
            security: ObjectSecurity::default(),
            namespace: None,
        }
    }

    /// Заменить раскладку.
    #[must_use]
    pub const fn layout(mut self, layout: ChannelLayout) -> Self {
        self.layout = layout;
        self
    }

    /// Заменить политику безопасности.
    #[must_use]
    pub fn security(mut self, security: ObjectSecurity) -> Self {
        self.security = security;
        self
    }

    /// Создавать объекты канала в частном пространстве имён `namespace`
    /// (0.10, см. [`PrivateNamespace`]). Клиент подключается через
    /// [`SharedClient::connect_in`](crate::SharedClient::connect_in) с
    /// пространством той же границы.
    #[must_use]
    pub fn namespace(mut self, namespace: Arc<PrivateNamespace>) -> Self {
        self.namespace = Some(namespace);
        self
    }
}

/// Сервер single-client канала.
///
/// Одна сторона SPSC-канала: тип `Send`, но НЕ `Sync` -- им пользуется один
/// поток за раз (компилятор не даст поделить `&SharedServer` между
/// потоками). Для работы из нескольких потоков -- `AutoServer`/
/// `MultiServer`/`DispatchServer` (свой worker и очередь отправки) или
/// обёртка в `Mutex<SharedServer>`.
///
/// ```compile_fail,E0277
/// fn share(server: &xshm::SharedServer) {
///     std::thread::scope(|s| {
///         s.spawn(|| server.try_send_to_client(b"a"));
///         s.spawn(|| server.try_send_to_client(b"b"));
///     });
/// }
/// ```
#[derive(Debug)]
pub struct SharedServer {
    // Порядок полей важен для Drop: кольца и view ссылаются на память
    // `_mapping`, поэтому он объявлен последним и размапливается последним.
    view: SharedView,
    events: Option<SharedEvents>, // None для anonymous режима
    ring_tx: RingBuffer,
    ring_rx: RingBuffer,
    connected: bool,
    /// Удерживаемый handle процесса клиента (если клиент 0.8+ передал PID и
    /// процесс удалось открыть) -- для детекции его смерти.
    peer: Option<ProcessWatch>,
    /// Время создания процесса клиента, сообщённое им в рукопожатии и
    /// сверенное при открытии `peer` (`0` -- клиент время не сообщил, сверки
    /// не было; см. `verified_peer_pid`).
    peer_start: u64,
    /// Ревизия 3: сервер single-client канала (`start_with` и каналы
    /// Auto/Dispatch, не Multi-слот и не anonymous) -- освобождает claim
    /// умершего клиента и отнимает протухший захват по толчку.
    exclusive: bool,
    /// Claim (токен) клиента текущей сессии, прочитанный при фиксации
    /// рукопожатия (S6); `CLAIM_FREE` -- клиент без claim.
    session_claim: u32,
    /// Не подключён: чужой claim без заявки и момент его ПЕРВОГО наблюдения
    /// (отъём -- не раньше `reserve_timeout` от него, §2.3 дизайна).
    claim_seen: Option<(u32, Instant)>,
    /// Task 7: клиент сессии, закрытой сервером (`mark_disconnected` при
    /// живом процессе и claim-е сессии), -- handle его процесса и токен.
    /// Single-client: разбирается на толчке (`observe_idle_claim`), без
    /// новых ожиданий; Multi забирает его в слот (`take_lingering`). Не
    /// больше одного; сбрасывается новой сессией и `Drop`.
    lingering: Option<Lingering>,
    /// Срок протухшего захвата (`RESERVE_TIMEOUT`; тесты подменяют).
    reserve_timeout: Duration,
    /// Маяк присутствия `<имя>_beacon` (0.10, R19): взведён после публикации
    /// канала, сбрасывается первым шагом `Drop`. `None` -- anonymous,
    /// выделенный канал Dispatch, слот Multi (маяк у базы).
    presence: Option<ServerPresence>,
    /// Состояние маяка присутствия (`presence()`, решение W(1)).
    presence_status: crate::PresenceStatus,
    /// Тесты: пробуждения ожиданий `wait_for_client*` (включая внутренние
    /// повторы после `CONNECT_REQ` без заявки).
    #[cfg(test)]
    wait_wakeups: Arc<std::sync::atomic::AtomicU64>,
    _mapping: Mapping,
    /// Частное пространство имён канала (0.10): держим, пока живы объекты
    /// -- с последним handle пространство исчезает, и клиент не нашёл бы
    /// канал. Последним: закрывается после секции и событий.
    _namespace: Option<Arc<PrivateNamespace>>,
    /// Делает тип `!Sync` (0.10, нулевой размер): кольцо -- SPSC, и
    /// `&self`-методы (`try_send_*`, `send_*`, `receive_*`) из двух потоков
    /// одновременно были бы гонкой данных на одной стороне канала. `Send`
    /// сохраняется (см. `unsafe impl Send` ниже): владение можно передать
    /// другому потоку, делить -- нельзя.
    _not_sync: PhantomData<Cell<()>>,
}

// SAFETY: все поля либо владеющие (`Mapping`), либо синхронизируются через
// атомарные операции в shared memory; привязки к потоку-создателю нет.
unsafe impl Send for SharedServer {}

impl SharedServer {
    /// Сервер канала `name` с раскладкой по умолчанию (`ChannelLayout::DEFAULT`).
    pub fn start(name: &str) -> Result<Self> {
        Self::start_with(name, &ChannelConfig::default())
    }

    /// Сервер канала `name` с параметрами `config`: раскладка каждого
    /// направления и политика безопасности объектов канала (секция и 7
    /// событий).
    ///
    /// Клиент подстраивается сам: раскладка записана в сегменте
    /// (`SharedClient::layout`). `Err(InvalidConfig)` -- `config.layout` не
    /// прошёл `ChannelLayout::validate` или `ObjectSecurity::Descriptor`
    /// некорректен (оба -- до создания объектов).
    ///
    /// ```no_run
    /// use xshm::{ChannelConfig, ChannelLayout, ObjectSecurity, RingConfig, SharedServer};
    ///
    /// let config = ChannelConfig::default()
    ///     .layout(ChannelLayout::symmetric(RingConfig::new(64 << 10)))
    ///     .security(ObjectSecurity::Everyone); // служба SYSTEM <-> пользователь
    /// let server = SharedServer::start_with("Global\\MyService", &config)?;
    /// # Ok::<(), xshm::ShmError>(())
    /// ```
    ///
    /// **Маяк присутствия** (0.10, INTEROP R19): рядом с каналом сервер
    /// держит маяк `<имя>_beacon` (событие-уведомление + секция состояния
    /// `<имя>_beacon_state`, см. [`Beacon`](crate::Beacon)) в том же
    /// пространстве имён и с той же политикой `config.security`. Он
    /// взводится после публикации секции и всех событий и сбрасывается
    /// первым шагом `Drop`: клиент, стартовавший раньше сервера
    /// (`AutoClient`, `MultiClient`, [`SharedClient::wait_for_server`](
    /// crate::SharedClient::wait_for_server)), спит на нём в ядре, а не
    /// опрашивает. Маяк -- лучшее усилие (решение W(1)): занят объектом
    /// другого типа или с чужим DACL -- старт всё равно удаётся, сервер
    /// работает без маяка ([`SharedServer::presence`] --
    /// `PresenceStatus::Unavailable`), клиенты находят его резервной
    /// проверкой `discovery_fallback` (по умолчанию -- в пределах 10 с).
    pub fn start_with(name: &str, config: &ChannelConfig) -> Result<Self> {
        Self::start_slot(
            config.namespace.as_ref(),
            name,
            config.layout,
            0,
            &config.security.attributes()?,
            Presence::On,
        )
    }

    /// Сервер канала `name`, публикующий в `ControlBlock::slot_count` число
    /// `slot_count` (Multi: `max_clients`; `0` -- не Multi). Дескриптор
    /// `security` -- общий для секции, всех событий канала и маяка
    /// присутствия (Multi строит его один раз на все слоты). `presence` --
    /// держать ли маяк присутствия `<имя>_beacon` (R19; слоты Multi и
    /// выделенные каналы Dispatch -- `Off`).
    pub(crate) fn start_slot(
        namespace: Option<&Arc<PrivateNamespace>>,
        name: &str,
        layout: ChannelLayout,
        slot_count: u32,
        security: &SecurityAttributes,
        presence: Presence,
    ) -> Result<Self> {
        layout.validate()?;
        let root = namespace.map(Arc::as_ref);
        let mapping = Mapping::create(
            ObjectPath::mapping(root, name),
            layout.segment_size(),
            security,
        )?;
        let mut server = Self::from_mapping(mapping, layout, slot_count)?;
        // Ревизия 3: не Multi-слот (claim слота ведёт диспетчер Multi).
        server.exclusive = slot_count == 0;
        server.events = Some(SharedEvents::create(root, name, security)?);
        server._namespace = namespace.cloned();
        if presence == Presence::On {
            // R19: взвод -- строго ПОСЛЕ публикации секции (`magic` в
            // `from_mapping`) и всех 7 событий: клиент, проснувшийся по
            // маяку, не получает `NOT_FOUND`. Лучшее усилие (решение W(1)):
            // занятый или недоступный маяк старт не отказывает -- сервер
            // работает без него (`presence()`), клиенты находят его резервной
            // проверкой `discovery_fallback`.
            let (beacon, status) = ServerPresence::start(namespace, name, security);
            server.presence = beacon;
            server.presence_status = status;
        }
        Ok(server)
    }

    /// Маяк присутствия этого сервера (0.10, INTEROP R19): `Active` -- взведён;
    /// `Disabled` -- маяка нет по устройству (anonymous, выделенный канал
    /// Dispatch, слот Multi); `Unavailable(причина)` -- маяк не создан или не
    /// взведён (имя `<имя>_beacon` занято объектом другого типа, DACL маяка,
    /// созданного клиентом другого пользователя первым, не включает этот
    /// процесс, нет привилегии): сервер работает, клиенты находят его
    /// резервной проверкой `discovery_fallback` (по умолчанию -- в пределах
    /// 10 с), а не сразу.
    #[must_use]
    pub fn presence(&self) -> crate::PresenceStatus {
        self.presence_status.clone()
    }

    /// Создание anonymous сервера без имени (только через handle)
    ///
    /// Anonymous сервер создает section без имени в глобальном namespace.
    /// Доступ к section возможен только через handle, что идеально для
    /// передачи handle в kernel driver. Events не создаются, используется
    /// polling-based handshake через `wait_for_client_noevent()`.
    ///
    /// **ВАЖНО:** Это НЕ то же самое, что `start("")` (пустая строка).
    /// Windows NT API требует именно `ObjectName = NULL` в `OBJECT_ATTRIBUTES`
    /// для создания anonymous section. Пустая строка через `NtName::new("")`
    /// создаст `UNICODE_STRING` с путем `"\\BaseNamedObjects\\"`, что является
    /// именованной секцией, а не anonymous. Поэтому нужна отдельная функция.
    pub fn start_anonymous() -> Result<Self> {
        Self::start_anonymous_with_layout(ChannelLayout::DEFAULT)
    }

    /// Anonymous-сервер с заданной раскладкой (см. `start_anonymous`).
    /// Драйвер читает раскладку из `LayoutBlock` сегмента (INTEROP §c).
    ///
    /// 0.10: дескриптор секции -- политика по умолчанию
    /// (`ObjectSecurity::CurrentUser`) вместо NULL DACL. По имени секцию не
    /// открыть; дубликат handle получает права исходного handle без
    /// проверки DACL, поэтому политика здесь не настраивается.
    pub fn start_anonymous_with_layout(layout: ChannelLayout) -> Result<Self> {
        layout.validate()?;
        let security = ObjectSecurity::default().attributes()?;
        let mapping = Mapping::create_anonymous(layout.segment_size(), &security)?;
        Self::from_mapping(mapping, layout, 0)
    }

    /// Инициализация свежей секции: сегмент публикуется (`magic`) до
    /// создания событий -- клиент, открывший секцию раньше событий, получит
    /// ошибку открытия событий и повторит попытку.
    fn from_mapping(mapping: Mapping, layout: ChannelLayout, slot_count: u32) -> Result<Self> {
        // SAFETY: `Mapping` только что создал отображение размером не меньше
        // `layout.segment_size()` (адрес выровнен на гранулу выделения) и
        // держит его живым, пока жив сам, а он переезжает в возвращаемую
        // структуру; layout проверен вызывающим.
        let view = unsafe {
            SharedView::initialize(
                mapping.as_ptr(),
                layout,
                std::process::id(),
                crate::win::current_process_start_time(),
                slot_count,
            )
        }?;
        let ring_tx = view.ring_a();
        let ring_rx = view.ring_b();
        Ok(Self {
            view,
            events: None,
            ring_tx,
            ring_rx,
            connected: false,
            peer: None,
            peer_start: 0,
            exclusive: false,
            session_claim: CLAIM_FREE,
            claim_seen: None,
            lingering: None,
            reserve_timeout: crate::constants::RESERVE_TIMEOUT,
            presence: None,
            presence_status: crate::PresenceStatus::Disabled,
            #[cfg(test)]
            wait_wakeups: Arc::default(),
            _namespace: None,
            _mapping: mapping,
            _not_sync: PhantomData,
        })
    }

    /// Раскладка канала (параметры обоих направлений).
    #[must_use]
    pub const fn layout(&self) -> &ChannelLayout {
        self.view.layout()
    }

    /// Получить handles событий для передачи в kernel driver
    ///
    /// Возвращает `None` если сервер создан в anonymous режиме (без событий).
    /// В этом случае используется polling через `wait_for_client_noevent()`.
    #[must_use]
    pub fn get_event_handles(&self) -> Option<crate::events::EventHandles> {
        self.events.as_ref().map(|e| e.get_event_handles())
    }

    pub fn wait_for_client(&mut self, timeout: Option<Duration>) -> Result<()> {
        if self.connected {
            return Err(ShmError::AlreadyConnected);
        }

        // Для anonymous режима используем polling
        let Some(events) = self.events.as_ref() else {
            return self.wait_for_client_noevent(timeout);
        };

        // Находка 21: `CONNECT_REQ` без `CLIENT_HELLO` (толчок Multi,
        // клиент, откатившийся по таймауту, чужой процесс) больше не обрывает
        // ожидание `HandshakeFailed`: автосбросный сигнал поглощён, ждём
        // дальше остаток таймаута.
        let deadline = Deadline::new(timeout);
        loop {
            let fired = events.connect_req.wait(deadline.remaining())?;
            #[cfg(test)]
            self.wait_wakeups.fetch_add(1, Ordering::AcqRel);
            if !fired {
                return Err(ShmError::Timeout);
            }
            if self.hello_pending() {
                return self.complete_handshake();
            }
            // Толчок без заявки (ревизия 3): проверить протухший захват.
            if self.exclusive {
                observe_idle_claim(
                    &self.view,
                    &mut self.claim_seen,
                    &mut self.lingering,
                    self.reserve_timeout,
                );
            }
        }
    }

    /// Принять клиента асинхронно (0.10; семантика -- `wait_for_client` без
    /// таймаута): future ждёт `C2S_CONNECT_REQ` в реакторе IOCP, сигнал без
    /// `CLIENT_HELLO` поглощает и ждёт дальше, при заявке проводит
    /// рукопожатие (ревизия 2: фиксация CAS `client_state`, проигранный CAS
    /// -- `Err(HandshakeFailed)`, как у синхронного пути). Таймаут -- у
    /// исполнителя: брошенный future снимает ожидание и возвращает
    /// потреблённый `CONNECT_REQ` событию (лишний сигнал протокол допускает).
    ///
    /// `Err(AlreadyConnected)` -- клиент уже подключён; anonymous-сервер --
    /// `Err(InvalidConfig)` (событий нет; его путь --
    /// `wait_for_client_noevent`).
    ///
    /// Любой исполнитель (здесь -- минимальный `block_on` на `thread::park`;
    /// с tokio -- `tokio::time::timeout(d, server.accept_async()).await
    /// .map_err(|_| ShmError::Timeout)??`, как в README):
    ///
    /// ```no_run
    /// # use std::future::Future;
    /// # use std::pin::pin;
    /// # use std::sync::Arc;
    /// # use std::task::{Context, Poll, Wake, Waker};
    /// # use std::thread::{self, Thread};
    /// # struct Unpark(Thread);
    /// # impl Wake for Unpark {
    /// #     fn wake(self: Arc<Self>) {
    /// #         self.0.unpark();
    /// #     }
    /// # }
    /// # /// Минимальный исполнитель: опрос, пока не готово, сон до `wake`.
    /// # fn block_on<F: Future>(future: F) -> F::Output {
    /// #     let waker = Waker::from(Arc::new(Unpark(thread::current())));
    /// #     let mut cx = Context::from_waker(&waker);
    /// #     let mut future = pin!(future);
    /// #     loop {
    /// #         if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
    /// #             return value;
    /// #         }
    /// #         thread::park();
    /// #     }
    /// # }
    /// let mut server = xshm::SharedServer::start("MyChannel")?;
    /// block_on(server.accept_async())?;
    /// let mut buf = Vec::new();
    /// let len = block_on(server.recv_async(&mut buf))?;
    /// block_on(server.send_async(&buf[..len]))?;
    /// # Ok::<(), xshm::ShmError>(())
    /// ```
    pub fn accept_async(&mut self) -> impl Future<Output = Result<()>> + Send + '_ {
        AcceptFuture {
            server: self,
            waits: WaitSet::default(),
        }
    }

    /// Принять сообщение клиента асинхронно (0.10): `Ok(len)` -- payload в
    /// `buf[..len]`. Ждёт `C2S_DATA` в реакторе IOCP без потока на канал и
    /// без опроса (подробно -- модуль `asynch`: порядок «регистрация ->
    /// перепроверка» без потерянного пробуждения).
    ///
    /// Отключение клиента -- `Err(NotConnected)`, смерть его процесса --
    /// `Err(PeerDied)`; обе -- только когда кольцо дочитано, и после них
    /// сервер отмечен неподключённым (`is_connected() == false`, можно снова
    /// `accept_async`). Anonymous-сервер -- `Err(InvalidConfig)`.
    ///
    /// `&mut self`: future -- `Send` (`tokio::spawn`), а приём и отправка
    /// одной стороны одновременно требуют разделения (`Mutex`, задача-
    /// владелец с каналом команд).
    ///
    /// Future выполняет операцию на каждом опросе: после `Ready` его не
    /// опрашивают снова (повторный опрос выполнит ещё одну операцию).
    pub fn recv_async<'a>(
        &'a mut self,
        buf: &'a mut Vec<u8>,
    ) -> impl Future<Output = Result<usize>> + Send + 'a {
        RecvFuture::new(self, buf)
    }

    /// Отправить сообщение клиенту асинхронно **без перезаписи** (0.10):
    /// нет места -- future ждёт его (`S2C_SPACE` через заявку
    /// `space_waiter`, как `wait_for_space`), затем пишет. Ошибки длины --
    /// как у `try_send_to_client`; ушедший клиент -- `Err(NotConnected)`,
    /// умерший -- `Err(PeerDied)` (если места так и нет; сервер при этом НЕ
    /// отмечается отключённым -- входящее кольцо дочитывает `recv_async`).
    /// Anonymous-сервер -- `Err(InvalidConfig)`.
    ///
    /// Future выполняет операцию на каждом опросе: после `Ready` его не
    /// опрашивают снова (повторный опрос выполнит ещё одну операцию).
    pub fn send_async<'a>(
        &'a mut self,
        data: &'a [u8],
    ) -> impl Future<Output = Result<()>> + Send + 'a {
        SendFuture::new(self, data)
    }

    /// Клиент выставил заявку (`client_state == CLIENT_HELLO`). Acquire:
    /// PID/время клиента, записанные до заявки, видны `complete_handshake`.
    fn hello_pending(&self) -> bool {
        self.view
            .control_block()
            .client_state
            .load(Ordering::Acquire)
            == HANDSHAKE_CLIENT_HELLO
    }

    /// Ждать клиента ИЛИ стороннее событие `wake` (дескриптор события этого
    /// процесса: команда worker-у, остановка). `Ok(true)` -- клиент
    /// подключён (handshake завершён); `Ok(false)` -- сработал `wake` или
    /// истёк `timeout` (`None` -- без таймаута). Только именованный сервер.
    pub(crate) fn wait_for_client_or(
        &mut self,
        wake: BorrowedHandle<'_>,
        timeout: Option<Duration>,
    ) -> Result<bool> {
        if self.connected {
            return Err(ShmError::AlreadyConnected);
        }
        let Some(events) = self.events.as_ref() else {
            return Err(ShmError::InvalidConfig(
                "anonymous server has no events to wait on",
            ));
        };
        // Находка 21: `CONNECT_REQ` без `CLIENT_HELLO` -- не ошибка и не
        // повод вернуть управление: сигнал поглощён, ждём остаток таймаута
        // (как в `wait_for_client`). Наружу выходят только подключение,
        // `wake`, таймаут и ошибки самого рукопожатия.
        //
        // Task 10 (INTEROP §e.1, П1-П3): `wake` -- индекс 0. Цикл поглощает
        // пустой `CONNECT_REQ` и ждёт снова, не возвращаясь к вызывающему:
        // стой `wake` вторым, процесс, взводящий `CONNECT_REQ` чаще, чем мы
        // ждём, держал бы `stop()` worker-а Auto и лобби Dispatch вечно. А
        // чтобы индекс 0 не скрыл заявку (поток `send` взводит `wake` на
        // каждой отправке), исход решает СОСТОЯНИЕ: после любого возврата
        // из ожидания сначала `hello_pending()` -- заявка есть, рукопожатие
        // (оставшийся взведённым `CONNECT_REQ` следующее ожидание поглотит
        // как пустой). Пустой сигнал стоит O(1): одно чтение состояния и
        // `observe_idle_claim`.
        let deadline = Deadline::new(timeout);
        loop {
            let fired = wait_any(
                &[wake, events.connect_req.as_handle()],
                deadline.remaining(),
            )?;
            #[cfg(test)]
            self.wait_wakeups.fetch_add(1, Ordering::AcqRel);
            if self.hello_pending() {
                self.complete_handshake()?;
                return Ok(true);
            }
            if fired != Some(ACCEPT_CONNECT_REQ) {
                // `wake` или таймаут.
                return Ok(false);
            }
            // Толчок без заявки (ревизия 3): проверить протухший захват.
            if self.exclusive {
                observe_idle_claim(
                    &self.view,
                    &mut self.claim_seen,
                    &mut self.lingering,
                    self.reserve_timeout,
                );
            }
            #[cfg(test)]
            adversary_resignal(&self.view, &events.connect_req);
        }
    }

    /// Принять клиента, чей `CONNECT_REQ` уже получен вызывающим (Multi ждёт
    /// события сразу всех слотов в одном наборе). `Err(HandshakeFailed)` --
    /// `client_state` не `CLIENT_HELLO` (сигнал без заявки): здесь ошибка
    /// остаётся -- ожидания нет, а Multi по ней отличает «толчок»
    /// (`nudge_stale_claims`) и откат клиента и молча продолжает свой цикл.
    pub(crate) fn accept_pending(&mut self) -> Result<()> {
        if self.connected {
            return Err(ShmError::AlreadyConnected);
        }
        if !self.hello_pending() {
            return Err(ShmError::HandshakeFailed);
        }
        self.complete_handshake()
    }

    /// Ожидание клиента без событий (polling по shared memory).
    /// Использовать когда нет доступа к именованным событиям.
    pub fn wait_for_client_noevent(&mut self, timeout: Option<Duration>) -> Result<()> {
        if self.connected {
            return Err(ShmError::AlreadyConnected);
        }

        let control = self.view.control_block();
        let start = Instant::now();

        loop {
            let client_state = control.client_state.load(Ordering::Acquire);
            if client_state == HANDSHAKE_CLIENT_HELLO {
                break;
            }

            if let Some(t) = timeout
                && start.elapsed() >= t
            {
                return Err(ShmError::Timeout);
            }

            std::thread::sleep(Duration::from_millis(1));
        }

        self.complete_handshake()
    }

    /// Серверная сторона рукопожатия (ревизия 3, INTEROP R16/R18), общая для
    /// событийного и polling-путей; вызывается при `client_state ==
    /// CLIENT_HELLO` (Acquire в `hello_pending`/`wait_for_client_noevent`).
    ///
    /// - S1: предварительно -- claim `T`, самоописание клиента (PID, время)
    ///   и наблюдение его процесса (`open_peer`);
    /// - S2: заявка подтверждённо умершего держателя claim-а снимается до
    ///   фиксации -- без нового `generation` и без `S2C_CONNECT`;
    /// - S3: сброс колец -> `generation + 1` (Release) -> заголовки и
    ///   `server_state = SERVER_READY`. Буферы сбрасываются ДО публикации
    ///   generation, поэтому клиент, увидевший фиксацию (S5 публикуется
    ///   позже), видит уже очищенные кольца;
    /// - S4: сброс `DISCONNECT` -- ДО фиксации (ревизия 3, G1): прощальный
    ///   сигнал клиента, принятого по проигранному отзыву и сразу ушедшего,
    ///   больше не съедается;
    /// - S5: фиксация CAS `CLIENT_HELLO -> SERVER_READY`; проиграна
    ///   (клиент отозвал заявку) -- `server_state`/заголовки в `IDLE`,
    ///   `S2C_CONNECT` НЕ сигналится, `Err(HandshakeFailed)`. Опубликованный
    ///   `generation` остаётся: клиент засчитывает его только при
    ///   проигранном отзыве;
    /// - S6: самоописание и claim сессии -- владельца зафиксированной заявки
    ///   (G2: им мог оказаться следующий клиент -- тогда наблюдается его
    ///   процесс);
    /// - S7: `S2C_CONNECT`, подключён.
    fn complete_handshake(&mut self) -> Result<()> {
        let control = self.view.control_block();
        // S1. PID и время клиента, записанные им ДО `CLIENT_HELLO` (видны
        // благодаря Acquire-чтению `client_state`), -- пока без потребления:
        // одноразовыми их делает S6. 0 -- клиент без PID (старая версия).
        let claim = control.slot_claim.load(Ordering::Acquire);
        let pid = control.client_pid.load(Ordering::Acquire);
        let start = control.client_start_time();
        let mut watch = ProcessWatch::open_peer(pid, start);

        // S2. Заявка умершего: процесс не наблюдается, он же держатель
        // claim-а (защита от ABA G2: PID прочитан у прежнего клиента) и
        // подтверждённо ушёл. Снимаем CAS-ом -- отзыв самого клиента или
        // гонка с ним решаются однозначно.
        //
        // Fix round 1: непосредственно перед CAS -- перепроверка `slot_claim
        // == T`. S1-чтения могли устареть: прежний клиент P отозвал заявку,
        // освободил claim и умер, а новый держатель A уже поставил свою
        // заявку (`slot_owner_pid` ещё P, пока A не дошёл до C2). Пока стоит
        // `T`, новой заявки нет (I3) -- CAS снимает заявку именно умершего;
        // claim сменился -- S2 пропускается, заявку A фиксируют S3-S7 (S6
        // наблюдает процесс A по его самоописанию). Клиент без claim
        // (`T == 0`, наследный путь) -- без перепроверки.
        let holder_dead = watch.is_none() && claim_holder_gone(control, pid, start);
        #[cfg(test)]
        if holder_dead {
            handshake_hook(&self.view, HandshakePoint::BeforeDeadWithdraw);
        }
        if holder_dead
            && (claim == CLAIM_FREE || control.slot_claim.load(Ordering::Acquire) == claim)
        {
            // Fix round 2: самоописание и заголовки умершего обнуляются ДО
            // CAS, который делает `IDLE` видимым. После CAS (claim умершего
            // ещё стоит) клиент лобби с брошенным мьютексом вправе
            // перехватить claim и записать своё самоописание (C2/C5) --
            // обнуление после CAS затёрло бы его, и его сессия осталась бы
            // без наблюдения. До CAS писать сюда некому: заявка умершего
            // стоит (C6 и перехват невозможны), claim по перепроверке -- его
            // (I3), сам он мёртв. CAS не удался (заявки уже нет) -- обнулены
            // всё равно только поля умершего.
            control.client_pid.store(0, Ordering::Release);
            control.announce_client_start(0);
            let (header_a, header_b) = self.view.headers();
            header_a
                .handshake_state
                .store(HANDSHAKE_IDLE, Ordering::Release);
            header_b
                .handshake_state
                .store(HANDSHAKE_IDLE, Ordering::Release);
            if control
                .client_state
                .compare_exchange(
                    HANDSHAKE_CLIENT_HELLO,
                    HANDSHAKE_IDLE,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                #[cfg(test)]
                handshake_hook(&self.view, HandshakePoint::AfterDeadWithdraw);
                // Claim -- последним: пока он стоит, новой заявки нет.
                if claim != CLAIM_FREE {
                    let _ = control.slot_claim.compare_exchange(
                        claim,
                        CLAIM_FREE,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                }
            }
            return Err(ShmError::HandshakeFailed);
        }

        // S3.
        let new_generation = control.generation.load(Ordering::Acquire).wrapping_add(1);
        let (header_a, header_b) = self.view.headers();
        header_a.reset(new_generation);
        header_b.reset(new_generation);
        control.generation.store(new_generation, Ordering::Release);
        header_a
            .handshake_state
            .store(HANDSHAKE_SERVER_READY, Ordering::Release);
        header_b
            .handshake_state
            .store(HANDSHAKE_SERVER_READY, Ordering::Release);
        control
            .server_state
            .store(HANDSHAKE_SERVER_READY, Ordering::Release);

        // S4. `DISCONNECT` -- одно автосбросное событие на обе стороны:
        // сигнал прошлой сессии (никто его не дождался) иначе мгновенно
        // «отключил» бы нового клиента. ДО фиксации: после неё клиент может
        // уже уйти и взвести свой.
        if let Some(events) = &self.events {
            let _ = events.disconnect.reset();
        }
        #[cfg(test)]
        handshake_hook(&self.view, HandshakePoint::BeforeCommit);

        // S5. Точка фиксации: заявка ещё стоит -- рукопожатие наше.
        if control
            .client_state
            .compare_exchange(
                HANDSHAKE_CLIENT_HELLO,
                HANDSHAKE_SERVER_READY,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            // Клиент отозвал заявку (таймаут/остановка) раньше нас.
            control
                .server_state
                .store(HANDSHAKE_IDLE, Ordering::Release);
            header_a
                .handshake_state
                .store(HANDSHAKE_IDLE, Ordering::Release);
            header_b
                .handshake_state
                .store(HANDSHAKE_IDLE, Ordering::Release);
            return Err(ShmError::HandshakeFailed);
        }
        #[cfg(test)]
        handshake_hook(&self.view, HandshakePoint::AfterCommit);

        // S6. Отклонение от дизайна §2.2 S6 (найдено моделью
        // `handshake_model`): дизайн забирал самоописание `swap`-ом и брал
        // `T2 = slot_claim` без сверки. Если владелец сессии (клиент,
        // принятый по проигранному отзыву) успел уйти и освободить claim, а
        // следующий клиент -- захватить его и записать своё самоописание,
        // `swap` забрал бы ЕГО PID (его сессия осталась бы без наблюдения, а
        // при совпадении PID одного процесса -- ABA значения), а
        // `session_claim` назвал бы чужой токен. Поэтому: прочитать claim и
        // самоописание, затем убедиться, что владелец ещё в сессии
        // (`client_state == SERVER_READY`: свой claim он освобождает только
        // после CAS `SERVER_READY -> IDLE`, R18) -- значит, прочитанное --
        // его. Потребить CAS-ом; если после этого claim уже не `t2` (токены
        // уникальны), потреблённое могло оказаться значением следующего
        // клиента -- вернуть его.
        let session_claim = control.slot_claim.load(Ordering::Acquire);
        let pid2 = control.client_pid.load(Ordering::Acquire);
        let start2 = control.client_start_time();
        let owner_present = control.client_state.load(Ordering::Acquire) == HANDSHAKE_SERVER_READY;
        let (mut peer_start, claim) = (start, if owner_present { session_claim } else { claim });
        if owner_present {
            if control
                .client_pid
                .compare_exchange(pid2, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                if control.slot_claim.load(Ordering::Acquire) == session_claim {
                    control.announce_client_start(0);
                } else {
                    let _ = control.client_pid.compare_exchange(
                        0,
                        pid2,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                }
            }
            if (pid2, start2) != (pid, start) {
                // G2: зафиксирована заявка следующего клиента (прежний
                // отозвал свою между S1 и S5) -- наблюдаем его процесс.
                watch = ProcessWatch::open_peer(pid2, start2);
                peer_start = start2;
                if watch.is_none() && claim_holder_gone(control, pid2, start2) {
                    return Err(self.abort_session(claim));
                }
            }
        }

        // S7.
        self.peer_start = if watch.is_some() { peer_start } else { 0 };
        self.peer = watch;
        self.session_claim = claim;
        self.claim_seen = None;
        // Новая сессия: наблюдение за клиентом прошлой (Task 7) больше не
        // нужно -- claim уже не его (новый держатель взял его свободным).
        self.lingering = None;
        // Для anonymous-сервера событий нет -- клиент узнаёт о готовности
        // опросом `server_state`.
        // S7, отказ сигнала (0.10, INTEROP п. 29): сессия уже
        // зафиксирована (S5) -- клиент мог засчитать её по проигранному
        // отзыву. Закрыть её с сигналом, как `disconnect_local` (`IDLE`,
        // потом `DISCONNECT`), а не оставить полуоткрытой.
        if let Some(events) = &self.events
            && let Err(err) = signal_connect_ack(&self.view, events)
        {
            self.disconnect_local();
            return Err(err);
        }
        self.connected = true;
        Ok(())
    }

    /// S6: зафиксированная сессия принадлежит подтверждённо умершему
    /// клиенту -- закрыть её, как `mark_disconnected`, освободить его claim
    /// и взвести `DISCONNECT` (на случай ложного «умер»: живой клиент,
    /// засчитавший подключение по проигранному отзыву, увидит разрыв, а не
    /// сессию без сервера). `S2C_CONNECT` не сигналится.
    fn abort_session(&mut self, claim: u32) -> ShmError {
        let control = self.view.control_block();
        control
            .server_state
            .store(HANDSHAKE_IDLE, Ordering::Release);
        let _ = control.client_state.compare_exchange(
            HANDSHAKE_SERVER_READY,
            HANDSHAKE_IDLE,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        let (header_a, header_b) = self.view.headers();
        header_a
            .handshake_state
            .store(HANDSHAKE_IDLE, Ordering::Release);
        header_b
            .handshake_state
            .store(HANDSHAKE_IDLE, Ordering::Release);
        if claim != CLAIM_FREE {
            let _ = control.slot_claim.compare_exchange(
                claim,
                CLAIM_FREE,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
        if let Some(events) = &self.events {
            let _ = events.disconnect.set();
        }
        ShmError::HandshakeFailed
    }

    #[must_use]
    pub const fn is_connected(&self) -> bool {
        self.connected
    }

    /// Доступ к событиям сервера (для внутреннего использования)
    ///
    /// ВАЖНО: Для anonymous режима возвращает None - события не создаются.
    /// Используйте только для named режима (SharedServer::start).
    #[must_use]
    pub const fn events(&self) -> Option<&SharedEvents> {
        self.events.as_ref()
    }

    /// Проверка, является ли сервер anonymous (без имени и events)
    #[must_use]
    pub const fn is_anonymous(&self) -> bool {
        self.events.is_none()
    }

    /// Получить raw HANDLE секции для передачи в kernel driver
    /// ВАЖНО: Handle принадлежит Mapping, не закрывать вручную!
    #[must_use]
    pub fn section_handle(&self) -> isize {
        self._mapping.section_handle()
    }

    /// Доступ к shared view (для внутреннего использования)
    pub(crate) const fn view(&self) -> &SharedView {
        &self.view
    }

    /// Claim (токен) клиента текущей сессии (ревизия 3, S6); Multi сверяет
    /// с ним штатный уход клиента.
    pub(crate) const fn session_claim(&self) -> u32 {
        self.session_claim
    }

    /// Тесты: подменить срок протухшего захвата.
    #[cfg(test)]
    pub(crate) const fn set_reserve_timeout(&mut self, timeout: Duration) {
        self.reserve_timeout = timeout;
    }

    /// Тесты: счётчик пробуждений ожиданий `wait_for_client*`.
    #[cfg(test)]
    pub(crate) fn wait_wakeups(&self) -> Arc<std::sync::atomic::AtomicU64> {
        Arc::clone(&self.wait_wakeups)
    }

    pub(crate) fn mark_disconnected(&mut self) {
        self.connected = false;
        self.peer_start = 0;
        // Handle процесса бывшего клиента закрывается в конце (RAII): по нему
        // ещё решается освобождение claim-а.
        let peer = self.peer.take();
        let session_claim = core::mem::replace(&mut self.session_claim, CLAIM_FREE);

        // Сбрасываем состояние в shared memory для возможности reconnect
        let control = self.view.control_block();
        control
            .server_state
            .store(HANDSHAKE_IDLE, Ordering::Release);
        // 0.9: CAS, а не store -- сбрасываем только состояние СВОЕЙ сессии
        // (`SERVER_READY`). Если следующий клиент уже успел написать
        // `CLIENT_HELLO` (лобби: он вошёл сразу, как прежний отпустил
        // мьютекс лобби), безусловный store стёр бы его заявку, и сервер
        // принял бы его `CONNECT_REQ` за ложный.
        let _ = control.client_state.compare_exchange(
            HANDSHAKE_SERVER_READY,
            HANDSHAKE_IDLE,
            Ordering::AcqRel,
            Ordering::Acquire,
        );

        // Заголовки -- тоже CAS `SERVER_READY -> IDLE` (только значение
        // своей сессии, его поставил S3): ушедший клиент уже перевёл их в
        // `IDLE`, а следующий мог успеть записать `CLIENT_HELLO` (C7) --
        // безусловный store затёр бы его. Поле информационное (Rust его не
        // читает), следующий S3 всё равно перезапишет оба заголовка.
        let (header_a, header_b) = self.view.headers();
        for header in [header_a, header_b] {
            let _ = header.handshake_state.compare_exchange(
                HANDSHAKE_SERVER_READY,
                HANDSHAKE_IDLE,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }

        // Ревизия 3 (R18): клиент умер в сессии (handle процесса с
        // подтверждённым временем создания сигнален) -- сам он claim уже не
        // снимет: освобождаем его claim сессии сразу, ПОСЛЕ сброса
        // состояния. Штатный уход и отключение сервером claim не трогают:
        // его снимает клиент (L).
        let Some(peer) = peer else {
            return;
        };
        if session_claim == CLAIM_FREE {
            return;
        }
        if self.exclusive && peer.has_exited() {
            let _ = control.slot_claim.compare_exchange(
                session_claim,
                CLAIM_FREE,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            return;
        }
        // Task 7: клиент жив (или это слот Multi), а claim всё ещё его --
        // он может умереть, так и не заметив закрытия сессии (`DISCONNECT`
        // сервера, порча кольца). Handle его процесса не закрываем, а
        // держим с токеном («задержавшийся владелец»): подтверждённая смерть
        // освобождает claim сразу (`release_if_holder_dead`), а не через
        // `RESERVE_TIMEOUT`. Ушёл сам (L) -- claim сменится, и наблюдение
        // снимется при первой проверке.
        if control.slot_claim.load(Ordering::Acquire) == session_claim {
            self.lingering = Some((peer, session_claim));
        }
    }

    /// Multi (Task 7): забрать «задержавшегося владельца», оставленного
    /// `mark_disconnected` (отказ S7 рукопожатия слота), -- его наблюдает
    /// диспетчер привязкой процесса к порту.
    pub(crate) const fn take_lingering(&mut self) -> Option<Lingering> {
        self.lingering.take()
    }

    /// Взвести `DISCONNECT` канала (anonymous -- событий нет, no-op).
    /// Вызывающий уже перевёл состояния в `IDLE` (R14: сначала состояние,
    /// потом сигнал -- клиент, проснувшись, видит закрытую сессию).
    pub(crate) fn signal_disconnect(&self) {
        if let Some(events) = &self.events {
            let _ = events.disconnect.set();
        }
    }

    /// Локальный разрыв сессии сервером (ошибка: порча кольца, отказ
    /// ожидания; закрытие сессии лобби) -- уход сервера по R14/INTEROP §e.2
    /// п. 5: `mark_disconnected` (состояния -> `IDLE`), потом `DISCONNECT`.
    /// Без сигнала клиент, спящий в своём наборе без таймаута, остался бы
    /// «подключённым» к никому (final review I2). Claim не трогаем: его
    /// снимет клиент, получив `DISCONNECT` (уход L); клиента, умершего, не
    /// заметив разрыва, -- сервер по подтверждённой смерти на первом же
    /// толчке (Task 7, `lingering`).
    pub(crate) fn disconnect_local(&mut self) {
        self.mark_disconnected();
        self.signal_disconnect();
    }

    /// `CONNECT_REQ` без заявки у неподключённого сервера single-client
    /// канала (ревизия 3, §2.3 дизайна): толчок проигравшего клиента --
    /// повод проверить протухший захват (см. `observe_idle_claim`).
    fn observe_idle(&mut self) {
        if self.exclusive && !self.connected {
            observe_idle_claim(
                &self.view,
                &mut self.claim_seen,
                &mut self.lingering,
                self.reserve_timeout,
            );
        }
    }

    const fn ensure_connected(&self) -> Result<()> {
        if !self.connected {
            Err(ShmError::NotConnected)
        } else {
            Ok(())
        }
    }

    pub fn send_to_client(&self, payload: &[u8]) -> Result<WriteOutcome> {
        self.ensure_connected()?;
        let result = self.ring_tx.write_message(payload)?;
        // Сигнализируем только если events доступны
        if let Some(ref events) = self.events
            && result.was_empty
        {
            let _ = events.s2c.data.set();
        }
        Ok(result)
    }

    /// Отправка клиенту **без перезаписи** непрочитанных данных.
    ///
    /// Сообщение либо целиком ложится в свободное место кольца
    /// Server -> Client, либо возвращается `Err(ShmError::QueueFull)` и
    /// кольцо не меняется. Подробная семантика -- `RingBuffer::try_write_message`
    /// (атомарность, консервативность проверки, `MessageTooSmall`/`MessageTooLarge`).
    /// Ждать освобождения места -- `wait_for_space`.
    pub fn try_send_to_client(&self, payload: &[u8]) -> Result<WriteOutcome> {
        self.ensure_connected()?;
        let result = self.ring_tx.try_write_message(payload)?;
        if let Some(ref events) = self.events
            && result.was_empty
        {
            let _ = events.s2c.data.set();
        }
        Ok(result)
    }

    /// Отправка клиенту без перезаписи с заполнением на месте (zero-copy,
    /// 0.10): `f` получает срез из `len` байт payload прямо в кольце
    /// Server -> Client и заполняет его целиком; после возврата `f` кадр
    /// публикуется (сигнал `DATA` -- как у `try_send_to_client`). Место и
    /// ошибки -- как у `try_send_to_client` (`QueueFull` -- `f` не
    /// вызывается); ждать места -- `wait_for_space(len, ..)`.
    ///
    /// Начальное содержимое среза не определено (остатки прежних кадров).
    /// Срез -- неопубликованная часть разделяемой памяти: недоверенный
    /// клиент, нарушая протокол, может менять эти байты во время `f` --
    /// пострадает только сообщение, которое получит он сам.
    ///
    /// `&mut self`: пока работает `f`, этот сервер больше никто не трогает --
    /// ни вложенная отправка из `f` (она получила бы ту же область кольца, и
    /// одно из сообщений пропало бы), ни другой поток. Это проверяет
    /// компилятор:
    ///
    /// ```compile_fail,E0502
    /// fn nested(server: &mut xshm::SharedServer) {
    ///     let _ = server.send_with(16, |buf| {
    ///         let _ = server.try_send_to_client(b"nested-message!!");
    ///         buf.fill(b'O');
    ///     });
    /// }
    /// ```
    pub fn send_with(&mut self, len: usize, f: impl FnOnce(&mut [u8])) -> Result<WriteOutcome> {
        self.ensure_connected()?;
        // SAFETY: `&mut self` -- этот сервер (единственный писатель кольца
        // Server -> Client в процессе: секцию создаёт он сам, без
        // `OBJ_OPENIF`) на время `f` не используется больше никем: ни
        // вложенной записи из `f`, ни другого потока.
        let result = unsafe { self.ring_tx.write_with(len, f) }?;
        if let Some(ref events) = self.events
            && result.was_empty
        {
            let _ = events.s2c.data.set();
        }
        Ok(result)
    }

    /// Свободное место в кольце Server -> Client.
    ///
    /// Из потока, который пишет в это кольцо, -- нижняя граница: если
    /// `free_space().fits(n)`, следующий `try_send_to_client` с payload длины
    /// `n` пройдёт. Вне подключения возвращает `FreeSpace::ZERO`.
    #[must_use]
    pub fn free_space(&self) -> FreeSpace {
        if self.connected {
            self.ring_tx.free_space()
        } else {
            FreeSpace::ZERO
        }
    }

    /// Ждать, пока в кольце Server -> Client освободится место под payload
    /// длины `payload_len`. `Ok(true)` -- место есть, `Ok(false)` -- таймаут;
    /// `timeout = None` -- без ограничения (мёртвый клиент место не
    /// освободит -- лучше задавать таймаут и параллельно следить за живостью).
    ///
    /// 0.9: одно ожидание в ядре на весь `timeout`, без нарезки на срезы, по
    /// набору `[S2C_SPACE, S2C_DISCONNECT, процесс клиента]`:
    /// - `SPACE` -- клиент снял заявку (места хватает) или опустошил кольцо
    ///   (клиент старой версии, не знающий заявку, будит только так);
    /// - `DISCONNECT` при `client_state != SERVER_READY` -- клиент штатно
    ///   ушёл: `Err(NotConnected)`, событие возвращается взведённым;
    /// - процесс клиента сигнален (клиент 0.8+ передал PID) -- `Err(PeerDied)`.
    ///
    /// Клиент без PID (старая версия), упавший без `DISCONNECT`, не будит
    /// ничем: единственная граница тогда -- `timeout` вызывающего. Без
    /// событий (anonymous) -- опрос с шагом 1 мс.
    ///
    /// `wait_for_space` (по `&self`) сторону отключённой не помечает: уход
    /// клиента только сообщается (`NotConnected`, `DISCONNECT` остаётся
    /// взведённым), сессию заканчивает следующий `poll_client`.
    pub fn wait_for_space(&self, payload_len: usize, timeout: Option<Duration>) -> Result<bool> {
        self.wait_for_space_spin(payload_len, timeout, SpinPolicy::NONE)
    }

    /// `wait_for_space` со спином перед ожиданием в ядре (0.10): до
    /// `spin.iterations` перепроверок места по позициям (заявка
    /// `space_waiter` на время спина не ставится), затем обычное ожидание.
    /// При `timeout = Some(0)` спина нет; дедлайн `timeout` спин сверяет раз
    /// в 64 итерации (перебег -- не больше 64 итераций). `SpinPolicy::NONE` -- то же, что
    /// `wait_for_space`.
    pub fn wait_for_space_spin(
        &self,
        payload_len: usize,
        timeout: Option<Duration>,
        spin: SpinPolicy,
    ) -> Result<bool> {
        self.ensure_connected()?;
        let peer_left = || self.client_left();
        let disconnect = self.events.as_ref().map(|e| DisconnectWatch {
            event: &e.disconnect,
            peer_left: &peer_left,
        });
        let event = self.events.as_ref().map(|e| &e.s2c.space);
        self.ring_tx.wait_for_space(
            payload_len,
            event,
            self.peer.as_ref(),
            disconnect.as_ref(),
            timeout,
            spin,
        )
    }

    /// Для auto-режима: заявка (под payload длины `payload_len`, с PAD в
    /// текущей позиции записи) и снятие ожидания места в исходящем кольце.
    pub(crate) fn arm_space_waiter(&self, payload_len: usize) {
        self.ring_tx.arm_space_waiter_for(payload_len);
    }

    pub(crate) fn disarm_space_waiter(&self) {
        self.ring_tx.disarm_space_waiter();
    }

    pub fn receive_from_client(&self, buffer: &mut Vec<u8>) -> Result<usize> {
        self.ensure_connected()?;
        let len = self.ring_rx.read_message(buffer)?;
        self.signal_space_after_read();
        Ok(len)
    }

    /// Zero-copy приём от клиента (0.10): `f` получает payload сообщения
    /// срезом прямо из кольца Client -> Server, после возврата `f` сообщение
    /// забирается (сигнал `SPACE` -- как у `receive_from_client`).
    /// `Err(QueueEmpty)`/`Err(Corrupted)` -- `f` не вызывается;
    /// `Err(Overrun)` -- клиент перезаписал кадр во время `f` (только запись
    /// с перезаписью), результат `f` отброшен. Подробно --
    /// `RingBuffer::read_with`.
    ///
    /// # Safety
    /// Пока выполняется `f`, никто не изменяет байты этого сообщения в
    /// разделяемой памяти (иначе -- гонка данных и TOCTOU в `f`). Выполнено,
    /// когда клиент -- доверенный честный пир xshm, пишущий это направление
    /// без перезаписи (`try_send_to_server`, `send_with`, `wait_for_space`;
    /// не `send_to_server`). Для недоверенного клиента --
    /// `receive_from_client` (копия с проверкой после копирования).
    ///
    /// Второго чтения этого направления во время `f` (вложенного или из
    /// другого потока) не бывает по типу: `&mut self`. Вложенный приём не
    /// компилируется:
    ///
    /// ```compile_fail,E0502
    /// fn nested(server: &mut xshm::SharedServer) {
    ///     let mut buf = Vec::new();
    ///     // SAFETY: (пример не компилируется)
    ///     let _ = unsafe {
    ///         server.receive_with(|_data| server.receive_from_client(&mut buf))
    ///     };
    /// }
    /// ```
    pub unsafe fn receive_with<R>(&mut self, f: impl FnOnce(&[u8]) -> R) -> Result<R> {
        // SAFETY: неизменность кадра -- контракт этой функции (`# Safety`);
        // единственность читателя на время `f` даёт `&mut self`.
        unsafe { self.receive_with_shared(f) }
    }

    /// `receive_with` по `&self` -- для worker-а Auto, который держит
    /// сервер один (сервер не выходит из его потока).
    ///
    /// # Safety
    /// Контракт `receive_with` и, сверх него, контракт единственного
    /// читателя `RingBuffer::read_with`: никакого другого чтения кольца
    /// Client -> Server, пока выполняется `f`.
    pub(crate) unsafe fn receive_with_shared<R>(&self, f: impl FnOnce(&[u8]) -> R) -> Result<R> {
        self.ensure_connected()?;
        // SAFETY: контракт `read_with` -- контракт этой функции (`# Safety`).
        let result = unsafe { self.ring_rx.read_with(f) };
        if result.is_ok() {
            self.signal_space_after_read();
        }
        result
    }

    /// Сигнализируем только если events доступны. SPACE -- когда кольцо
    /// опустело (как раньше) ИЛИ когда писатель оставил заявку и места ему
    /// теперь хватает (`try_send`/`wait_for_space` на той стороне).
    fn signal_space_after_read(&self) {
        if let Some(ref events) = self.events
            && (self.ring_rx.take_space_waiter() || self.ring_rx.is_empty())
        {
            let _ = events.c2s.space.set();
        }
    }

    /// Ждать данных от клиента.
    ///
    /// Исходы:
    /// - `Ok(true)` -- в кольце есть сообщение или сработал `C2S_DATA`
    ///   (возможно, устаревший: читать до `QueueEmpty`);
    /// - `Ok(false)` -- таймаут (anonymous-режим без данных -- сразу);
    /// - `Err(NotConnected)` -- клиент штатно ушёл (`DISCONNECT` при
    ///   `client_state != SERVER_READY`) и всё, что он успел записать, уже
    ///   прочитано; либо сервер не подключён;
    /// - `Err(PeerDied)` -- процесс клиента умер и всё, что он успел
    ///   записать, уже прочитано.
    ///
    /// Пока в кольце есть данные ушедшего клиента, возвращается `Ok(true)`.
    /// На завершающем исходе (`NotConnected` по уходу, `PeerDied`) сервер
    /// помечается отключённым (отсюда `&mut self`): `is_connected() ==
    /// false`, следующий `wait_for_client` примет нового клиента, повторный
    /// `poll_client` -- `Err(NotConnected)` сразу. Claim штатно ушедшего
    /// клиента не трогается (его снимает сам клиент), умершего -- снимается.
    ///
    /// Набор ожидания -- `[C2S_DATA, S2C_DISCONNECT, процесс клиента]`,
    /// одно ожидание в ядре на весь `timeout`; устаревший `DISCONNECT`
    /// (клиент в сессии) поглощается, ожидание продолжается остатком
    /// таймаута. `wait_for_space` (по `&self`) об уходе только сообщает
    /// (`NotConnected`, событие взведено) и сторону не помечает -- сессию
    /// закончит следующий `poll_client`.
    pub fn poll_client(&mut self, timeout: Option<Duration>) -> Result<bool> {
        self.poll_client_spin(timeout, SpinPolicy::NONE)
    }

    /// `poll_client` со спином перед ожиданием в ядре (0.10): до
    /// `spin.iterations` проверок кольца по позициям, затем обычное
    /// ожидание `DATA`. Спин сокращает задержку, когда клиент отвечает
    /// быстрее пробуждения из ядра, ценой занятого ядра CPU на время спина;
    /// при `timeout = Some(0)` спина нет; дедлайн `timeout` спин сверяет раз
    /// в 64 итерации (перебег -- не больше 64 итераций). `SpinPolicy::NONE` -- то же, что
    /// `poll_client`.
    pub fn poll_client_spin(
        &mut self,
        timeout: Option<Duration>,
        spin: SpinPolicy,
    ) -> Result<bool> {
        self.ensure_connected()?;
        let result = self.wait_client_data(timeout, spin);
        if matches!(result, Err(ShmError::NotConnected | ShmError::PeerDied)) {
            // Завершающий исход: кольцо дочитано, клиент ушёл или умер.
            // `mark_disconnected` снимает claim только умершего (R18).
            self.mark_disconnected();
        }
        result
    }

    /// Ожидание данных `poll_client_spin` без пометки отключения.
    /// `Err(NotConnected)`/`Err(PeerDied)` -- только завершающие исходы
    /// (кольцо пусто, клиент ушёл/умер).
    fn wait_client_data(&self, timeout: Option<Duration>, spin: SpinPolicy) -> Result<bool> {
        let peer_left = || self.client_left();
        // Для anonymous режима просто проверяем буфер (polling)
        let Some(events) = self.events.as_ref() else {
            if !self.ring_rx.is_empty()
                || (timeout != Some(Duration::ZERO)
                    && spin_until(spin, deadline_of(timeout), || self.ring_rx.has_unread()))
            {
                return Ok(true);
            }
            // Событий нет -- уход виден только по состоянию в памяти.
            // Клиент пишет данные до `IDLE` (R14): перепроверка кольца
            // после состояния не теряет его последних кадров.
            if peer_left() {
                return if self.ring_rx.is_empty() {
                    Err(ShmError::NotConnected)
                } else {
                    Ok(true)
                };
            }
            return match &self.peer {
                Some(peer) if peer.has_exited() && self.ring_rx.is_empty() => {
                    Err(ShmError::PeerDied)
                }
                _ => Ok(false), // Нет данных, но не timeout
            };
        };
        let disconnect = DisconnectWatch {
            event: &events.disconnect,
            peer_left: &peer_left,
        };
        self.ring_rx.wait_for_data(
            &events.c2s.data,
            self.peer.as_ref(),
            Some(&disconnect),
            timeout,
            spin,
        )
    }

    /// Клиент закрыл сессию: `client_state` уже не `SERVER_READY` (уход L
    /// ставит `IDLE` до `DISCONNECT`, R14). Отличает свежий `DISCONNECT`
    /// от устаревшего.
    fn client_left(&self) -> bool {
        self.view
            .control_block()
            .client_state
            .load(Ordering::Acquire)
            != HANDSHAKE_SERVER_READY
    }

    /// Worker-ы (спин перед сном, 0.10): в кольце от клиента есть
    /// непрочитанное -- по позициям, без барьера.
    pub(crate) fn has_unread_from_client(&self) -> bool {
        self.ring_rx.has_unread()
    }

    /// Multi: граница дочитывания кольца от ушедшего клиента (снимок
    /// `write_pos`).
    pub(crate) fn client_drain_snapshot(&self) -> DrainBound {
        self.ring_rx.drain_snapshot()
    }

    /// Multi: дочитывание ушедшего клиента до границы `bound`
    /// (`RingBuffer::drain_message`); прочитанное освобождает место, как
    /// `receive_from_client`.
    pub(crate) fn drain_from_client(
        &self,
        buffer: &mut Vec<u8>,
        bound: &mut DrainBound,
    ) -> Result<Option<usize>> {
        self.ensure_connected()?;
        let len = self.ring_rx.drain_message(buffer, bound)?;
        if len.is_some() {
            self.signal_space_after_read();
        }
        Ok(len)
    }

    /// Multi: проба кольца от клиента для спина диспетчера.
    pub(crate) const fn client_ring_probe(&self) -> UnreadProbe<'_> {
        self.ring_rx.unread_probe()
    }

    /// PID процесса клиента, если клиент 0.8+ передал его в handshake.
    #[must_use]
    pub fn peer_pid(&self) -> Option<u32> {
        self.peer.as_ref().map(ProcessWatch::pid)
    }

    /// PID клиента, только если он сообщил и время создания процесса
    /// (ненулевое), и оно совпало с процессом, открытым по этому PID
    /// (`ProcessWatch::open_peer`). Это **сверка самоописания** пира с
    /// существующим процессом, а не доказательство личности: PID и время
    /// пир пишет в разделяемую память сам, а время чужого процесса читается
    /// с `PROCESS_QUERY_LIMITED_INFORMATION`. Клиент без времени (`0`) --
    /// `None`, «не сверен» (0.10, Dispatch: сверка PID канала).
    pub(crate) fn verified_peer_pid(&self) -> Option<u32> {
        self.peer
            .as_ref()
            .filter(|_| self.peer_start != 0)
            .map(ProcessWatch::pid)
    }

    /// Жив ли процесс клиента: `Some(true/false)` -- клиент наблюдается через
    /// удерживаемый handle процесса; `None` -- не подключён или наблюдения
    /// нет (клиент старой версии, не передал PID, или handle не открылся --
    /// например, клиент в другой сессии без прав). Один syscall, без блокировки.
    #[must_use]
    pub fn is_peer_alive(&self) -> Option<bool> {
        if !self.connected {
            return None;
        }
        self.peer.as_ref().map(|peer| !peer.has_exited())
    }

    /// Handle процесса клиента для набора ожидания worker-а (auto-режим).
    pub(crate) fn peer_wait_handle(&self) -> Option<BorrowedHandle<'_>> {
        self.peer.as_ref().map(AsHandle::as_handle)
    }

    /// Забрать handle процесса клиента, не закрывая его (Multi: handle может
    /// быть в наборе ожидания worker-а, который ещё спит, -- закрывать
    /// ожидаемый handle нельзя, worker закроет его сам после пробуждения).
    pub(crate) const fn take_peer(&mut self) -> Option<ProcessWatch> {
        self.peer_start = 0;
        self.peer.take()
    }

    /// Тесты: подменить наблюдаемый процесс клиента сессии; прежний --
    /// вызывающему.
    #[cfg(test)]
    pub(crate) const fn replace_peer(&mut self, watch: ProcessWatch) -> Option<ProcessWatch> {
        self.peer.replace(watch)
    }
}

impl Endpoint for SharedServer {
    fn ensure_connected(&self) -> Result<()> {
        Self::ensure_connected(self)
    }

    fn channel_events(&self) -> Option<&SharedEvents> {
        self.events.as_ref()
    }

    fn rx_data(events: &SharedEvents) -> &EventHandle {
        &events.c2s.data
    }

    fn tx_space(events: &SharedEvents) -> &EventHandle {
        &events.s2c.space
    }

    fn rx(&self) -> &RingBuffer {
        &self.ring_rx
    }

    fn tx(&self) -> &RingBuffer {
        &self.ring_tx
    }

    fn peer_process(&self) -> Option<BorrowedHandle<'_>> {
        self.peer_wait_handle()
    }

    fn peer_left(&self) -> bool {
        self.client_left()
    }

    fn receive(&self, buf: &mut Vec<u8>) -> Result<usize> {
        self.receive_from_client(buf)
    }

    fn try_send(&self, data: &[u8]) -> Result<()> {
        self.try_send_to_client(data).map(drop)
    }

    fn mark_gone(&mut self) {
        self.mark_disconnected();
    }
}

/// Future `accept_async`: ожидание `CONNECT_REQ` в реакторе и рукопожатие.
struct AcceptFuture<'a> {
    server: &'a mut SharedServer,
    waits: WaitSet,
}

impl AcceptFuture<'_> {
    fn step(&mut self, waker: &Waker) -> Poll<Result<()>> {
        if self.server.connected {
            return Poll::Ready(Err(ShmError::AlreadyConnected));
        }
        if self.server.events.is_none() {
            return Poll::Ready(Err(ANONYMOUS));
        }
        // Как `wait_for_client`: заявку проверяем только по сигналу
        // `CONNECT_REQ` (клиент ставит `CLIENT_HELLO` до сигнала); сигнал
        // без заявки (толчок Multi, откатившийся клиент) поглощён -- ждём
        // дальше.
        if self.waits.harvest(waker)[OP] {
            if self.server.hello_pending() {
                return Poll::Ready(self.server.complete_handshake());
            }
            // Толчок без заявки (ревизия 3): проверить протухший захват.
            self.server.observe_idle();
        }
        let Some(events) = self.server.events.as_ref() else {
            return Poll::Ready(Err(ANONYMOUS));
        };
        match self
            .waits
            .ensure_handle(OP, events.connect_req.as_handle(), waker)
        {
            Ok(()) => Poll::Pending,
            Err(err) => Poll::Ready(Err(err)),
        }
    }

    /// Снять ожидание; потреблённый, но не разобранный `CONNECT_REQ` --
    /// вернуть (заявку тогда примет следующее ожидание).
    fn finish(&mut self) {
        let event = self.server.events.as_ref().map(|e| &e.connect_req);
        self.waits.clear(event, None);
    }
}

impl Future for AcceptFuture<'_> {
    type Output = Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let result = this.step(cx.waker());
        if result.is_ready() {
            this.finish();
        }
        result
    }
}

impl Drop for AcceptFuture<'_> {
    fn drop(&mut self) {
        self.finish();
    }
}

impl Drop for SharedServer {
    fn drop(&mut self) {
        // R19: маяк присутствия -- ПЕРВЫМ шагом, до `IDLE` и закрытия
        // объектов: «маяк взведён, канала нет» остаётся признаком падения
        // сервера, а не его штатного ухода.
        if let Some(presence) = self.presence.take() {
            presence.lower();
        }
        let control = self.view.control_block();
        control
            .server_state
            .store(HANDSHAKE_IDLE, Ordering::Release);
        control
            .client_state
            .store(HANDSHAKE_IDLE, Ordering::Release);
        let (header_a, header_b) = self.view.headers();
        header_a
            .handshake_state
            .store(HANDSHAKE_IDLE, Ordering::Release);
        header_b
            .handshake_state
            .store(HANDSHAKE_IDLE, Ordering::Release);
        if self.connected {
            self.signal_disconnect();
        }
    }
}

#[cfg(test)]
mod handshake_tests {
    //! Ревизия 2 (дефект 5): рукопожатие фиксируется CAS-ами `client_state`
    //! с обеих сторон -- откат клиента и ответ сервера не перетирают друг
    //! друга, `S2C_CONNECT` не остаётся взведённым для следующего клиента.

    use super::*;
    use crate::client::SharedClient;

    fn unique(tag: &str) -> String {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("XSHM_HS_{tag}_{}_{}", std::process::id(), ts % 1_000_000)
    }

    /// Клиент отозвал заявку (CAS `HELLO -> IDLE`) раньше, чем сервер её
    /// зафиксировал: рукопожатия нет, `S2C_CONNECT` не сигналится.
    #[test]
    fn server_does_not_commit_a_withdrawn_hello() {
        let name = unique("WITHDRAWN");
        let mut server = SharedServer::start(&name).unwrap();
        let generation = server
            .view()
            .control_block()
            .generation
            .load(Ordering::Acquire);
        // Заявка уже отозвана к моменту фиксации (клиентский CAS выиграл).
        server
            .view()
            .control_block()
            .client_state
            .store(HANDSHAKE_IDLE, Ordering::Release);
        assert_eq!(server.complete_handshake(), Err(ShmError::HandshakeFailed));
        assert!(!server.is_connected());
        let control = server.view().control_block();
        assert_eq!(control.server_state.load(Ordering::Acquire), HANDSHAKE_IDLE);
        assert_eq!(control.client_state.load(Ordering::Acquire), HANDSHAKE_IDLE);
        assert_ne!(control.generation.load(Ordering::Acquire), generation);
        let ack = &server.events().unwrap().connect_ack;
        assert!(
            !ack.wait(Some(Duration::ZERO)).unwrap(),
            "S2C_CONNECT взведён для никого"
        );
    }

    /// Устаревший `S2C_CONNECT` (сервер старой версии ответил клиенту,
    /// который уже откатился) не подтверждает заявку следующего клиента:
    /// клиент сбрасывает событие до `HELLO` и засчитывает подключение
    /// только по смене `generation`.
    #[test]
    fn stale_connect_ack_does_not_confirm_the_next_client() {
        let name = unique("STALEACK");
        let server = SharedServer::start(&name).unwrap();
        server.events().unwrap().connect_ack.set().unwrap();
        let err = SharedClient::connect(&name, Duration::from_millis(100)).unwrap_err();
        assert_eq!(err, ShmError::Timeout);
        let control = server.view().control_block();
        assert_eq!(control.client_state.load(Ordering::Acquire), HANDSHAKE_IDLE);
        assert_eq!(control.client_pid.load(Ordering::Acquire), 0);
    }

    /// Сервер принял клиента и сразу отключил (Drop) до того, как клиент
    /// дождался `S2C_CONNECT`: подключение засчитано по `generation`,
    /// `DISCONNECT` взведён, прощальное сообщение в кольце.
    #[test]
    fn client_sees_a_handshake_even_if_the_server_already_left() {
        let name = unique("GONE");
        let mut server = SharedServer::start(&name).unwrap();
        let accept = std::thread::spawn(move || {
            server
                .wait_for_client(Some(Duration::from_secs(5)))
                .unwrap();
            server.send_to_client(b"bye").unwrap();
            drop(server); // IDLE -> DISCONNECT
        });
        // Даём серверу сесть на `CONNECT_REQ`, затем подключаемся; к
        // моменту проверки сервер может быть уже в IDLE.
        std::thread::sleep(Duration::from_millis(30));
        let client = SharedClient::connect(&name, Duration::from_secs(5)).unwrap();
        accept.join().unwrap();
        assert!(client.is_connected());
        assert!(
            client
                .events()
                .disconnect
                .wait(Some(Duration::from_secs(1)))
                .unwrap()
        );
        let mut buf = Vec::new();
        let len = client.receive_from_server(&mut buf).unwrap();
        assert_eq!(&buf[..len], b"bye");
    }

    // ─── Ревизия 3 (Task 22) ────────────────────────────────────────────────

    use crate::client::{AckWait, ClaimMode, Hello};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Назначить одноразовый хук рукопожатия на точку `at` (этот поток).
    fn set_hook(at: HandshakePoint, hook: impl FnOnce(&SharedView) + 'static) {
        HANDSHAKE_HOOK.with(|slot| *slot.borrow_mut() = Some((at, Box::new(hook))));
    }

    use crate::test_support::dead_process;

    /// G2 (§7.1 п. 5): между публикацией `generation` (S3) и фиксацией (S5)
    /// клиент P отозвал заявку, а A выставил свою -- уже при новом
    /// `generation`. Фиксация легла на заявку A: A принят по `SERVER_READY`
    /// (по старому правилу «`generation != g0`» -- устаревший сигнал), сервер
    /// наблюдает процесс A. `forge_p` -- P заявил чужое время создания:
    /// самоописание A иное, и S6 переоткрывает процесс по нему.
    fn inflight_aba(forge_p: bool) {
        let name = format!("{}_{forge_p}", unique("ABA"));
        let mut server = SharedServer::start(&name).unwrap();
        if forge_p {
            let forged = crate::win::current_process_start_time().wrapping_add(1);
            crate::client::FORGED_START_TIME.with(|cell| cell.set(Some(forged)));
        }
        let p = Hello::post(None, &name, true, ClaimMode::Take);
        crate::client::FORGED_START_TIME.with(|cell| cell.set(None));
        let p = p.unwrap();
        let slot: Rc<RefCell<Option<Hello>>> = Rc::default();
        let (a_slot, a_name) = (slot.clone(), name);
        set_hook(HandshakePoint::BeforeCommit, move |_| {
            assert!(!p.withdraw(), "P отозвал заявку");
            let a = Hello::post(None, &a_name, true, ClaimMode::Take).unwrap();
            *a_slot.borrow_mut() = Some(a);
        });
        server.complete_handshake().unwrap();
        let a = slot.borrow_mut().take().unwrap();
        assert!(server.is_connected());
        assert_eq!(server.peer_pid(), Some(std::process::id()));
        assert_eq!(server.is_peer_alive(), Some(true));
        assert!(
            matches!(a.wait_ack(Duration::from_secs(1), None), AckWait::Accepted),
            "A не принял фиксацию своей заявки"
        );
        let client = a.finish();
        assert_eq!(server.session_claim(), client.claim());
        client.send_to_server(b"aba").unwrap();
        let mut buf = Vec::new();
        assert_eq!(server.receive_from_client(&mut buf), Ok(3));
    }

    #[test]
    fn inflight_aba_accepts_by_ready() {
        inflight_aba(false);
        inflight_aba(true);
    }

    /// G1 (§7.1 п. 6): клиент засчитал рукопожатие по проигранному отзыву и
    /// сразу ушёл, пока сервер между фиксацией (S5) и `S2C_CONNECT` (S7).
    /// Его `DISCONNECT` не съеден сбросом: сервер сбрасывает событие ДО
    /// фиксации (S4).
    #[test]
    fn fast_leaver_disconnect_not_eaten() {
        let name = unique("FASTLEAVE");
        let mut server = SharedServer::start(&name).unwrap();
        let hello = Hello::post(None, &name, true, ClaimMode::Take).unwrap();
        set_hook(HandshakePoint::AfterCommit, move |_| {
            assert!(hello.withdraw(), "отзыв обязан проиграть фиксации");
            drop(hello.finish());
        });
        server.complete_handshake().unwrap();
        assert!(Endpoint::peer_left(&server), "уход клиента не виден");
        assert!(
            server
                .events()
                .unwrap()
                .disconnect
                .wait(Some(Duration::ZERO))
                .unwrap(),
            "DISCONNECT ушедшего клиента съеден"
        );
        let control = server.view().control_block();
        assert_eq!(control.slot_claim.load(Ordering::Acquire), 0);
    }

    /// S7 (0.10, INTEROP п. 29): `S2C_CONNECT` не просигналился -- сессия, уже
    /// зафиксированная S5, закрывается с сигналом, как `disconnect_local`
    /// (`IDLE`, потом `DISCONNECT`): клиент, засчитавший подключение по
    /// проигранному отзыву, видит разрыв, а не сервер «подключённый» к
    /// никому. Канал принимает следующего клиента.
    #[test]
    fn failed_connect_ack_closes_the_session() {
        let name = unique("ACKFAIL");
        let mut server = SharedServer::start(&name).unwrap();
        let hello = Hello::post(None, &name, true, ClaimMode::Take).unwrap();
        FAIL_CONNECT_ACK.with(|fail| fail.set(true));
        assert!(matches!(
            server.complete_handshake(),
            Err(ShmError::WindowsError { .. })
        ));
        assert!(!server.is_connected());
        let control = server.view().control_block();
        assert_eq!(control.server_state.load(Ordering::Acquire), HANDSHAKE_IDLE);
        assert_eq!(control.client_state.load(Ordering::Acquire), HANDSHAKE_IDLE);
        let (header_a, header_b) = server.view().headers();
        assert_eq!(
            header_a.handshake_state.load(Ordering::Acquire),
            HANDSHAKE_IDLE
        );
        assert_eq!(
            header_b.handshake_state.load(Ordering::Acquire),
            HANDSHAKE_IDLE
        );
        assert!(
            server
                .events()
                .unwrap()
                .disconnect
                .wait(Some(Duration::ZERO))
                .unwrap(),
            "сессия закрыта без DISCONNECT"
        );
        // Клиент: отзыв проигран (`generation` сменился) -- засчитан
        // подключённым, но сервер уже ушёл; его уход освобождает claim.
        assert!(hello.withdraw(), "фиксация S5 состоялась");
        let client = hello.finish();
        drop(client);
        assert_eq!(control.slot_claim.load(Ordering::Acquire), 0);
        // Следующий клиент подключается.
        let next = Hello::post(None, &name, true, ClaimMode::Take).unwrap();
        server.complete_handshake().unwrap();
        assert!(matches!(
            next.wait_ack(Duration::from_secs(1), None),
            AckWait::Accepted
        ));
        assert!(server.is_connected());
    }

    /// Заявка процесса `pid`/`start` с владельцем claim-а `owner`.
    fn post_foreign_hello(server: &SharedServer, (pid, start): (u32, u64), owner: (u32, u64)) {
        let control = server.view().control_block();
        control.slot_claim.store(0x0BAD_C1A1, Ordering::Release);
        control.announce_owner_start(owner.1);
        control.slot_owner_pid.store(owner.0, Ordering::Release);
        control.announce_client_start(start);
        control.client_pid.store(pid, Ordering::Release);
        control
            .client_state
            .store(HANDSHAKE_CLIENT_HELLO, Ordering::Release);
    }

    /// §7.1 п. 7 (S2): заявка подтверждённо умершего процесса, который сам
    /// и держит claim, снимается до фиксации -- без нового `generation`, без
    /// `S2C_CONNECT`, claim свободен.
    #[test]
    fn dead_hello_withdrawn_before_commit() {
        let name = unique("DEADHELLO");
        let mut server = SharedServer::start(&name).unwrap();
        let dead = dead_process();
        post_foreign_hello(&server, dead, dead);
        let control = server.view().control_block();
        let generation = control.generation.load(Ordering::Acquire);
        assert_eq!(server.complete_handshake(), Err(ShmError::HandshakeFailed));
        assert!(!server.is_connected());
        let control = server.view().control_block();
        assert_eq!(control.generation.load(Ordering::Acquire), generation);
        assert_eq!(control.client_state.load(Ordering::Acquire), HANDSHAKE_IDLE);
        assert_eq!(control.slot_claim.load(Ordering::Acquire), 0);
        assert_eq!(control.client_pid.load(Ordering::Acquire), 0);
        assert_eq!(control.server_state.load(Ordering::Acquire), HANDSHAKE_IDLE);
        assert!(
            !server
                .events()
                .unwrap()
                .connect_ack
                .wait(Some(Duration::ZERO))
                .unwrap()
        );
    }

    /// Fix round 1 (находка 2 ревью): S1 прочитал заявку умершего P, но до
    /// CAS S2 P успел отозвать её и освободить claim, а живой A -- поставить
    /// свою (`slot_owner_pid` в момент проверки ещё P). Перепроверка
    /// `slot_claim == T` перед CAS пропускает S2: заявку A фиксируют S3-S7,
    /// сервер наблюдает процесс A, A принят.
    #[test]
    fn dead_hello_withdrawal_rechecks_claim() {
        let name = unique("DEADRECHECK");
        let mut server = SharedServer::start(&name).unwrap();
        let dead = dead_process();
        post_foreign_hello(&server, dead, dead);
        let slot: Rc<RefCell<Option<Hello>>> = Rc::default();
        let (a_slot, a_name) = (slot.clone(), name);
        set_hook(HandshakePoint::BeforeDeadWithdraw, move |view| {
            // P отозвал заявку (W) и освободил claim, затем умер.
            let control = view.control_block();
            control
                .client_state
                .store(HANDSHAKE_IDLE, Ordering::Release);
            control.client_pid.store(0, Ordering::Release);
            control.announce_client_start(0);
            control.slot_claim.store(CLAIM_FREE, Ordering::Release);
            // Живой A захватил канал и поставил заявку.
            let a = Hello::post(None, &a_name, true, ClaimMode::Take).unwrap();
            *a_slot.borrow_mut() = Some(a);
        });
        assert_eq!(server.complete_handshake(), Ok(()), "заявка живого A снята");
        let a = slot.borrow_mut().take().unwrap();
        assert_eq!(server.peer_pid(), Some(std::process::id()));
        assert!(matches!(
            a.wait_ack(Duration::from_secs(1), None),
            AckWait::Accepted
        ));
        let client = a.finish();
        assert_eq!(server.session_claim(), client.claim());
    }

    /// Fix round 2: окно S2 между CAS `HELLO -> IDLE` и снятием claim-а
    /// умершего. Клиент лобби с брошенным мьютексом видит `IDLE` и
    /// перехватывает claim (`TakeOverAbandoned`), записывая своё
    /// самоописание; сервер не должен его затереть -- обнуление полей
    /// умершего идёт ДО CAS. Затем сервер принимает перехватившего и
    /// наблюдает его процесс.
    #[test]
    fn dead_hello_withdrawal_does_not_clobber_takeover() {
        let name = unique("DEADTAKEOVER");
        let mut server = SharedServer::start(&name).unwrap();
        let dead = dead_process();
        post_foreign_hello(&server, dead, dead);
        let slot: Rc<RefCell<Option<Hello>>> = Rc::default();
        let (n_slot, n_name) = (slot.clone(), name);
        set_hook(HandshakePoint::AfterDeadWithdraw, move |_| {
            let n = Hello::post(None, &n_name, true, ClaimMode::TakeOverAbandoned).unwrap();
            *n_slot.borrow_mut() = Some(n);
        });
        assert_eq!(server.complete_handshake(), Err(ShmError::HandshakeFailed));
        let n = slot.borrow_mut().take().expect("перехват не состоялся");
        let control = server.view().control_block();
        assert_eq!(control.slot_claim.load(Ordering::Acquire), n.claim());
        assert_eq!(
            control.client_pid.load(Ordering::Acquire),
            std::process::id(),
            "самоописание перехватившего затёрто"
        );
        assert_ne!(control.client_start_time(), 0);
        assert_eq!(
            control.client_state.load(Ordering::Acquire),
            HANDSHAKE_CLIENT_HELLO
        );
        assert_eq!(server.complete_handshake(), Ok(()));
        assert_eq!(server.peer_pid(), Some(std::process::id()));
        assert!(matches!(
            n.wait_ack(Duration::from_secs(1), None),
            AckWait::Accepted
        ));
        let client = n.finish();
        assert_eq!(server.session_claim(), client.claim());
    }

    /// §7.1 п. 8: PID заявки не совпадает с владельцем claim-а (ABA G2: PID
    /// прочитан у прежнего клиента) -- S2 не срабатывает, смерть P не
    /// снимает заявку живого владельца.
    #[test]
    fn dead_hello_of_other_pid_not_withdrawn() {
        let name = unique("DEADOTHER");
        let mut server = SharedServer::start(&name).unwrap();
        let me = (std::process::id(), crate::win::current_process_start_time());
        post_foreign_hello(&server, dead_process(), me);
        let generation = server
            .view()
            .control_block()
            .generation
            .load(Ordering::Acquire);
        assert_eq!(server.complete_handshake(), Ok(()));
        let control = server.view().control_block();
        assert_ne!(control.generation.load(Ordering::Acquire), generation);
        assert_eq!(
            control.client_state.load(Ordering::Acquire),
            HANDSHAKE_SERVER_READY
        );
        assert_eq!(control.slot_claim.load(Ordering::Acquire), 0x0BAD_C1A1);
        assert_eq!(server.session_claim(), 0x0BAD_C1A1);
    }

    /// §7.1 п. 9 (§2.3): протухший захват без заявки отнимается только по
    /// толчку проигравшего клиента и не раньше `reserve_timeout` от первого
    /// наблюдения; без толчка сервер не просыпается.
    #[test]
    fn stale_claim_reclaimed_on_nudge_only() {
        use crate::win::Mapping;
        let name = unique("STALECLAIM");
        let mut server = SharedServer::start(&name).unwrap();
        server.set_reserve_timeout(Duration::from_millis(200));
        let wakeups = server.wait_wakeups();
        let mapping = Mapping::open_header(ObjectPath::mapping(None, &name)).unwrap();
        let control = SharedView::peek_control(&mapping).unwrap();
        control.slot_claim.store(0x5747_0001, Ordering::Release);
        let waiter = std::thread::spawn(move || {
            let wake = EventHandle::create_unnamed(false).unwrap();
            let result = server.wait_for_client_or(wake.as_handle(), Some(Duration::from_secs(10)));
            (result, server)
        });
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(wakeups.load(Ordering::Acquire), 0, "сервер не спал");

        let started = Instant::now();
        let err = SharedClient::connect(&name, Duration::from_secs(2)).unwrap_err();
        assert_eq!(err, ShmError::AlreadyConnected);
        assert!(started.elapsed() < Duration::from_millis(100));
        let deadline = Instant::now() + Duration::from_secs(2);
        while wakeups.load(Ordering::Acquire) == 0 {
            assert!(Instant::now() < deadline, "толчок не разбудил сервер");
            std::thread::sleep(Duration::from_millis(5));
        }
        // Первое наблюдение -- не отъём.
        assert_eq!(control.slot_claim.load(Ordering::Acquire), 0x5747_0001);

        std::thread::sleep(Duration::from_millis(250));
        let err = SharedClient::connect(&name, Duration::from_secs(2)).unwrap_err();
        assert_eq!(err, ShmError::AlreadyConnected);
        let deadline = Instant::now() + Duration::from_secs(2);
        while control.slot_claim.load(Ordering::Acquire) != 0 {
            assert!(Instant::now() < deadline, "протухший захват не отнят");
            std::thread::sleep(Duration::from_millis(5));
        }
        let client = SharedClient::connect(&name, Duration::from_secs(2)).unwrap();
        let (result, server) = waiter.join().unwrap();
        assert_eq!(result, Ok(true));
        assert!(server.is_connected() && client.is_connected());
    }
}

#[cfg(test)]
mod stray_tests {
    use super::*;
    use crate::client::SharedClient;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;

    fn unique(tag: &str) -> String {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("XSHM_SRV_{tag}_{}_{}", std::process::id(), ts % 1_000_000)
    }

    /// Находка 21: `CONNECT_REQ` без `CLIENT_HELLO` (толчок, откатившийся
    /// клиент, чужой процесс) не обрывает ожидание `HandshakeFailed` --
    /// сервер дожидается настоящего клиента в пределах таймаута.
    #[test]
    fn stray_connect_req_does_not_fail_wait() {
        let name = unique("STRAY");
        let mut server = SharedServer::start(&name).unwrap();
        let waiter = std::thread::spawn(move || {
            let result = server.wait_for_client(Some(Duration::from_secs(2)));
            (result, server)
        });
        // Даём серверу сесть на `CONNECT_REQ`, затем ложный сигнал без HELLO.
        std::thread::sleep(Duration::from_millis(30));
        let events = SharedEvents::open(&name).unwrap();
        events.connect_req.set().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let client = SharedClient::connect(&name, Duration::from_secs(2)).unwrap();
        let (result, server) = waiter.join().unwrap();
        assert_eq!(result, Ok(()));
        assert!(server.is_connected());
        assert!(client.is_connected());
    }

    /// То же для `wait_for_client_or` (worker Auto, лобби Dispatch): ложный
    /// сигнал не даёт ни ошибки, ни `Ok(false)` -- ожидание продолжается.
    #[test]
    fn stray_connect_req_does_not_end_wait_or() {
        let name = unique("STRAYOR");
        let mut server = SharedServer::start(&name).unwrap();
        let waiter = std::thread::spawn(move || {
            let wake = EventHandle::create_unnamed(false).unwrap();
            let result = server.wait_for_client_or(wake.as_handle(), Some(Duration::from_secs(2)));
            (result, server)
        });
        std::thread::sleep(Duration::from_millis(30));
        let events = SharedEvents::open(&name).unwrap();
        events.connect_req.set().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let client = SharedClient::connect(&name, Duration::from_secs(2)).unwrap();
        let (result, server) = waiter.join().unwrap();
        assert_eq!(result, Ok(true));
        assert!(server.is_connected());
        assert!(client.is_connected());
    }

    /// Ложный сигнал при коротком таймауте: остаток пересчитывается, итог --
    /// `Timeout`, а не `HandshakeFailed` и не ожидание сверх дедлайна.
    #[test]
    fn stray_connect_req_keeps_the_deadline() {
        let name = unique("STRAYDL");
        let mut server = SharedServer::start(&name).unwrap();
        server.events().unwrap().connect_req.set().unwrap();
        let started = Instant::now();
        let result = server.wait_for_client(Some(Duration::from_millis(200)));
        let elapsed = started.elapsed();
        assert_eq!(result, Err(ShmError::Timeout));
        assert!(elapsed >= Duration::from_millis(190), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
    }

    /// Поток-атакующий: взводит `event` без пауз, пока `run` истинно.
    fn flood(event: Arc<EventHandle>, run: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            while run.load(Ordering::Acquire) {
                let _ = event.set();
            }
        })
    }

    /// Task 10 (П1): поток пустых `CONNECT_REQ` (два атакующих без пауз и
    /// «противник, всегда выигрывающий гонку» -- `ADVERSARY_MARK`: сигнал
    /// снова взведён к каждому ожиданию) не голодит `wake` --
    /// `wait_for_client_or` возвращает `Ok(false)` вскоре после `wake`, а
    /// не ждёт конца потока. 20 повторов.
    #[test]
    fn wake_not_starved_by_connect_req_flood() {
        let name = unique("WAKEFLOOD");
        let mut server = SharedServer::start(&name).unwrap();
        server
            .view()
            .control_block()
            .slot_count
            .store(ADVERSARY_MARK, Ordering::Release);
        let connect_req = Arc::new(SharedEvents::open(&name).unwrap().connect_req);
        let wake = Arc::new(EventHandle::create_unnamed(false).unwrap());
        let run = Arc::new(AtomicBool::new(true));
        let attackers: Vec<_> = (0..2)
            .map(|_| flood(Arc::clone(&connect_req), Arc::clone(&run)))
            .collect();
        let (tx, rx) = mpsc::channel();
        let server_wake = Arc::clone(&wake);
        let waiter = std::thread::spawn(move || {
            for _ in 0..20 {
                let result = server.wait_for_client_or(server_wake.as_handle(), None);
                if tx.send(result).is_err() {
                    break;
                }
            }
        });
        let mut worst = Duration::ZERO;
        for round in 0..20 {
            std::thread::sleep(Duration::from_millis(50));
            let started = Instant::now();
            wake.set().unwrap();
            let result = rx.recv_timeout(Duration::from_secs(5));
            let elapsed = started.elapsed();
            if result.is_err() {
                // До исправления: снять противника, чтобы поток завершился.
                run.store(false, Ordering::Release);
                let mapping = Mapping::open_header(ObjectPath::mapping(None, &name)).unwrap();
                let control = SharedView::peek_control(&mapping).unwrap();
                control.slot_count.store(0, Ordering::Release);
            }
            assert_eq!(result, Ok(Ok(false)), "повтор {round}");
            worst = worst.max(elapsed);
        }
        run.store(false, Ordering::Release);
        for attacker in attackers {
            attacker.join().unwrap();
        }
        waiter.join().unwrap();
        assert!(worst <= Duration::from_millis(200), "{worst:?}");
    }

    /// Task 10 (П2, страж): поток `wake` (команды worker-а) не голодит
    /// заявку -- индекс 0 у `wake` не скрывает `CONNECT_REQ`: исход решает
    /// состояние (`hello_pending`). Детерминированно: `wake` взводится и
    /// перед каждым ожиданием (fix round 1, M1).
    #[test]
    fn hello_accepted_under_wake_storm() {
        let name = unique("WAKESTORM");
        let mut server = SharedServer::start(&name).unwrap();
        let wake = Arc::new(EventHandle::create_unnamed(false).unwrap());
        let run = Arc::new(AtomicBool::new(true));
        let storm = flood(Arc::clone(&wake), Arc::clone(&run));
        let server_wake = Arc::clone(&wake);
        let waiter = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                // Fix round 1 (M1): `wake` взведён к КАЖДОМУ ожиданию --
                // без решения по состоянию (`hello_pending` до индекса)
                // ожидание всегда возвращало бы `Ok(false)`.
                server_wake.set().unwrap();
                let result = server.wait_for_client_or(server_wake.as_handle(), None);
                if result != Ok(false) || Instant::now() >= deadline {
                    return (result, server);
                }
            }
        });
        std::thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        let client = SharedClient::connect(&name, Duration::from_secs(2));
        let elapsed = started.elapsed();
        run.store(false, Ordering::Release);
        storm.join().unwrap();
        let (result, server) = waiter.join().unwrap();
        let client = client.unwrap();
        assert_eq!(result, Ok(true));
        assert!(server.is_connected() && client.is_connected());
        assert!(elapsed <= Duration::from_millis(500), "{elapsed:?}");
    }

    /// Task 10 (П3): пустой `CONNECT_REQ` -- одно пробуждение и ни одной
    /// записи: 100 сигналов -- 100 пробуждений, `slot_claim`,
    /// `client_state`, `generation` не изменились, рукопожатия не было.
    #[test]
    fn stray_connect_req_costs_one_wakeup() {
        let name = unique("STRAYCOST");
        let mut server = SharedServer::start(&name).unwrap();
        let wakeups = server.wait_wakeups();
        let events = SharedEvents::open(&name).unwrap();
        let mapping = Mapping::open_header(ObjectPath::mapping(None, &name)).unwrap();
        let control = SharedView::peek_control(&mapping).unwrap();
        let before = (
            control.slot_claim.load(Ordering::Acquire),
            control.client_state.load(Ordering::Acquire),
            control.generation.load(Ordering::Acquire),
        );
        let wake = Arc::new(EventHandle::create_unnamed(false).unwrap());
        let server_wake = Arc::clone(&wake);
        let waiter = std::thread::spawn(move || {
            let result = server.wait_for_client_or(server_wake.as_handle(), None);
            (result, server)
        });
        std::thread::sleep(Duration::from_millis(30));
        for round in 1..=100u64 {
            events.connect_req.set().unwrap();
            let deadline = Instant::now() + Duration::from_millis(100);
            while wakeups.load(Ordering::Acquire) < round {
                assert!(
                    Instant::now() < deadline,
                    "сигнал {round} не разбудил сервер"
                );
                std::thread::yield_now();
            }
        }
        // Последнее пробуждение могло ещё не дойти до нового ожидания.
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(wakeups.load(Ordering::Acquire), 100);
        wake.set().unwrap();
        let (result, server) = waiter.join().unwrap();
        assert_eq!(result, Ok(false));
        assert!(!server.is_connected());
        let after = (
            control.slot_claim.load(Ordering::Acquire),
            control.client_state.load(Ordering::Acquire),
            control.generation.load(Ordering::Acquire),
        );
        assert_eq!(after, before);
    }
}

#[cfg(test)]
mod poll_tests {
    //! Синхронное ожидание данных (`poll_client*`) видит штатный уход
    //! клиента (`DISCONNECT`) так же, как `wait_for_space`: сначала
    //! дочитать кольцо, потом `Err(NotConnected)` и пометка отключения.

    use super::*;
    use crate::client::SharedClient;
    use crate::ring::DATA_KERNEL_WAITS;

    fn unique(tag: &str) -> String {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("XSHM_POLL_{tag}_{}_{}", std::process::id(), ts % 1_000_000)
    }

    /// Сервер принимает клиента в отдельном потоке.
    fn accept(mut server: SharedServer, name: &str) -> (SharedServer, SharedClient) {
        let waiter = std::thread::spawn(move || {
            server
                .wait_for_client(Some(Duration::from_secs(5)))
                .unwrap();
            server
        });
        let client = SharedClient::connect(name, Duration::from_secs(5)).unwrap();
        (waiter.join().unwrap(), client)
    }

    fn data_waits() -> u32 {
        DATA_KERNEL_WAITS.with(Cell::get)
    }

    /// Клиент записал три кадра и ушёл (L: `IDLE`, потом `DISCONNECT`):
    /// сервер дочитывает их байт-в-байт, затем `poll_client` -- сразу
    /// `NotConnected`, сторона помечена отключённой и принимает нового
    /// клиента.
    #[test]
    fn poll_client_sees_graceful_leave_after_drain() {
        let name = unique("LEAVE");
        let server = SharedServer::start(&name).unwrap();
        let (mut server, client) = accept(server, &name);
        let frames: [&[u8]; 3] = [b"first", b"second-frame", b"third!!"];
        for frame in frames {
            client.send_to_server(frame).unwrap();
        }
        drop(client);

        assert_eq!(server.poll_client(Some(Duration::from_secs(5))), Ok(true));
        let mut buf = Vec::new();
        for frame in frames {
            assert_eq!(server.receive_from_client(&mut buf), Ok(frame.len()));
            assert_eq!(buf, frame);
        }
        let started = Instant::now();
        assert_eq!(
            server.poll_client(Some(Duration::from_secs(5))),
            Err(ShmError::NotConnected)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(!server.is_connected());
        let started = Instant::now();
        assert_eq!(
            server.poll_client(Some(Duration::from_secs(5))),
            Err(ShmError::NotConnected)
        );
        assert!(started.elapsed() < Duration::from_millis(100));

        // Канал свободен: второй клиент принят, claim -- его.
        let (server, client) = accept(server, &name);
        assert!(server.is_connected());
        assert_eq!(
            server
                .view()
                .control_block()
                .slot_claim
                .load(Ordering::Acquire),
            client.claim()
        );
    }

    /// Главный путь: сервер уже спит в `poll_client(None)` (без таймаута),
    /// когда клиент уходит штатно. Одно ожидание в ядре, `NotConnected`,
    /// сторона помечена, `DISCONNECT` возвращён взведённым.
    #[test]
    fn blocked_poll_client_wakes_on_client_leave() {
        let name = unique("BLOCKED");
        let server = SharedServer::start(&name).unwrap();
        let (mut server, client) = accept(server, &name);
        let poller = std::thread::spawn(move || {
            let before = data_waits();
            let result = server.poll_client(None);
            (result, data_waits() - before, Instant::now(), server)
        });
        std::thread::sleep(Duration::from_millis(100)); // сервер уже спит
        let left_at = Instant::now();
        drop(client); // L: IDLE, потом DISCONNECT
        let (result, waits, woke, server) = poller.join().unwrap();
        assert_eq!(result, Err(ShmError::NotConnected));
        assert_eq!(waits, 1, "одно ожидание в ядре");
        assert!(woke.duration_since(left_at) < Duration::from_secs(1));
        assert!(!server.is_connected());
        assert!(
            server
                .events()
                .unwrap()
                .disconnect
                .wait(Some(Duration::ZERO))
                .unwrap(),
            "сигнал отключения возвращён взведённым"
        );
    }

    /// Подключённая пара без трафика: одно ожидание в ядре на весь
    /// таймаут, без нарезки и без лишних пробуждений.
    #[test]
    fn poll_client_idle_is_single_kernel_wait() {
        let name = unique("IDLE");
        let server = SharedServer::start(&name).unwrap();
        let (mut server, _client) = accept(server, &name);
        let before = data_waits();
        let started = Instant::now();
        assert_eq!(
            server.poll_client(Some(Duration::from_millis(200))),
            Ok(false)
        );
        assert!(started.elapsed() >= Duration::from_millis(190));
        assert_eq!(data_waits() - before, 1);
        assert!(server.is_connected());
    }

    /// Anonymous-сервер (событий нет): клиент вернул `client_state` в
    /// `IDLE` -- при пустом кольце `poll_client` заканчивает сессию.
    #[test]
    fn anonymous_poll_sees_client_state_idle() {
        let mut server = SharedServer::start_anonymous().unwrap();
        let control = server.view().control_block();
        control
            .client_state
            .store(HANDSHAKE_CLIENT_HELLO, Ordering::Release);
        server
            .wait_for_client_noevent(Some(Duration::from_secs(1)))
            .unwrap();
        assert!(server.is_connected());
        assert_eq!(server.poll_client(Some(Duration::ZERO)), Ok(false));
        server
            .view()
            .control_block()
            .client_state
            .store(HANDSHAKE_IDLE, Ordering::Release);
        assert_eq!(
            server.poll_client(Some(Duration::ZERO)),
            Err(ShmError::NotConnected)
        );
        assert!(!server.is_connected());
    }
}

#[cfg(test)]
mod presence_tests {
    //! Task 11 надёжности (R19): маяк присутствия сервера взводится после
    //! публикации секции и всех событий и сбрасывается первым в `Drop`.

    use super::*;
    use crate::beacon::Beacon;
    use crate::client::SharedClient;
    use crate::events::SharedEvents;

    fn unique(tag: &str) -> String {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        format!(
            "SRV_PRES_{tag}_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Клиент, проснувшийся по маяку, застаёт секцию и все 7 событий (ни
    /// одного `NOT_FOUND`); после `drop(server)` маяк сброшен, а клиент
    /// подключённой сессии видит `NotConnected`, а не исчезнувший канал.
    #[test]
    fn presence_raised_after_publish_lowered_first_in_drop() {
        for round in 0..200 {
            let name = unique(&format!("ORDER{round}"));
            let waiter = {
                let name = name.clone();
                std::thread::spawn(move || {
                    let beacon = Beacon::open(&name).unwrap();
                    assert!(
                        beacon.wait(Some(Duration::from_secs(5))).unwrap(),
                        "маяк не взведён"
                    );
                    Mapping::open(ObjectPath::mapping(None, &name)).expect("секция");
                    SharedEvents::open_in(None, &name).expect("события");
                    beacon
                })
            };
            // Дать ждущему открыть маяк раньше сервера (порядок любой).
            if round % 2 == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
            let server = SharedServer::start(&name).unwrap();
            let beacon = waiter.join().unwrap();
            assert!(beacon.is_raised().unwrap());
            drop(server);
            assert!(!beacon.is_raised().unwrap(), "маяк остался после Drop");
        }

        let name = unique("SESSION");
        let mut server = SharedServer::start(&name).unwrap();
        let client = {
            let name = name.clone();
            std::thread::spawn(move || SharedClient::connect(&name, Duration::from_secs(5)))
        };
        server
            .wait_for_client(Some(Duration::from_secs(5)))
            .unwrap();
        let client = client.join().unwrap().unwrap();
        let beacon = Beacon::open(&name).unwrap();
        assert!(beacon.is_raised().unwrap());
        drop(server);
        assert!(!beacon.is_raised().unwrap());
        assert_eq!(
            client.send_to_server(b"late").err(),
            Some(ShmError::NotConnected)
        );
    }
}
