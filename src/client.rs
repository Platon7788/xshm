use std::future::Future;
use std::marker::PhantomData;
use std::os::windows::io::{AsHandle, BorrowedHandle};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use crate::asynch::{Endpoint, OP, PEER, RecvFuture, SendFuture, WaitSet};
use crate::constants::{
    CLAIM_FREE, HANDSHAKE_CLIENT_HELLO, HANDSHAKE_IDLE, HANDSHAKE_SERVER_READY,
};
use crate::error::{DisconnectReason, Result, ShmError};
use crate::events::SharedEvents;
use crate::layout::ChannelLayout;
use crate::namespace::PrivateNamespace;
use crate::naming::ObjectPath;
use crate::presence::{
    Attempt, Busy, FreeNotifier, FreeWait, FreeWaiter, WaitCtl, release_claim_notify,
    release_own_claim,
};
use crate::ring::{DisconnectWatch, FreeSpace, RingBuffer, SpinPolicy, WriteOutcome};
use crate::shared::SharedView;
use crate::win::{EventHandle, Mapping, ProcessWatch, wait_any};

#[cfg(test)]
thread_local! {
    /// Тесты: подменить заявляемое время создания процесса (имитация
    /// подделанного или устаревшего `client_start`).
    pub(crate) static FORGED_START_TIME: core::cell::Cell<Option<u64>> =
        const { core::cell::Cell::new(None) };
}

#[cfg(test)]
thread_local! {
    /// Тесты (Task 6 надёжности): одноразовый хук `receive_from_server`
    /// между сверкой сессии на входе и чтением кольца -- играет роль
    /// сервера, успевшего за это окно закрыть сессию и принять другого
    /// клиента.
    pub(crate) static BEFORE_READ: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Тесты: тип хука `AFTER_HELLO`.
#[cfg(test)]
pub(crate) type HelloHook = Box<dyn FnOnce(&SharedView)>;

#[cfg(test)]
thread_local! {
    /// Тесты (решение X): одноразовый хук `Hello::post` сразу после заявки
    /// (C7), до открытия процесса сервера -- играет роль сервера, успевшего
    /// зафиксировать рукопожатие и умереть в этом окне.
    pub(crate) static AFTER_HELLO: std::cell::RefCell<Option<HelloHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
thread_local! {
    /// Тесты (fix round 1, I1 (a)): одноразовый хук `Hello::leave_robbed`
    /// после возврата claim-а, до ухода L.
    pub(crate) static BEFORE_ROB_LEAVE: std::cell::RefCell<Option<HelloHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
thread_local! {
    /// Тесты (решение AB): одноразовый хук ухода L между сверкой
    /// `generation` и CAS -- играет роль сервера, отнявшего claim «не
    /// заметившего» клиента и принявшего следующего.
    pub(crate) static BEFORE_LEAVE_CAS: std::cell::RefCell<Option<HelloHook>> =
        const { std::cell::RefCell::new(None) };
}

/// Время создания текущего процесса, заявляемое в `client_start_*`.
fn announced_start_time() -> u64 {
    #[cfg(test)]
    if let Some(forged) = FORGED_START_TIME.with(core::cell::Cell::get) {
        return forged;
    }
    crate::win::current_process_start_time()
}

/// Отозвать заявку `CLIENT_HELLO` (шаг W, ревизия 3; INTEROP R15): CAS
/// `client_state: CLIENT_HELLO -> IDLE` и разбор наблюдённого значения.
///
/// Точка фиксации рукопожатия -- `client_state`: сервер фиксирует его CAS
/// `CLIENT_HELLO -> SERVER_READY` (`SharedServer::complete_handshake`, S5),
/// клиент отзывает встречным CAS; выигрывает ровно один. Ревизия 3: заявку
/// ставит только держатель claim (C6 -- CAS из `IDLE`), поэтому стоящий
/// `CLIENT_HELLO` и наблюдённый `SERVER_READY` -- наши.
///
/// `true` -- отзывать поздно, рукопожатие состоялось: наблюдён
/// `SERVER_READY` (сервер зафиксировал нашу заявку) или `IDLE` при
/// сменившемся `generation` (зафиксировал и уже отключил: `DISCONNECT`
/// взведён, worker дочитает кольцо). `false` -- заявка отозвана (или её
/// снял сервер без рукопожатия: уход, заявка «умершего» S2) и откатана:
/// PID/время клиента (`announced` -- своё самоописание C5; CAS от него,
/// follow-up N3) и заголовки -- в `IDLE`. Claim здесь не трогается -- его
/// освобождает `Hello::withdraw` (после отката, R18).
fn withdraw_hello(
    view: &SharedView,
    generation_before: u32,
    announced: Option<(u32, u64)>,
) -> bool {
    let control = view.control_block();
    match control.client_state.compare_exchange(
        HANDSHAKE_CLIENT_HELLO,
        HANDSHAKE_IDLE,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Err(HANDSHAKE_SERVER_READY) => return true,
        Err(HANDSHAKE_IDLE) if control.generation.load(Ordering::Acquire) != generation_before => {
            return true;
        }
        // Отозвали сами, сервер снял заявку без рукопожатия или мусор.
        _ => {}
    }
    // Откат. Заявленные PID и время процесса отзываем вместе с HELLO: иначе
    // клиент старой версии, пришедший следом, унаследовал бы их (см.
    // `ControlBlock::client_pid`). Follow-up (решение AG, N3): CAS от своих
    // значений, а не store нуля -- после отъёма claim-а (R-O, протухший
    // захват) следующий заявитель мог уже записать своё самоописание (C5).
    if let Some((pid, start)) = announced {
        control.retract_client(pid, start);
    }
    let (header_a, header_b) = view.headers();
    header_a
        .handshake_state
        .store(HANDSHAKE_IDLE, Ordering::Release);
    header_b
        .handshake_state
        .store(HANDSHAKE_IDLE, Ordering::Release);
    false
}

/// Сигнал `S2C_CONNECT` -- ответ на нашу заявку (C10, ревизия 3).
///
/// Принят: `client_state == SERVER_READY` (сервер зафиксировал нашу заявку;
/// по I2 другой заявки нет) или `IDLE` при `generation != g0` (зафиксировал
/// и уже отключил).
///
/// Отклонение от дизайна §2.1 C10 (найдено моделью `handshake_model`):
/// сигнал при стоящей заявке (`CLIENT_HELLO`) -- всегда устаревший, даже
/// при `generation != g0`. Это S7 прошлой сессии (её клиент принят по
/// проигранному отзыву и уже ушёл, а мы -- его следующая попытка или
/// следующий держатель), а сервер успел опубликовать `generation` новой
/// сессии (S3), но ещё не зафиксировал её (S5). Правило дизайна засчитало
/// бы незафиксированное рукопожатие: уход L освободил бы claim при стоящем
/// `CLIENT_HELLO`, и сервер зафиксировал бы сессию ушедшего клиента.
fn accepted_on_signal(view: &SharedView, generation_before: u32) -> bool {
    let control = view.control_block();
    match control.client_state.load(Ordering::Acquire) {
        HANDSHAKE_SERVER_READY => true,
        HANDSHAKE_IDLE => control.generation.load(Ordering::Acquire) != generation_before,
        _ => false,
    }
}

/// Как клиент владеет заявкой `ControlBlock::slot_claim` (ревизия 3, R18).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClaimMode {
    /// Захватить claim своим токеном (CAS `0 -> T`), освободить при отзыве
    /// и уходе -- последним шагом.
    Take,
    /// Claim уже захвачен токеном (Multi: `try_claim_slot`): рукопожатие
    /// его не берёт и не освобождает -- снимает `release_claim` Multi.
    Held(u32),
    /// Как `Take`, но мьютекс лобби достался брошенным (прежний держатель
    /// умер): проигранный CAS `0 -> T` повторяется как CAS `c -> T` над
    /// захватом умершего.
    TakeOverAbandoned,
    /// Тесты: черновой клиент ревизии 2 без claim (store `HELLO`).
    #[cfg(test)]
    Legacy,
}

/// Откуда заявка берёт уведомитель «claim освобождён» (Task 12, R20),
/// которым уведомляет о каждом своём освобождении claim-а (откат C6, C8,
/// отзыв W, уход L).
#[derive(Clone, Debug)]
pub(crate) enum FreeLink {
    /// Открыть `<имя>_free` канала на C0 (нет -- пир без R20, без
    /// уведомления).
    Open,
    /// Уже открытый вызывающим (worker `AutoClient`: один на жизнь worker-а;
    /// `None` -- у канала его нет).
    Given(Option<Arc<FreeNotifier>>),
    /// Без уведомления: лобби Dispatch (клиентов сериализует мьютекс лобби
    /// -- ждущих claim-а нет), выделенный канал Dispatch (без маяка и
    /// `_free`), `Held` Multi (claim освобождает перебор Multi).
    Off,
}

/// Прерывание ожидания `S2C_CONNECT` (ревизия 2): событие своего процесса
/// (`wake` worker-а) и проверка «пора остановиться». Сработал `wake`, а
/// `should_stop()` вернул `true` -- заявка отзывается, `connect` возвращает
/// `Err(NotReady)`; иначе (например, `wake` от `send`) ожидание продолжается.
pub(crate) struct Interrupt<'a> {
    pub(crate) wake: BorrowedHandle<'a>,
    pub(crate) should_stop: &'a mut dyn FnMut() -> bool,
}

/// Набор ожидания `S2C_CONNECT` с прерыванием (Task 10, INTEROP §e.1):
/// индекс `wake` вызывающего.
const WAKE: usize = 0;
/// Индексы объектов пира в наборе `[S2C_CONNECT, процесс сервера]` (с
/// прерыванием они сдвинуты на 1 -- за `wake`).
const PEER_ACK: usize = 0;
const PEER_PROCESS: usize = 1;

/// Чем кончилось ожидание `S2C_CONNECT`.
pub(crate) enum AckWait {
    /// Сервер принял именно нашу заявку (`accepted_on_signal`).
    Accepted,
    /// Таймаут, остановка или ошибка ожидания -- заявку отзывать.
    Abandoned(ShmError),
}

/// Клиент single-client канала.
///
/// Одна сторона SPSC-канала: тип `Send`, но НЕ `Sync` -- им пользуется один
/// поток за раз. Для работы из нескольких потоков -- `AutoClient`/
/// `MultiClient`/`DispatchClient` или обёртка в `Mutex<SharedClient>`.
///
/// ```compile_fail,E0277
/// fn share(client: &xshm::SharedClient) {
///     std::thread::scope(|s| {
///         s.spawn(|| client.try_send_to_server(b"a"));
///         s.spawn(|| client.try_send_to_server(b"b"));
///     });
/// }
/// ```
#[derive(Debug)]
pub struct SharedClient {
    view: SharedView,
    events: SharedEvents,
    ring_tx: RingBuffer,
    ring_rx: RingBuffer,
    connected: bool,
    /// Удерживаемый handle процесса сервера (сервер 0.8+ опубликовал PID и
    /// процесс удалось открыть) -- для детекции его смерти.
    peer: Option<ProcessWatch>,
    /// `generation` сессии: сервер публикует новый при каждом рукопожатии,
    /// так что расхождение значит «канал/слот уже у другой сессии».
    generation: u32,
    /// Токен claim-а сессии (ревизия 3; `CLAIM_FREE` -- без claim).
    claim: u32,
    /// Claim наш (`Take`): освобождается последним шагом ухода (L).
    owns_claim: bool,
    /// Смерть сервера ВЫВЕДЕНА, а не наблюдена (N1 повторного ревью T11b):
    /// `Drop` выполняет полный уход L и после отметки отключения
    /// (`PeerDied` от наблюдения «уже завершился»), чтобы живой сервер с
    /// испорченным временем в сегменте не остался в сессии навсегда.
    leave_on_drop: bool,
    /// Уведомитель «claim освобождён» (Task 12, R20): освобождение claim-а
    /// при уходе L -- с уведомлением ждущих.
    free: Option<Arc<FreeNotifier>>,
    /// Последним: view и кольца указывают в эту память.
    _mapping: Mapping,
    /// Делает тип `!Sync` (0.10, нулевой размер): кольцо -- SPSC, и
    /// `&self`-методы (`try_send_*`, `send_*`, `receive_*`) из двух потоков
    /// одновременно были бы гонкой данных на одной стороне канала. `Send`
    /// сохраняется (см. `unsafe impl Send` ниже): владение можно передать
    /// другому потоку, делить -- нельзя.
    _not_sync: PhantomData<std::cell::Cell<()>>,
}

// SAFETY: см. `SharedServer` -- владеющий `Mapping` плюс атомарные операции в
// shared memory, привязки к потоку нет.
unsafe impl Send for SharedClient {}

impl SharedClient {
    /// Подключиться к каналу `name`, ожидая ответа сервера не дольше
    /// `timeout`. Раскладку (размеры колец, лимиты) задаёт сервер -- она
    /// читается из сегмента (`layout`).
    ///
    /// Ошибки открытия: `NotReady` -- сервер ещё инициализирует сегмент,
    /// `VersionMismatch` -- сервер другого формата памяти, `Corrupted` --
    /// под именем чужой объект или повреждённая раскладка.
    ///
    /// Канал занят (0.10, ревизия 3 рукопожатия, INTEROP R18): другой клиент
    /// держит заявку или сессию -- `Err(AlreadyConnected)` сразу, без
    /// ожидания `timeout` и без записи в сегмент. Подключения к одному
    /// каналу сериализует сам канал (исключительный claim
    /// `ControlBlock::slot_claim`), внешняя сериализация не нужна. Повтор --
    /// на стороне вызывающего (`AutoClient` повторяет сам через
    /// `reconnect_delay`).
    ///
    /// Сервер, зафиксировавший рукопожатие и умерший раньше, чем клиент
    /// открыл его процесс (решение X): отзыв заявки проигран, сессия
    /// состоялась -- `connect` возвращает `Ok`, `is_peer_alive()` сразу
    /// `Some(false)`, а первое ожидание (`poll_server`, `recv_async`,
    /// `wait_for_space`) дочитывает кольцо (прощание сервера не теряется) и
    /// отвечает `Err(PeerDied)`. Остаток: PID сервера в сегменте занят живым
    /// процессом с другим временем создания, а заявку засчитал его
    /// `S2C_CONNECT`, -- неотличимо от живого сервера с испорченным временем
    /// (`peer_tests::corrupted_server_start_hi_is_not_a_live_server`):
    /// подключение без наблюдения процесса, границу задаёт таймаут
    /// вызывающего.
    ///
    /// Каждое освобождение claim-а этим клиентом (отзыв заявки, уход) --
    /// с уведомлением «claim освобождён» (0.10, INTEROP R20): клиент,
    /// ждущий занятый канал ([`SharedClient::wait_connectable`],
    /// `AutoClient`), просыпается сразу.
    pub fn connect(name: &str, timeout: Duration) -> Result<Self> {
        Self::connect_impl(
            None,
            name,
            timeout,
            true,
            ClaimMode::Take,
            FreeLink::Open,
            None,
        )
    }

    /// Подключиться к каналу `name` в частном пространстве имён `ns` (0.10;
    /// сервер -- `ChannelConfig::namespace` с той же границей). Имя
    /// относительное: без `Local\`/`Global\` и без `\` (иначе
    /// `InvalidConfig`). Пространство нужно только на время подключения:
    /// дальше клиент держит открытые объекты канала.
    ///
    /// Ошибки -- как у [`SharedClient::connect`]; канала в пространстве нет
    /// -- `WindowsError { code: STATUS_OBJECT_NAME_NOT_FOUND }`.
    pub fn connect_in(ns: &PrivateNamespace, name: &str, timeout: Duration) -> Result<Self> {
        Self::connect_impl(
            Some(ns),
            name,
            timeout,
            true,
            ClaimMode::Take,
            FreeLink::Open,
            None,
        )
    }

    /// Дождаться, пока канал `name` не станет свободным (0.10, Task 12,
    /// INTEROP R21), не подключаясь: `Ok(true)` -- канал был свободен в
    /// какой-то момент: claim свободен и ни заявки, ни сессии нет (дальше --
    /// [`SharedClient::connect`]; проигравший гонку другому клиенту получит
    /// `AlreadyConnected` и зовёт снова -- без горячего цикла: следующий
    /// вызов снова спит); `Ok(false)` -- истёк `timeout` (`None` -- без
    /// ограничения).
    ///
    /// Без опроса: ждущий спит в ядре на уведомлении «claim освобождён»
    /// `<имя>_free`, которое сервер создаёт вместе с маяком присутствия, а
    /// каждый, кто освобождает claim (клиент -- отзыв и уход, сервер --
    /// снятие умершего и протухшего захвата), взводит при наличии ждущих
    /// (R20), -- и на процессах держателя и сервера. Захват без заявки
    /// (держатель завис или умер до `CLIENT_HELLO`) ждущий толкает сам
    /// (`CONNECT_REQ`) и один раз повторно через `RESERVE_TIMEOUT + 1 с` --
    /// сервер снимает протухший захват. Сервер без `_free` (пир без R20) --
    /// проверка раз в 250 мс (резерв).
    ///
    /// Страховка (fix round 1, решение AE): каждое ожидание уведомления --
    /// не дольше `DiscoveryOptions::fallback` (по умолчанию 10 с; свои
    /// параметры -- [`SharedClient::wait_connectable_with`]), затем
    /// проверка канала: держатель-пир, освободивший claim без уведомления
    /// (нет R20), и занятость без claim-а (сессия клиента без claim)
    /// находятся в её пределах.
    ///
    /// Лобби Dispatch ждать так незачем: его клиентов сериализует мьютекс
    /// лобби, и освобождая claim, они уведомление не взводят -- ожидание
    /// проснётся только по страховке или по выходу процесса держателя.
    ///
    /// # Errors
    ///
    /// Канала нет или он не опубликован -- ошибка открытия
    /// (`WindowsError { code: STATUS_OBJECT_NAME_NOT_FOUND }`, `NotReady`);
    /// процесс сервера завершился во время ожидания -- `PeerDied`; ошибка
    /// ОС ожидания.
    pub fn wait_connectable(name: &str, timeout: Option<Duration>) -> Result<bool> {
        Self::wait_connectable_with(name, timeout, &crate::DiscoveryOptions::default())
    }

    /// [`SharedClient::wait_connectable`] с параметрами `options`:
    /// страховочная проверка (`fallback`, `None` -- только события; меньше
    /// 1 мс -- `InvalidConfig`) и частное пространство имён (`namespace`;
    /// `security` не используется -- ждущий объектов не создаёт).
    ///
    /// # Errors
    ///
    /// Как у [`SharedClient::wait_connectable`]; `InvalidConfig` --
    /// `fallback` меньше 1 мс.
    pub fn wait_connectable_with(
        name: &str,
        timeout: Option<Duration>,
        options: &crate::DiscoveryOptions,
    ) -> Result<bool> {
        crate::presence::wait_connectable(
            options.namespace.as_deref(),
            name,
            timeout,
            options.fallback,
        )
    }

    /// [`SharedClient::wait_connectable`] для канала в частном пространстве
    /// имён `ns`.
    ///
    /// # Errors
    ///
    /// Как у [`SharedClient::wait_connectable`].
    pub fn wait_connectable_in(
        ns: &PrivateNamespace,
        name: &str,
        timeout: Option<Duration>,
    ) -> Result<bool> {
        crate::presence::wait_connectable(
            Some(ns),
            name,
            timeout,
            crate::DiscoveryOptions::default().fallback,
        )
    }

    /// Дождаться сервера канала `name` (0.10, INTEROP R19), не подключаясь:
    /// `Ok(true)` -- секция канала опубликована, все 7 событий на месте и
    /// процесс сервера не завершился (дальше -- [`SharedClient::connect`]);
    /// `Ok(false)` -- истёк `timeout` (`None` -- ждать без ограничения).
    /// Параметры по умолчанию ([`DiscoveryOptions::default`](crate::DiscoveryOptions)),
    /// свои -- [`SharedClient::wait_for_server_with`].
    ///
    /// Без опроса: пока сервера нет, вызов спит в ядре на маяке присутствия
    /// `<имя>_beacon`, который каждый именованный сервер взводит после
    /// публикации канала. Существующий маяк открывается по DACL создателя;
    /// если ожидающий пришёл первым, он создаёт маяк с политикой
    /// `DiscoveryOptions::security` (по умолчанию `CurrentUser`) -- DACL
    /// задаёт первый создатель, поэтому при сервере другого пользователя
    /// (служба на `Global\`) задайте ту же политику, что у канала сервера.
    /// Маяк, оставленный упавшим сервером, гасится
    /// (`Beacon::clear_if_stale`), и ожидание продолжается. Резервная
    /// проверка канала -- не реже `DiscoveryOptions::fallback` (по
    /// умолчанию 10 с): сервер без маяка (пир без R19, маяк занят заранее)
    /// всё равно находится. Маяк недоступен (нет прав, нет привилегии
    /// `Global\`) -- проверка раз в 250 мс.
    ///
    /// # Errors
    ///
    /// Сервер есть, но подключиться к нему нельзя -- его ошибка открытия
    /// (`VersionMismatch`, `Corrupted`, `AccessDenied`, `InvalidConfig`),
    /// ошибка ОС ожидания.
    pub fn wait_for_server(name: &str, timeout: Option<Duration>) -> Result<bool> {
        Self::wait_for_server_with(name, timeout, &crate::DiscoveryOptions::default())
    }

    /// [`SharedClient::wait_for_server`] для канала в частном пространстве
    /// имён `ns` (сервер -- `ChannelConfig::namespace` с той же границей;
    /// маяк -- относительное имя в пространстве).
    ///
    /// # Errors
    ///
    /// Как у [`SharedClient::wait_for_server`].
    pub fn wait_for_server_in(
        ns: &PrivateNamespace,
        name: &str,
        timeout: Option<Duration>,
    ) -> Result<bool> {
        crate::presence::wait_for_server(
            Some(ns),
            name,
            timeout,
            &crate::DiscoveryOptions::default(),
        )
    }

    /// [`SharedClient::wait_for_server`] с параметрами `options`: политика
    /// маяка, если его создаёт ожидающий, резервная проверка
    /// (`fallback`, `None` -- только события) и частное пространство имён.
    ///
    /// # Errors
    ///
    /// Как у [`SharedClient::wait_for_server`]; некорректный
    /// `ObjectSecurity::Descriptor` -- маяк не открывается, работает
    /// резервная проверка.
    pub fn wait_for_server_with(
        name: &str,
        timeout: Option<Duration>,
        options: &crate::DiscoveryOptions,
    ) -> Result<bool> {
        crate::presence::wait_for_server(options.namespace.as_deref(), name, timeout, options)
    }

    /// Раскладка канала, выбранная сервером.
    #[must_use]
    pub const fn layout(&self) -> &ChannelLayout {
        self.view.layout()
    }

    /// Подключение, ожидание которого прерывает `interrupt` (worker-ы
    /// Auto/Dispatch/Multi: `stop`/Drop не ждут `connect_timeout`).
    pub(crate) fn connect_interruptible(
        namespace: Option<&PrivateNamespace>,
        name: &str,
        timeout: Duration,
        claim: ClaimMode,
        free: FreeLink,
        interrupt: Interrupt<'_>,
    ) -> Result<Self> {
        Self::connect_impl(namespace, name, timeout, true, claim, free, Some(interrupt))
    }

    /// `announce_pid = false` -- поведение клиента 0.7/0.8.0 без обмена PID
    /// (нужно тестам совместимости версий).
    ///
    /// Порядок (ревизия 3, INTEROP §d.2 шаг 3a, R15/R18; шаги C0-C10 --
    /// `Hello::post`): claim (CAS `0 -> T`, `claim` -- как владеть им) ->
    /// владелец -> `g0 = generation` -> сбросить `S2C_CONNECT` -> время
    /// процесса (`client_start_*`) -> PID (`client_pid`) -> CAS `client_state:
    /// IDLE -> CLIENT_HELLO` -> заголовки -> claim всё ещё наш ->
    /// `C2S_CONNECT_REQ` -> ждать `S2C_CONNECT`. Принят: `client_state ==
    /// SERVER_READY` или `IDLE` при `generation != g0` (сервер уже успел
    /// отключить нас: `DISCONNECT` взведён, и worker дочитает кольцо --
    /// прощальное сообщение не теряется в откате).
    pub(crate) fn connect_impl(
        namespace: Option<&PrivateNamespace>,
        name: &str,
        timeout: Duration,
        announce_pid: bool,
        claim: ClaimMode,
        free: FreeLink,
        mut interrupt: Option<Interrupt<'_>>,
    ) -> Result<Self> {
        let hello = Hello::post_with(namespace, name, announce_pid, claim, free, timeout)?;
        // Сервер подтверждённо завершился ещё до открытия его процесса
        // (решение U): `S2C_CONNECT` никто не пошлёт, а наблюдать нечего --
        // ожидание заняло бы весь таймаут (с `Duration::MAX` -- вечно).
        // Отзыв W -- как при таймауте; проигранный отзыв -- подключены.
        // C9: сигнал серверу, C10: ожидание ответа.
        let waited = if hello.server_exited {
            AckWait::Abandoned(ShmError::PeerDied)
        } else {
            match hello.events.connect_req.set() {
                Ok(()) => hello.wait_ack(timeout, interrupt.as_mut()),
                Err(err) => AckWait::Abandoned(err),
            }
        };
        // Отзыв проиграл CAS сервера -- рукопожатие состоялось: продолжаем
        // как подключённые (сервер ждёт нас; если уже отключил -- worker
        // увидит `DISCONNECT` и дочитает кольцо).
        match waited {
            AckWait::Accepted => Ok(hello.finish()),
            AckWait::Abandoned(err) if hello.withdraw() => Ok(hello.finish_lost(&err)),
            AckWait::Abandoned(err) => Err(err),
        }
    }

    /// Подключиться к каналу `name` асинхронно (0.10; рукопожатие -- как у
    /// [`SharedClient::connect`], ревизия 3): future захватывает claim и
    /// выставляет заявку (`CLIENT_HELLO` + `C2S_CONNECT_REQ`) при первом
    /// опросе и ждёт `S2C_CONNECT` в реакторе IOCP; подключение
    /// засчитывается по фиксации своей заявки сервером, устаревший сигнал
    /// поглощается. Канал занят -- `Err(AlreadyConnected)` на первом опросе.
    ///
    /// Процесс сервера наблюдается (PID и время создания из сегмента, как
    /// у `ProcessWatch::open_peer`): сервер умер, не ответив, --
    /// `Err(PeerDied)` с тем же откатом заявки, что у таймаута, в том числе
    /// если он умер раньше, чем future открыл его процесс. Сервер без
    /// PID (старая версия) или процесс, который не открылся (другая сессия
    /// без прав), не наблюдается -- тогда границу задаёт только таймаут
    /// исполнителя.
    ///
    /// Таймаута нет -- он у исполнителя. Брошенный future отзывает заявку
    /// так же, как синхронный таймаут: CAS `CLIENT_HELLO -> IDLE`, откат
    /// PID/заголовков и освобождение claim. Если сервер успел зафиксировать
    /// рукопожатие (его CAS выиграл), клиент уходит штатно, как `Drop`
    /// подключённого: `IDLE`, затем `DISCONNECT`, claim -- последним; сервер
    /// не остаётся «подключённым» к никому.
    /// Всё это -- атомарные операции и `NtSetEvent`, `Drop` не блокируется.
    ///
    /// Ошибки открытия -- как у `connect` (канала нет --
    /// `WindowsError { code: STATUS_OBJECT_NAME_NOT_FOUND }`).
    ///
    /// Любой исполнитель (здесь -- минимальный `block_on`; с tokio --
    /// `tokio::time::timeout(d, SharedClient::connect_async(name)).await
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
    /// let mut client = block_on(xshm::SharedClient::connect_async("MyChannel"))?;
    /// block_on(client.send_async(b"ping"))?;
    /// let mut buf = Vec::new();
    /// let len = block_on(client.recv_async(&mut buf))?;
    /// assert!(len > 0);
    /// # Ok::<(), xshm::ShmError>(())
    /// ```
    pub fn connect_async(name: &str) -> impl Future<Output = Result<Self>> + Send + 'static {
        ConnectFuture::new(None, name)
    }

    /// Подключиться асинхронно к каналу `name` в частном пространстве имён
    /// `ns` (0.10): то же рукопожатие, ожидание и откат брошенного future,
    /// что у [`SharedClient::connect_async`]; объекты канала ищутся, как у
    /// [`SharedClient::connect_in`] (сервер -- `ChannelConfig::namespace` с
    /// той же границей; его `accept_async` ждёт на событиях, открытых
    /// `start_with` в пространстве).
    ///
    /// `ns` -- по значению (`Arc`), чтобы future оставался `'static`
    /// (`tokio::spawn`). Future держит пространство до готовности (или до
    /// `Drop`); дальше подключённый клиент держит открытые объекты канала,
    /// handle пространства ему не нужен.
    ///
    /// Ошибки -- как у [`SharedClient::connect_in`], на первом опросе: имя с
    /// `Local\`/`Global\`, с `\` или пустое -- `InvalidConfig`; канала в
    /// пространстве нет -- `WindowsError { code: STATUS_OBJECT_NAME_NOT_FOUND }`;
    /// канал занят -- `AlreadyConnected`.
    pub fn connect_async_in(
        ns: Arc<PrivateNamespace>,
        name: &str,
    ) -> impl Future<Output = Result<Self>> + Send + 'static {
        ConnectFuture::new(Some(ns), name)
    }

    /// Принять сообщение сервера асинхронно (0.10; семантика --
    /// `SharedServer::recv_async`): отключение сервера -- `Err(NotConnected)`,
    /// смерть его процесса -- `Err(PeerDied)`, обе -- после дочитанного
    /// кольца, затем клиент отмечен неподключённым.
    ///
    /// Future выполняет операцию на каждом опросе: после `Ready` его не
    /// опрашивают снова (повторный опрос выполнит ещё одну операцию).
    pub fn recv_async<'a>(
        &'a mut self,
        buf: &'a mut Vec<u8>,
    ) -> impl Future<Output = Result<usize>> + Send + 'a {
        RecvFuture::new(self, buf)
    }

    /// Отправить сообщение серверу асинхронно без перезаписи (0.10;
    /// семантика -- `SharedServer::send_async`).
    ///
    /// Future выполняет операцию на каждом опросе: после `Ready` его не
    /// опрашивают снова (повторный опрос выполнит ещё одну операцию).
    pub fn send_async<'a>(
        &'a mut self,
        data: &'a [u8],
    ) -> impl Future<Output = Result<()>> + Send + 'a {
        SendFuture::new(self, data)
    }
}

/// Выставленная заявка клиента (`CLIENT_HELLO`), ответа сервера ещё нет:
/// общая часть синхронного и асинхронного подключения.
pub(crate) struct Hello {
    view: SharedView,
    events: SharedEvents,
    /// `generation` до заявки (`g0`, C3).
    generation_before: u32,
    /// Токен claim-а заявки (`CLAIM_FREE` -- без claim, только тесты).
    claim: u32,
    /// Claim взят этой заявкой (`Take`/`TakeOverAbandoned`) -- освобождается
    /// при отзыве и уходе; `Held` -- освобождает владелец (Multi).
    owns_claim: bool,
    /// Процесс сервера (PID и время создания из `ControlBlock`, Task 10):
    /// его смерть во время ожидания `S2C_CONNECT` -- `PeerDied`; тот же
    /// handle после рукопожатия наблюдает сервер подключённого клиента
    /// (`finish`). `None` -- сервер старой версии без PID или процесс не
    /// открылся (другая сессия без прав): тогда границу задаёт только
    /// таймаут.
    server: Option<ProcessWatch>,
    /// Уведомитель «claim освобождён» (Task 12, R20): освобождения заявки
    /// (откат C6, C8, отзыв W) -- с уведомлением; переходит клиенту.
    free: Option<Arc<FreeNotifier>>,
    /// Своё самоописание C5 (`client_pid`, `client_start`; `None` -- без
    /// обмена PID): отзыв W снимает его CAS-ом от этих значений (N3).
    announced: Option<(u32, u64)>,
    /// Сервер подтверждённо ушёл ещё до открытия его процесса
    /// (`is_process_gone`; `open_peer` не наблюдает мёртвый процесс). Им
    /// пользуется `connect_async` (`PeerDied` на первом опросе, Task 18);
    /// синхронное ожидание по-прежнему ограничено таймаутом: время сервера
    /// в сегменте -- данные пира, и при их порче (`peer_tests::
    /// corrupted_server_start_hi_is_not_a_live_server`) `connect`
    /// подключается без наблюдения, а не отказывает.
    server_gone: bool,
    /// Узкое «сервер завершился» для синхронного пути (fix round 1, решение
    /// U; `win::is_announced_process_exited`): процесса с PID сервера нет
    /// вовсе, либо он открылся, сигнален и время создания совпало с
    /// объявленным. Живой процесс с чужим временем сюда не попадает.
    /// `SharedClient::connect_impl` при `true` не ждёт `S2C_CONNECT` --
    /// `PeerDied` с отзывом заявки.
    server_exited: bool,
    /// Последним: view указывает в эту память.
    mapping: Mapping,
}

// SAFETY: как у `SharedClient` -- владеющий `Mapping` (view указывает в его
// память и переезжает вместе с ним) плюс атомарные операции в разделяемой
// памяти; привязки к потоку нет.
unsafe impl Send for Hello {}

impl std::fmt::Debug for Hello {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hello")
            .field("generation_before", &self.generation_before)
            .field("claim", &self.claim)
            .field("owns_claim", &self.owns_claim)
            .finish_non_exhaustive()
    }
}

/// Освободить свой claim (CAS `token -> CLAIM_FREE`) с уведомлением ждущих
/// `free` (Task 12, R20): если он уже не наш (отъём по дедлайну), -- no-op.
fn release_claim(view: &SharedView, token: u32, free: Option<&FreeNotifier>) {
    let _ = release_own_claim(view.control_block(), token, free);
}

/// Сколько ждать возврата claim-а на C8-провале, когда таймаута
/// подключения нет (`connect_async`, тесты `Hello::post`; follow-up N1):
/// синхронное подключение ждёт остаток своего таймаута. Claim держит
/// клиент, чей C6 упрётся в нашу зафиксированную заявку, -- он откатится и
/// освободит claim за микросекунды. Путь редкий: только после отъёма R-O
/// claim-а живого держателя (пир, не очищающий поля владельца, INTEROP §h
/// п. 12). Не дождались -- запасной уход (`Hello::leave_by_generation`).
const ROB_LEAVE_WAIT: Duration = Duration::from_millis(100);

/// Исход возврата claim-а на C8-провале (`Hello::retake`).
enum Retake {
    /// Сессии уже нет (`client_state != SERVER_READY`) -- уходить не из чего.
    Gone,
    /// Claim снова наш -- обычный уход L.
    Taken,
    /// Не дождались -- запасной уход по `generation`.
    TimedOut,
}

impl Hello {
    /// Открыть канал и выставить заявку (C0-C8 ревизии 3, INTEROP R15, R18;
    /// сигнал `CONNECT_REQ` -- C9 -- делает вызывающий).
    ///
    /// `Err(AlreadyConnected)` -- канал занят: claim держит другой клиент
    /// (тогда в сегмент не записано ничего, а сервер, не занятый сессией,
    /// получил толчок `CONNECT_REQ` -- §2.3 дизайна), заявка или сессия
    /// чужого клиента без claim (C6), или claim отняли до сигнала (C8).
    /// `Held`: провал C6 -- `Err(HandshakeFailed)` (Multi повторит захват).
    ///
    /// `free` (Task 12, R20) -- уведомитель «claim освобождён», которым
    /// заявка и клиент уведомляют о своих освобождениях claim-а;
    /// `FreeLink::Open` -- открыть `<имя>_free` здесь же, на C0 (его нет --
    /// без уведомления).
    pub(crate) fn post(
        namespace: Option<&PrivateNamespace>,
        name: &str,
        announce_pid: bool,
        claim: ClaimMode,
        free: FreeLink,
    ) -> Result<Self> {
        Self::post_with(namespace, name, announce_pid, claim, free, ROB_LEAVE_WAIT)
    }

    /// `post` с бюджетом возврата claim-а на C8-провале `retake` (follow-up
    /// N1: синхронное подключение -- остаток своего таймаута).
    pub(crate) fn post_with(
        namespace: Option<&PrivateNamespace>,
        name: &str,
        announce_pid: bool,
        claim: ClaimMode,
        free: FreeLink,
        retake: Duration,
    ) -> Result<Self> {
        // C0: открыть всё, не трогая память (ошибки -- без побочных эффектов).
        let mapping = Mapping::open(ObjectPath::mapping(namespace, name))?;
        // SAFETY: отображение длиной `mapping.size()` живёт, пока жив
        // `mapping`, а он переезжает в возвращаемую структуру вместе с view;
        // адрес view выровнен на гранулу выделения.
        let view = unsafe { SharedView::attach(mapping.as_ptr(), mapping.size()) }?;
        let control = view.control_block();
        let events = SharedEvents::open_in(namespace, name)?;
        let free = match free {
            FreeLink::Open => FreeNotifier::open(namespace, name).ok().map(Arc::new),
            FreeLink::Given(free) => free,
            FreeLink::Off => None,
        };

        // C1-C2: исключительный захват claim-а и владелец.
        let (token, owns_claim) = match claim {
            ClaimMode::Take | ClaimMode::TakeOverAbandoned => {
                let token = crate::shared::claim_token();
                let taken = match control.slot_claim.compare_exchange(
                    CLAIM_FREE,
                    token,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => true,
                    // Лобби: мьютекс брошен -- захват `held` принадлежит его
                    // умершему держателю (R12: claim лобби берут только под
                    // мьютексом), перехватываем. Только из `IDLE` (fix
                    // round 1): стоящие `CLIENT_HELLO`/`SERVER_READY`
                    // умершего разбирает сервер -- S2 (по его самоописанию и
                    // `slot_owner_pid`, которые перехват затёр бы в C2/C5)
                    // или наблюдение процесса; нам -- обычный отказ и толчок.
                    Err(held) => {
                        claim == ClaimMode::TakeOverAbandoned
                            && control.client_state.load(Ordering::Acquire) == HANDSHAKE_IDLE
                            && control
                                .slot_claim
                                .compare_exchange(held, token, Ordering::AcqRel, Ordering::Acquire)
                                .is_ok()
                    }
                };
                if !taken {
                    // Проигрыш: ничего не пишем. Толчок -- чтобы сервер,
                    // не занятый сессией, назначил дедлайн протухшему
                    // захвату (держатель мог умереть до `CLIENT_HELLO`).
                    if control.server_state.load(Ordering::Acquire) != HANDSHAKE_SERVER_READY {
                        let _ = events.connect_req.set();
                    }
                    return Err(ShmError::AlreadyConnected);
                }
                // Follow-up (решение AG, N2): claim был свободен, а заявка
                // или сессия уже стоит (держатель ограблен и умер, пир без
                // claim): C6 всё равно провалится. Не писать ни поля
                // владельца (C2), ни самоописание (C5) поверх -- по ним S2
                // опознаёт умершего; claim отпустить сразу (с уведомлением)
                // и, если стоит заявка, а сервер не в сессии, толкнуть его.
                let standing = control.client_state.load(Ordering::Acquire);
                if standing != HANDSHAKE_IDLE {
                    let _ = release_claim_notify(control, token, free.as_deref());
                    if standing == HANDSHAKE_CLIENT_HELLO
                        && control.server_state.load(Ordering::Acquire) != HANDSHAKE_SERVER_READY
                    {
                        let _ = events.connect_req.set();
                    }
                    return Err(ShmError::AlreadyConnected);
                }
                // C2: время своего процесса -- ДО PID, чья Release-запись
                // публикует обе половины (как `try_claim_slot` Multi). Время
                // -- настоящее, не заявляемое: по нему сервер подтверждает
                // смерть держателя (S2).
                control.announce_owner_start(crate::win::current_process_start_time());
                control
                    .slot_owner_pid
                    .store(std::process::id(), Ordering::Release);
                (token, true)
            }
            ClaimMode::Held(token) => (token, false),
            #[cfg(test)]
            ClaimMode::Legacy => (CLAIM_FREE, false),
        };
        let release = || {
            if owns_claim {
                release_claim(&view, token, free.as_deref());
            }
        };

        // C3: `generation` ДО заявки (`g0`).
        let generation_before = control.generation.load(Ordering::Acquire);
        // C4: `S2C_CONNECT`, оставшийся от чужого рукопожатия, иначе
        // «подтвердил» бы нашу заявку раньше сервера. Сбрасываем ДО
        // `CLIENT_HELLO`: ответ на нашу заявку может прийти только после.
        let _ = events.connect_ack.reset();

        // C5: время процесса, затем PID -- ДО `CLIENT_HELLO`: Release-CAS
        // заявки делает их видимыми серверу вместе с ней (находка 23).
        let announced = announce_pid.then(|| (std::process::id(), announced_start_time()));
        if let Some((pid, start)) = announced {
            control.announce_client_start(start);
            control.client_pid.store(pid, Ordering::Release);
        }

        // C6: заявка -- только CAS из `IDLE` (ревизия 3): чужие `HELLO` или
        // сессия (клиент без claim) не перетираются.
        let mut stale_hello = false;
        let posted = match claim {
            #[cfg(test)]
            ClaimMode::Legacy => {
                control
                    .client_state
                    .store(HANDSHAKE_CLIENT_HELLO, Ordering::Release);
                true
            }
            _ => match control.client_state.compare_exchange(
                HANDSHAKE_IDLE,
                HANDSHAKE_CLIENT_HELLO,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => true,
                Err(observed) => {
                    stale_hello = observed == HANDSHAKE_CLIENT_HELLO;
                    false
                }
            },
        };
        if !posted {
            // Follow-up (N3): своё самоописание -- CAS от своих значений.
            if let Some((pid, start)) = announced {
                control.retract_client(pid, start);
            }
            release();
            // Follow-up (решение AG, N2): claim был свободен, а заявка уже
            // стоит -- заявка без держателя (умерший после отъёма claim-а,
            // пир без claim). Снять её может только сервер (S2) и только на
            // `CONNECT_REQ`: толчок -- после своего освобождения claim-а (S2
            // заявки без claim-а судит по claim `0`), если сервер не в
            // сессии.
            if stale_hello && control.server_state.load(Ordering::Acquire) != HANDSHAKE_SERVER_READY
            {
                let _ = events.connect_req.set();
            }
            return Err(if matches!(claim, ClaimMode::Held(_)) {
                ShmError::HandshakeFailed
            } else {
                ShmError::AlreadyConnected
            });
        }

        // C7: заголовки.
        let (header_a, header_b) = view.headers();
        header_a
            .handshake_state
            .store(HANDSHAKE_CLIENT_HELLO, Ordering::Release);
        header_b
            .handshake_state
            .store(HANDSHAKE_CLIENT_HELLO, Ordering::Release);

        #[cfg(test)]
        if let Some(hook) = AFTER_HELLO.with(|hook| hook.borrow_mut().take()) {
            hook(&view);
        }

        // Процесс сервера (Task 10) -- по PID и времени создания из
        // сегмента; открывается после заявки, а не на C0, чтобы проигравший
        // захват (частый исход при занятом канале) не платил за открытие.
        // Сервер сегмента за время рукопожатия не меняется: это тот же
        // процесс, что наблюдает подключённый клиент (`finish`).
        let (server_pid, server_start) = (
            control.server_pid.load(Ordering::Acquire),
            control.server_start_time(),
        );
        let server = ProcessWatch::open_peer(server_pid, server_start);
        let server_gone = server.is_none() && crate::win::is_process_gone(server_pid, server_start);
        let server_exited =
            server_gone && crate::win::is_announced_process_exited(server_pid, server_start);

        let hello = Self {
            view,
            events,
            generation_before,
            claim: token,
            owns_claim,
            server,
            free,
            announced,
            server_gone,
            server_exited,
            mapping,
        };

        // C8: claim всё ещё наш? Нет -- его отняли: по дедлайну (держатель
        // завис между C1 и C6 дольше `RESERVE_TIMEOUT`, нарушение A1) или
        // быстрым путём R-O (Task 12: поля владельца в окне C1-C2 были
        // прежнего, умершего держателя). Отзыв; проигран -- сервер успел
        // зафиксировать заявку: вернуть claim и уйти (`leave_robbed`).
        if token != CLAIM_FREE
            && hello
                .view
                .control_block()
                .slot_claim
                .load(Ordering::Acquire)
                != token
        {
            if hello.withdraw() {
                hello.leave_robbed(retake);
            }
            return Err(ShmError::AlreadyConnected);
        }
        Ok(hello)
    }

    /// Тесты: токен claim-а заявки.
    #[cfg(test)]
    pub(crate) const fn claim(&self) -> u32 {
        self.claim
    }

    /// Fix round 1 (решение AE, I1 (a)): claim отняли (C8), а отзыв проигран
    /// -- сервер зафиксировал нашу заявку. Мы в сессии без своего claim-а:
    /// прежний `drop(finish())` уходил CAS пары `(SERVER_READY, T)`, который
    /// при чужом claim-е не проходит, -- сессия-призрак, сервер заклинен до
    /// выхода нашего процесса (каждый следующий клиент -- `AlreadyConnected`).
    ///
    /// Теперь: пока `client_state == SERVER_READY` -- вернуть свой claim (CAS
    /// `0 -> T`; занят -- ждать освобождения на `<имя>_free` по протоколу
    /// R21 не дольше `wait`, а подтверждённо умершего держателя -- перехват
    /// его claim-а; follow-up N1), записать поля владельца (C2) и уйти
    /// обычным L (`SharedClient::drop`: CAS пары, `DISCONNECT`, claim
    /// последним). `IDLE` -- сервер сессию уже закрыл, уходить не из чего.
    ///
    /// Почему не 32-битный CAS `client_state` при `generation == G`: после
    /// отъёма I2 не держится -- claim у другого клиента, и между сверкой и
    /// CAS сервер мог закрыть нашу сессию и зафиксировать его (тот же
    /// контрпример, что у решения AB; `generation`, прочитанный в `finish`,
    /// тогда уже его). С возвращённым claim-ом действует довод AB: новую
    /// заявку выставляет только держатель claim-а, а он -- мы, поэтому
    /// `SERVER_READY` при CAS пары -- наша сессия (или сессия-призрак другого
    /// ограбленного, чей владелец уходит так же). Claim занят до дедлайна
    /// (держатель жив и всё это время не дошёл до C6) -- запасной уход
    /// 32-битным CAS при `generation` нашей сессии (`leave_by_generation`):
    /// уход без записей оставил бы сессию-призрак навсегда (re-review N1,
    /// модель), а остаток запасного ухода -- ложный конец следующей сессии,
    /// не заклинивание (INTEROP §h п. 12).
    pub(crate) fn leave_robbed(self, wait: Duration) {
        let control = self.view.control_block();
        // `generation` нашей сессии: отзыв только что наблюдал её
        // зафиксированной (`SERVER_READY`). Нужен лишь запасному уходу
        // ниже.
        let session = control.generation.load(Ordering::Acquire);
        match self.retake(wait) {
            Retake::Gone => {}
            Retake::Taken => {
                // C2 -- как при захвате: время своего процесса, затем PID.
                control.announce_owner_start(crate::win::current_process_start_time());
                control
                    .slot_owner_pid
                    .store(std::process::id(), Ordering::Release);
                #[cfg(test)]
                if let Some(hook) = BEFORE_ROB_LEAVE.with(|hook| hook.borrow_mut().take()) {
                    hook(&self.view);
                }
                drop(self.finish());
            }
            Retake::TimedOut => self.leave_by_generation(session),
        }
    }

    /// Вернуть claim на C8-провале (follow-up N1, R21): ждать освобождения
    /// на `<имя>_free` (протокол ждущего) и процессе держателя не дольше
    /// `wait`; держатель подтверждённо умер -- перехватить его claim.
    fn retake(&self, wait: Duration) -> Retake {
        let control = self.view.control_block();
        let deadline = Instant::now().checked_add(wait);
        let mut attempt = || {
            if control.client_state.load(Ordering::Acquire) != HANDSHAKE_SERVER_READY {
                return Attempt::Done(Retake::Gone);
            }
            let Err(held) = control.slot_claim.compare_exchange(
                CLAIM_FREE,
                self.claim,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) else {
                return Attempt::Done(Retake::Taken);
            };
            // Держатель подтверждённо умер (до C6 -- заявки у него нет: она
            // упёрлась бы в нашу сессию) -- его claim не освободит никто,
            // кроме дедлайна сервера, а тот при `SERVER_READY` не идёт:
            // перехват CAS-ом его токена, поля умершего -- CAS-ом от них.
            let (owner, owner_start) = (
                control.slot_owner_pid.load(Ordering::Acquire),
                control.owner_start_time(),
            );
            if owner != 0
                && crate::win::is_process_gone(owner, owner_start)
                && control.slot_claim.load(Ordering::Acquire) == held
                && control.forget_owner(owner, owner_start)
                && control
                    .slot_claim
                    .compare_exchange(held, self.claim, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                return Attempt::Done(Retake::Taken);
            }
            Attempt::Busy(Busy {
                key: 0,
                holder: Some((owner, owner_start)),
                server: Some((
                    control.server_pid.load(Ordering::Acquire),
                    control.server_start_time(),
                )),
            })
        };
        let Some(free) = self.free.clone() else {
            // Пир без R20: проверка раз в 5 мс в пределах `wait`.
            loop {
                match attempt() {
                    Attempt::Done(outcome) => return outcome,
                    Attempt::Failed(_) => return Retake::Gone,
                    Attempt::Busy(_) => {}
                }
                if deadline.is_none_or(|at| Instant::now() >= at) {
                    return Retake::TimedOut;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let mut waiter = FreeWaiter::new(free);
        match waiter.wait_free(
            &mut attempt,
            &mut || {},
            &mut WaitCtl {
                wake: None,
                running: &|| true,
                on_wake: &mut || {},
                wakeup: &|| {},
                deadline,
                renudge: None,
            },
        ) {
            Ok(FreeWait::Claimed(outcome)) => outcome,
            Ok(FreeWait::Failed(_) | FreeWait::ServerGone | FreeWait::Stopped) => Retake::Gone,
            Ok(FreeWait::TimedOut) | Err(_) => Retake::TimedOut,
        }
    }

    /// Запасной уход (follow-up N1): claim вернуть не удалось до дедлайна
    /// (держатель жив и не дошёл до C6 всё это время). Сессия-призрак
    /// заклинила бы канал до выхода нашего процесса, поэтому -- уход
    /// 32-битным CAS `client_state: SERVER_READY -> IDLE` при `generation ==
    /// session` (наблюдённого отзывом), затем `DISCONNECT`.
    ///
    /// Остаток (довод решения AB, `handshake_model`): если МЕЖДУ сверкой
    /// `generation` и CAS сервер сам закрыл нашу сессию и успел
    /// зафиксировать сессию держателя, CAS закроет её -- ложный конец
    /// живой сессии, а не заклинивание: её клиент увидит уход сервера
    /// (`server_state`), его claim снимет уход L или дедлайн
    /// `RESERVE_TIMEOUT` (модель: свойство W держится).
    fn leave_by_generation(&self, session: u32) {
        let control = self.view.control_block();
        if control.generation.load(Ordering::Acquire) != session {
            return;
        }
        if control
            .client_state
            .compare_exchange(
                HANDSHAKE_SERVER_READY,
                HANDSHAKE_IDLE,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            let (header_a, header_b) = self.view.headers();
            header_a
                .handshake_state
                .store(HANDSHAKE_IDLE, Ordering::Release);
            header_b
                .handshake_state
                .store(HANDSHAKE_IDLE, Ordering::Release);
            let _ = self.events.disconnect.set();
        }
    }

    /// Сигнал `S2C_CONNECT` -- ответ на эту заявку (C10).
    fn accepted(&self) -> bool {
        accepted_on_signal(&self.view, self.generation_before)
    }

    /// Процесс сервера, наблюдаемый с заявки (`None` -- не наблюдается).
    pub(crate) const fn server_watch(&self) -> Option<&ProcessWatch> {
        self.server.as_ref()
    }

    /// Ждать `S2C_CONNECT` (C10) до `timeout`, следя за процессом сервера
    /// и -- если задано -- за `wake` вызывающего (Task 10, INTEROP §e.1).
    ///
    /// Набор: с прерыванием -- `[wake, S2C_CONNECT, процесс сервера]`, без
    /// него -- `[S2C_CONNECT, процесс сервера]` (процесс -- если открылся).
    ///
    /// Почему `wake` -- индекс 0 (П1): устаревший `S2C_CONNECT` цикл
    /// поглощает и ждёт снова, не возвращаясь к вызывающему. Стой `wake`
    /// после него, процесс, взводящий `S2C_CONNECT` чаще, чем мы ждём,
    /// держал бы `stop()` worker-а до `connect_timeout`.
    ///
    /// Почему после `wake` -- нулевое ожидание остальных, а не решение по
    /// состоянию (П2): `wake` взводит и каждый `send` приложения, и поток
    /// отправок не должен голодить подтверждение. Но засчитать заявку можно
    /// только по сигналу: `client_state == SERVER_READY` сервер публикует
    /// (S5) раньше сигнала (S7), а модель рукопожатия (`handshake_model`)
    /// доказана для «принят по `S2C_CONNECT`» -- поэтому, если не пора
    /// остановиться, одно ожидание `[S2C_CONNECT, процесс]` с нулевым
    /// таймаутом, и его исход разбирается как обычный.
    ///
    /// Процесс сервера сигнален -- `Abandoned(PeerDied)`: сервер умер, не
    /// ответив (отзыв W как при таймауте; проигранный отзыв -- подключены,
    /// смерть увидит приём). Устаревший сигнал (`accepted_on_signal ==
    /// false`) поглощается, ожидание продолжается.
    pub(crate) fn wait_ack(
        &self,
        timeout: Duration,
        mut interrupt: Option<&mut Interrupt<'_>>,
    ) -> AckWait {
        // Таймаут, непредставимый как `Instant` (`Duration::MAX`), -- ждать без
        // дедлайна, а не паниковать на переполнении.
        let deadline = crate::ring::deadline_of(Some(timeout));
        // Объекты пира: `[S2C_CONNECT, процесс сервера]`; хвост массива --
        // заполнитель, в ожидание уходит `[..peer_count]`.
        let ack = self.events.connect_ack.as_handle();
        let mut peer = [ack, ack];
        let peer_count = match self.server.as_ref() {
            Some(server) => {
                peer[PEER_PROCESS] = server.as_handle();
                2
            }
            None => 1,
        };
        let peer = &peer[..peer_count];
        loop {
            let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
            if left.is_some_and(|left| left.is_zero()) {
                return AckWait::Abandoned(ShmError::Timeout);
            }
            let fired = match interrupt.as_deref_mut() {
                Some(int) => {
                    let mut set = [int.wake, ack, ack];
                    set[1..=peer_count].copy_from_slice(peer);
                    match wait_any(&set[..=peer_count], left) {
                        Ok(Some(WAKE)) => {
                            if (int.should_stop)() {
                                return AckWait::Abandoned(ShmError::NotReady);
                            }
                            // `wake` от `send`: исход решает сигнал --
                            // одно нулевое ожидание объектов пира.
                            wait_any(peer, Some(Duration::ZERO))
                        }
                        Ok(Some(index)) => Ok(Some(index - 1)),
                        other => other,
                    }
                }
                None => wait_any(peer, left),
            };
            match fired {
                Ok(Some(PEER_ACK)) => {
                    if self.accepted() {
                        return AckWait::Accepted;
                    }
                    // Устаревший сигнал (fix round 1, M2): процесс сервера
                    // стоит в наборе после `S2C_CONNECT`, и поток устаревших
                    // сигналов скрывал бы его смерть до таймаута. Одна
                    // проверка на пустой сигнал -- O(1) (нулевое ожидание).
                    if self.server.as_ref().is_some_and(ProcessWatch::has_exited) {
                        return AckWait::Abandoned(ShmError::PeerDied);
                    }
                    #[cfg(test)]
                    crate::server::adversary_resignal(&self.view, &self.events.connect_ack);
                }
                Ok(Some(_)) => return AckWait::Abandoned(ShmError::PeerDied),
                // Нулевое ожидание после `wake` ничего не застало -- ждём
                // дальше; иначе истёк таймаут.
                Ok(None) => {
                    if deadline.is_some_and(|at| Instant::now() >= at) {
                        return AckWait::Abandoned(ShmError::Timeout);
                    }
                }
                Err(err) => return AckWait::Abandoned(err),
            }
        }
    }

    /// Отзыв W: `true` -- поздно, рукопожатие состоялось (продолжать как
    /// подключённые); `false` -- заявка отозвана и откатана, claim (свой)
    /// освобождён последним.
    pub(crate) fn withdraw(&self) -> bool {
        if withdraw_hello(&self.view, self.generation_before, self.announced) {
            return true;
        }
        if self.owns_claim {
            release_claim(&self.view, self.claim, self.free.as_deref());
        }
        false
    }

    /// Рукопожатие состоялось (шаг A): подключённый клиент.
    pub(crate) fn finish(self) -> SharedClient {
        self.finish_with(false)
    }

    /// Заявка брошена с ошибкой `err`, но отзыв проигран -- сервер успел
    /// зафиксировать рукопожатие: подключены. Брошена как `PeerDied` --
    /// значит, этот путь уже счёл сервер умершим (синхронный --
    /// `server_exited`, async -- более широкое `server_gone`, в том числе PID,
    /// занятый процессом с другим временем создания): подключение получает
    /// наблюдение «уже завершился» (решение X, M1 ревью T11b).
    pub(crate) fn finish_lost(self, err: &ShmError) -> SharedClient {
        self.finish_with(*err == ShmError::PeerDied)
    }

    /// Шаг A. `server_dead` -- сервер признан умершим до открытия его
    /// процесса.
    fn finish_with(self, server_dead: bool) -> SharedClient {
        let Self {
            view,
            events,
            generation_before: _,
            claim,
            owns_claim,
            server: peer,
            free,
            announced: _,
            server_gone: _,
            server_exited,
            mapping,
        } = self;
        let control = view.control_block();
        // Решение X: сервер признан умершим ещё до открытия его процесса, а
        // отзыв заявки проигран (он успел зафиксировать рукопожатие) --
        // подключены, но наблюдать некого. Вариант «connect отвечает
        // `PeerDied`» отброшен: сессия зафиксирована, в кольце может лежать
        // прощание сервера, и после фиксации смерть пира везде сообщается
        // ПОСЛЕ дочитывания (`poll_*`, `recv_async`, worker-ы Auto/Multi).
        // Поэтому -- наблюдение «уже завершился» (всегда сигнально): первое
        // же ожидание видит смерть, данные дочитываются, затем `PeerDied`.
        // Не удалось создать событие (нехватка ресурсов) -- без наблюдения,
        // как до исправления. Решение принимает путь, бросивший заявку, а не
        // `finish` по признакам: живой сервер с испорченным временем в
        // сегменте (`server_gone` без `server_exited`), чью заявку синхронный
        // путь принял по `S2C_CONNECT`, наблюдения не получает и работает
        // (`peer_tests::corrupted_server_start_hi_is_not_a_live_server`).
        //
        // N1 повторного ревью T11b: смерть НАБЛЮДЁННАЯ (`server_exited`:
        // процесса с PID нет либо он сигнален и время совпало) и
        // ВЫВЕДЕННАЯ (`connect_async` по широкому `server_gone`: процесс с
        // этим PID жив, но время создания другое -- PID переиспользован ИЛИ
        // время в сегменте испорчено у живого сервера) различаются при
        // уходе. Выведенная оставляет `Drop` полный уход L (CAS пары +
        // `DISCONNECT`), даже после `PeerDied`: иначе живой сервер,
        // успевший зафиксировать (а `CONNECT_REQ` async уже подал), остался
        // бы «в сессии» навсегда, и все следующие клиенты получали бы
        // `AlreadyConnected`. Если сервер и правда мёртв, лишний сигнал
        // безвреден (S4 следующей сессии его сбросит).
        let (peer, inferred_death) = match peer {
            Some(peer) => (Some(peer), false),
            None if server_dead => (
                ProcessWatch::exited(control.server_pid.load(Ordering::Acquire)).ok(),
                !server_exited,
            ),
            None => (None, false),
        };
        let (header_a, header_b) = view.headers();
        // Процесс сервера -- открытый при заявке (`Hello::post`; `None` --
        // сервер старой версии без PID: наблюдения нет, как в 0.7).

        let generation = control.generation.load(Ordering::Acquire);
        header_a.connection_gen.store(generation, Ordering::Release);
        header_b.connection_gen.store(generation, Ordering::Release);
        header_a
            .handshake_state
            .store(HANDSHAKE_SERVER_READY, Ordering::Release);
        header_b
            .handshake_state
            .store(HANDSHAKE_SERVER_READY, Ordering::Release);

        let ring_tx = view.ring_b();
        let ring_rx = view.ring_a();

        SharedClient {
            view,
            events,
            ring_tx,
            ring_rx,
            connected: true,
            peer,
            generation,
            claim,
            owns_claim,
            leave_on_drop: inferred_death,
            free,
            _mapping: mapping,
            _not_sync: PhantomData,
        }
    }
}

/// Future `connect_async`/`connect_async_in`: заявка при первом опросе,
/// ожидание `S2C_CONNECT` в реакторе, откат заявки в `Drop`.
struct ConnectFuture {
    /// Частное пространство имён канала (`connect_async_in`); `None` --
    /// общий `\BaseNamedObjects` (`connect_async`). Держится до готовности.
    namespace: Option<Arc<PrivateNamespace>>,
    name: String,
    /// Выставленная заявка (до подтверждения или отказа).
    hello: Option<Hello>,
    /// Первый опрос уже был (заявка выставлялась).
    started: bool,
    /// Процесс сервера наблюдает сама заявка (`Hello::server_watch`): его
    /// смерть во время рукопожатия -- `Err(PeerDied)`; без наблюдения, как
    /// у синхронного `connect`, границу задаёт таймаут исполнителя.
    waits: WaitSet,
}

impl ConnectFuture {
    fn new(namespace: Option<Arc<PrivateNamespace>>, name: &str) -> Self {
        Self {
            namespace,
            name: name.to_owned(),
            hello: None,
            started: false,
            waits: WaitSet::default(),
        }
    }

    /// Снять все регистрации набора и отпустить наблюдение за сервером --
    /// на КАЖДОМ готовом исходе ДО `Hello::finish`/`Hello::withdraw`
    /// (Task 4 надёжности). `withdraw` последним шагом отпускает claim:
    /// живая к этому моменту регистрация на автосбросном `S2C_CONNECT`
    /// потребила бы подтверждение СЛЕДУЮЩЕГО клиента (он сбрасывает событие
    /// на C4 и ждёт его сам), и тот ждал бы до таймаута -- сколь угодно
    /// долго, если исполнитель держит future после `Ready`.
    ///
    /// Потреблённый `S2C_CONNECT` не возвращается: пока claim наш (снятие
    /// идёт до его отпуска), любой `S2C_CONNECT` адресован нам -- claim
    /// исключителен, а следующий заявитель сбрасывает событие до своей
    /// заявки (C4); `DISCONNECT` набор не регистрирует, процесс сервера --
    /// уведомление (остаётся взведённым). Handle процесса сервера живёт в
    /// заявке и переходит подключённому клиенту (`Hello::finish`) -- его
    /// регистрация к этому моменту уже снята.
    fn release(&mut self) {
        self.waits.clear(None, None);
    }

    fn step(&mut self, waker: &Waker) -> Step {
        if !self.started {
            self.started = true;
            let posted = Hello::post(
                self.namespace.as_deref(),
                &self.name,
                true,
                ClaimMode::Take,
                FreeLink::Open,
            );
            let hello = match posted {
                Ok(hello) => hello,
                Err(err) => return Step::Failed(err),
            };
            if let Err(err) = hello.events.connect_req.set() {
                // Как синхронный путь: отзыв, проигравший CAS сервера, --
                // всё же подключение.
                return Step::Abandon(hello, err);
            }
            // Task 18 (Task 16 N1): `open_peer` не наблюдает уже мёртвый
            // процесс («неизвестно» -- верно для PID из рукопожатия). Здесь
            // PID -- сервер, владеющий открытым сегментом: подтверждённо
            // ушедший (`is_process_gone`: процесса нет, он завершён или PID
            // занят процессом с другим временем создания) -- `PeerDied` с
            // откатом, как при таймауте. Иначе future ждал бы вечно.
            // Двусмысленность (нет прав, PID 0) -- без наблюдения, как раньше.
            // Признак вычисляет `Hello::post` при открытии процесса.
            if hello.server_gone {
                return Step::Abandon(hello, ShmError::PeerDied);
            }
            self.hello = Some(hello);
        }
        let Some(hello) = self.hello.as_ref() else {
            // Повторный опрос завершённого future.
            return Step::Failed(ShmError::NotReady);
        };
        let fired = self.waits.harvest(waker);
        // Как `Hello::wait_ack` (C10): подтверждение -- `S2C_CONNECT` при
        // фиксации своей заявки (`accepted_on_signal`); иначе сигнал
        // устаревший, поглощён.
        if fired[OP] && hello.accepted() {
            return self.take_hello(Step::Accepted);
        }
        if fired[PEER] {
            // Сервер умер, не ответив: откат, как у синхронного таймаута
            // (CAS `CLIENT_HELLO -> IDLE`). Отзыв проиграл -- сервер успел
            // зафиксировать рукопожатие: подключены, смерть увидит приём.
            return self.take_hello(|hello| Step::Abandon(hello, ShmError::PeerDied));
        }
        let registered = self
            .waits
            .ensure_handle(OP, hello.events.connect_ack.as_handle(), waker)
            .and_then(|()| match hello.server_watch() {
                Some(server) => self.waits.ensure_handle(PEER, server.as_handle(), waker),
                None => Ok(()),
            });
        match registered {
            Ok(()) => Step::Pending,
            // Ждать нельзя -- заявку отзываем, как при ошибке ожидания.
            Err(err) => self.take_hello(|hello| Step::Abandon(hello, err)),
        }
    }

    /// Готовый исход с заявкой (`hello` есть -- проверено в `step`).
    fn take_hello(&mut self, outcome: impl FnOnce(Hello) -> Step) -> Step {
        match self.hello.take() {
            Some(hello) => outcome(hello),
            None => Step::Failed(ShmError::NotReady),
        }
    }
}

/// Исход опроса `ConnectFuture::step`: регистрации на готовых исходах ещё
/// стоят -- их снимает `poll` (`release`) до `finish`/`withdraw`.
enum Step {
    /// Ждём `S2C_CONNECT` (регистрации живы).
    Pending,
    /// Сервер зафиксировал нашу заявку.
    Accepted(Hello),
    /// Заявку отзывать (W) с этой ошибкой; проигранный отзыв -- подключены.
    Abandon(Hello, ShmError),
    /// Заявки нет (не выставилась или future уже завершён).
    Failed(ShmError),
}

impl Future for ConnectFuture {
    type Output = Result<SharedClient>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        // Порядок «снять регистрации -> только потом finish/withdraw
        // (отпуск claim-а)» -- см. `ConnectFuture::release`.
        match this.step(cx.waker()) {
            Step::Pending => Poll::Pending,
            Step::Accepted(hello) => {
                this.release();
                Poll::Ready(Ok(hello.finish()))
            }
            Step::Abandon(hello, err) => {
                this.release();
                Poll::Ready(if hello.withdraw() {
                    Ok(hello.finish_lost(&err))
                } else {
                    Err(err)
                })
            }
            Step::Failed(err) => {
                this.release();
                Poll::Ready(Err(err))
            }
        }
    }
}

impl Drop for ConnectFuture {
    fn drop(&mut self) {
        // Сначала регистрации, потом отзыв (отпуск claim-а) -- как на
        // готовых исходах (`release`). `S2C_CONNECT` не возвращаем: заявка
        // отзывается, а следующий клиент всё равно сбрасывает событие до
        // своей заявки.
        self.release();
        // Заявка без ответа -- откат, как у синхронного таймаута. Отзыв
        // опоздал (CAS сервера выиграл) -- мы подключены, но владельца нет:
        // уходим штатно через `Drop` клиента (CAS `SERVER_READY -> IDLE`,
        // затем `DISCONNECT`), а не оставляем серверу сессию без клиента.
        if let Some(hello) = self.hello.take()
            && hello.withdraw()
        {
            drop(hello.finish());
        }
    }
}

impl Endpoint for SharedClient {
    fn ensure_connected(&self) -> Result<()> {
        Self::ensure_connected(self)
    }

    fn channel_events(&self) -> &SharedEvents {
        &self.events
    }

    fn rx_data(events: &SharedEvents) -> &EventHandle {
        &events.s2c.data
    }

    fn tx_space(events: &SharedEvents) -> &EventHandle {
        &events.c2s.space
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
        self.server_left()
    }

    fn receive(&self, buf: &mut Vec<u8>) -> Result<usize> {
        self.receive_from_server(buf)
    }

    fn try_send(&self, data: &[u8]) -> Result<()> {
        self.try_send_to_server(data).map(drop)
    }

    fn mark_gone(&mut self) {
        self.mark_disconnected();
    }
}

impl SharedClient {
    /// Тесты: токен claim-а этого клиента (`CLAIM_FREE` -- без claim).
    #[cfg(test)]
    pub(crate) const fn claim(&self) -> u32 {
        self.claim
    }

    /// Тесты: проекция сегмента канала (метка «противника» T10/T11c).
    #[cfg(test)]
    pub(crate) const fn view(&self) -> &SharedView {
        &self.view
    }

    /// Сессия всё ещё наша: `generation` не сменился (ревизия 2). Сменился
    /// -- сервер уже провёл рукопожатие с другим клиентом этого канала/слота
    /// (а наш `DISCONNECT` мог быть сброшен его `complete_handshake`).
    pub(crate) fn is_session_current(&self) -> bool {
        self.view.control_block().generation.load(Ordering::Acquire) == self.generation
    }

    /// Сервер закрыл нашу сессию: `server_state` уже не `SERVER_READY` или
    /// `generation` сменился. Отличает свежий `DISCONNECT` от устаревшего
    /// -- одно правило для всех потребителей стороны клиента
    /// (`poll_server`, `wait_for_space`, async, ответ лобби Dispatch,
    /// worker-ы Auto и Multi; T11c, решение AD).
    pub(crate) fn server_left(&self) -> bool {
        self.view
            .control_block()
            .server_state
            .load(Ordering::Acquire)
            != HANDSHAKE_SERVER_READY
            || !self.is_session_current()
    }

    #[must_use]
    pub const fn is_connected(&self) -> bool {
        self.connected
    }

    pub(crate) const fn events(&self) -> &SharedEvents {
        &self.events
    }

    pub(crate) fn mark_disconnected(&mut self) {
        self.connected = false;
        self.peer = None;
    }

    /// Worker-ы (Auto, Multi): сессия кончилась по `reason`, кольцо уже
    /// дочитано. Сервер ушёл сам (`Graceful`, `PeerDied`) или сессия уже
    /// чужая -- `mark_disconnected`: `Drop` только снимет claim, без
    /// ответного `DISCONNECT` (поздний сигнал достался бы следующему
    /// клиенту). Разрыв по нашей ошибке (`Error`: порча кольца, отказ
    /// ожидания) -- сервер ещё в сессии: отметку НЕ ставим, и `Drop`
    /// выполняет полный уход L (`client_state -> IDLE`, потом
    /// `DISCONNECT`, claim -- последним; INTEROP §c.5, §d.2 шаг 7). Иначе
    /// сервер остался бы «подключённым», а следующий захват упирался бы в
    /// C6 (`AlreadyConnected`) навсегда (final review I1).
    pub(crate) fn end_session(&mut self, reason: DisconnectReason) {
        if reason != DisconnectReason::Error {
            self.mark_disconnected();
        }
    }

    /// Подключён и сессия наша (final review M1): после смены `generation`
    /// (сервер отнял claim «не заметившего» клиента и принял другого)
    /// запись ушла бы в кольцо чужой сессии -- второй писатель SPSC.
    fn ensure_connected(&self) -> Result<()> {
        if self.connected && self.is_session_current() {
            Ok(())
        } else {
            Err(ShmError::NotConnected)
        }
    }

    /// Можно писать серверу (Task 6 надёжности): `ensure_connected` и
    /// сервер ещё в сессии (`server_state == SERVER_READY`, Acquire). Сервер
    /// ушёл (`server_state = IDLE`, `DISCONNECT`), а клиент событий ещё не
    /// разобрал -- кольцо, которое S3 следующей сессии сбросит, кадров не
    /// получает: запись, начатая до сброса и законченная во время него,
    /// оставила бы кадр в кольце новой сессии. S3 сбрасывает кольца при
    /// `server_state == IDLE`, а `SERVER_READY` ставит после `generation +
    /// 1`, так что пара проверок закрывает окно целиком; остаток -- запись,
    /// застрявшая между проверкой и публикацией дольше `RESERVE_TIMEOUT` и
    /// рукопожатия следующего клиента: вытесненный поток или (у `send_with`)
    /// столь же долгое замыкание заполнения (INTEROP §h п. 11; окно чтения
    /// устроено иначе -- там же). Цена -- одна
    /// Acquire-загрузка на запись, без системных вызовов.
    fn ensure_writable(&self) -> Result<()> {
        self.ensure_connected()?;
        if self
            .view
            .control_block()
            .server_state
            .load(Ordering::Acquire)
            == HANDSHAKE_SERVER_READY
        {
            Ok(())
        } else {
            Err(ShmError::NotConnected)
        }
    }

    /// Ошибка кольца после смены сессии -- не порча, а чужая сессия:
    /// `Corrupted` от позиций, сброшенных S3 следующей сессии, при ушедшем
    /// сервере -- `NotConnected` (только путь ошибки, на успешных операциях
    /// проверки нет).
    fn session_error(&self, err: ShmError) -> ShmError {
        if err == ShmError::Corrupted && self.server_left() {
            ShmError::NotConnected
        } else {
            err
        }
    }

    pub fn send_to_server(&self, payload: &[u8]) -> Result<WriteOutcome> {
        self.ensure_writable()?;
        let result = self
            .ring_tx
            .write_message(payload)
            .map_err(|err| self.session_error(err))?;
        if result.was_empty {
            let _ = self.events.c2s.data.set();
        }
        Ok(result)
    }

    /// Отправка серверу **без перезаписи** непрочитанных данных.
    ///
    /// Сообщение либо целиком ложится в свободное место кольца
    /// Client -> Server, либо `Err(ShmError::QueueFull)` и кольцо не
    /// меняется. Семантика -- `SharedServer::try_send_to_client`.
    pub fn try_send_to_server(&self, payload: &[u8]) -> Result<WriteOutcome> {
        self.ensure_writable()?;
        let result = self
            .ring_tx
            .try_write_message(payload)
            .map_err(|err| self.session_error(err))?;
        if result.was_empty {
            let _ = self.events.c2s.data.set();
        }
        Ok(result)
    }

    /// Отправка серверу без перезаписи с заполнением на месте (zero-copy,
    /// 0.10; семантика -- `SharedServer::send_with`): `f` заполняет `len`
    /// байт payload прямо в кольце Client -> Server.
    ///
    /// `&mut self`: ни вложенной отправки из `f`, ни другого потока, пока
    /// работает `f` (иначе вторая запись получила бы ту же область кольца):
    ///
    /// ```compile_fail,E0502
    /// fn nested(client: &mut xshm::SharedClient) {
    ///     let _ = client.send_with(16, |buf| {
    ///         let _ = client.try_send_to_server(b"nested-message!!");
    ///         buf.fill(b'O');
    ///     });
    /// }
    /// ```
    pub fn send_with(&mut self, len: usize, f: impl FnOnce(&mut [u8])) -> Result<WriteOutcome> {
        self.ensure_writable()?;
        // SAFETY: контракт `write_with` -- единственный писатель В ПРЕДЕЛАХ
        // этой конечной точки: `&mut self` исключает вложенную запись из `f`
        // и запись из другого потока через этот же клиент (тип ещё и
        // `!Sync`). Второй, устаревший `SharedClient` того же сегмента в
        // этом процессе -- уже не эта точка, а пир, нарушающий протокол
        // (одна сторона на направление); это случай враждебного пира,
        // принятый для неопубликованной области (Ruling 17): страдает только
        // сообщение, которое получит сервер.
        let result =
            unsafe { self.ring_tx.write_with(len, f) }.map_err(|err| self.session_error(err))?;
        if result.was_empty {
            let _ = self.events.c2s.data.set();
        }
        Ok(result)
    }

    /// Свободное место в кольце Client -> Server (нижняя граница из потока
    /// писателя, см. `SharedServer::free_space`). Вне подключения --
    /// `FreeSpace::ZERO`.
    #[must_use]
    pub fn free_space(&self) -> FreeSpace {
        if self.connected {
            self.ring_tx.free_space()
        } else {
            FreeSpace::ZERO
        }
    }

    /// Ждать места в кольце Client -> Server под payload длины `payload_len`
    /// (семантика -- `SharedServer::wait_for_space`).
    ///
    /// Набор ожидания -- `[C2S_SPACE, S2C_DISCONNECT, процесс сервера]`,
    /// одно ожидание на весь `timeout`; `DISCONNECT` при `server_state !=
    /// SERVER_READY` -- `Err(NotConnected)`. Сторону отключённой не
    /// помечает (`&self`; событие остаётся взведённым) -- сессию заканчивает
    /// следующий `poll_server`.
    pub fn wait_for_space(&self, payload_len: usize, timeout: Option<Duration>) -> Result<bool> {
        self.wait_for_space_spin(payload_len, timeout, SpinPolicy::NONE)
    }

    /// `wait_for_space` со спином перед ожиданием в ядре (0.10; семантика
    /// -- `SharedServer::wait_for_space_spin`).
    pub fn wait_for_space_spin(
        &self,
        payload_len: usize,
        timeout: Option<Duration>,
        spin: SpinPolicy,
    ) -> Result<bool> {
        self.ensure_writable()?;
        // T11c: то же правило ухода, что у остальных потребителей
        // (`server_left`, с `generation`; до T11c здесь сверялось только
        // `server_state`).
        let peer_left = || self.server_left();
        let disconnect = DisconnectWatch {
            event: &self.events.disconnect,
            peer_left: &peer_left,
        };
        self.ring_tx.wait_for_space(
            payload_len,
            &self.events.c2s.space,
            self.peer.as_ref(),
            Some(&disconnect),
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

    pub fn receive_from_server(&self, buffer: &mut Vec<u8>) -> Result<usize> {
        self.ensure_connected()?;
        #[cfg(test)]
        if let Some(hook) = BEFORE_READ.with(|hook| hook.borrow_mut().take()) {
            hook();
        }
        // Сверка сессии ещё и перед фиксацией чтения (Task 6 надёжности):
        // сессия сменилась после проверки на входе -- кадр новой сессии не
        // забирается и не отдаётся.
        let len = self
            .ring_rx
            .read_message_checked(buffer, || self.is_session_current())
            .map_err(|err| self.session_error(err))?;
        self.signal_space_after_read();
        Ok(len)
    }

    /// Zero-copy приём от сервера (0.10; семантика и ошибки --
    /// `SharedServer::receive_with`): `f` получает payload срезом прямо из
    /// кольца Server -> Client.
    ///
    /// # Safety
    /// Пока выполняется `f`, никто не изменяет байты этого сообщения в
    /// разделяемой памяти. Выполнено, когда сервер -- доверенный честный пир
    /// xshm, пишущий это направление без перезаписи (`try_send_to_client`,
    /// `send_with`, `wait_for_space`; не `send_to_client`). Для
    /// недоверенного сервера -- `receive_from_server`.
    ///
    /// Второго чтения этого направления во время `f` не бывает по типу
    /// (`&mut self`); вложенный приём не компилируется:
    ///
    /// ```compile_fail,E0502
    /// fn nested(client: &mut xshm::SharedClient) {
    ///     let mut buf = Vec::new();
    ///     // SAFETY: (пример не компилируется)
    ///     let _ = unsafe {
    ///         client.receive_with(|_data| client.receive_from_server(&mut buf))
    ///     };
    /// }
    /// ```
    pub unsafe fn receive_with<R>(&mut self, f: impl FnOnce(&[u8]) -> R) -> Result<R> {
        // SAFETY: неизменность кадра -- контракт этой функции (`# Safety`);
        // единственность читателя на время `f` даёт `&mut self`.
        unsafe { self.receive_with_shared(f) }
    }

    /// `receive_with` по `&self` -- для worker-а Auto, который держит
    /// клиента один (клиент не выходит из его потока).
    ///
    /// # Safety
    /// Контракт `receive_with` и, сверх него, контракт единственного
    /// читателя `RingBuffer::read_with`: никакого другого чтения кольца
    /// Server -> Client, пока выполняется `f`.
    pub(crate) unsafe fn receive_with_shared<R>(&self, f: impl FnOnce(&[u8]) -> R) -> Result<R> {
        self.ensure_connected()?;
        // SAFETY: контракт `read_with` -- контракт этой функции (`# Safety`).
        // Сверка сессии -- до `f` (кадр новой сессии не отдаётся).
        let result = unsafe {
            self.ring_rx
                .read_with_checked(|| self.is_session_current(), f)
        }
        .map_err(|err| self.session_error(err));
        if result.is_ok() {
            self.signal_space_after_read();
        }
        result
    }

    /// SPACE -- когда кольцо опустело ИЛИ писатель-сервер ждёт места и его
    /// теперь хватает (см. `SharedServer::receive_from_client`).
    fn signal_space_after_read(&self) {
        if self.ring_rx.take_space_waiter() || self.ring_rx.is_empty() {
            let _ = self.events.s2c.space.set();
        }
    }

    /// Ждать данных от сервера (семантика -- `SharedServer::poll_client`).
    ///
    /// Набор ожидания -- `[S2C_DATA, S2C_DISCONNECT, процесс сервера]`.
    /// `Err(NotConnected)` -- сервер закрыл сессию (`server_state !=
    /// SERVER_READY` или сменился `generation`) и всё, что он записал, уже
    /// прочитано; `Err(PeerDied)` -- процесс сервера умер, кольцо дочитано.
    /// На завершающем исходе клиент помечается отключённым (отсюда `&mut
    /// self`): повторный вызов -- `Err(NotConnected)` сразу, `Drop` только
    /// снимет claim, без ответного `DISCONNECT` (сервер уже ушёл; поздний
    /// сигнал достался бы следующему клиенту). `wait_for_space` об уходе
    /// только сообщает и сторону не помечает.
    pub fn poll_server(&mut self, timeout: Option<Duration>) -> Result<bool> {
        self.poll_server_spin(timeout, SpinPolicy::NONE)
    }

    /// `poll_server` со спином перед ожиданием в ядре (0.10; семантика --
    /// `SharedServer::poll_client_spin`).
    pub fn poll_server_spin(
        &mut self,
        timeout: Option<Duration>,
        spin: SpinPolicy,
    ) -> Result<bool> {
        self.ensure_connected()?;
        let peer_left = || self.server_left();
        let disconnect = DisconnectWatch {
            event: &self.events.disconnect,
            peer_left: &peer_left,
        };
        let result = self.ring_rx.wait_for_data(
            &self.events.s2c.data,
            self.peer.as_ref(),
            Some(&disconnect),
            timeout,
            spin,
        );
        if matches!(result, Err(ShmError::NotConnected | ShmError::PeerDied)) {
            // Завершающий исход: кольцо дочитано, сервер ушёл или умер.
            self.mark_disconnected();
        }
        result
    }

    /// Worker-ы (спин перед сном, 0.10): в кольце от сервера есть
    /// непрочитанное -- по позициям, без барьера.
    pub(crate) fn has_unread_from_server(&self) -> bool {
        self.ring_rx.has_unread()
    }

    /// PID процесса сервера, если сервер 0.8+ опубликовал его.
    #[must_use]
    pub fn peer_pid(&self) -> Option<u32> {
        self.peer.as_ref().map(ProcessWatch::pid)
    }

    /// Жив ли процесс сервера (семантика -- `SharedServer::is_peer_alive`).
    #[must_use]
    pub fn is_peer_alive(&self) -> Option<bool> {
        if !self.connected {
            return None;
        }
        self.peer.as_ref().map(|peer| !peer.has_exited())
    }

    pub(crate) fn peer_wait_handle(&self) -> Option<BorrowedHandle<'_>> {
        self.peer.as_ref().map(AsHandle::as_handle)
    }
}

impl SharedClient {
    /// CAS ухода L: пара `(client_state, slot_claim)` -- `(SERVER_READY, T)`
    /// -> `(IDLE, T)` одним 64-битным CAS (решение AB,
    /// `ControlBlock::leave_session`).
    fn leave_cas(&self) -> bool {
        #[cfg(test)]
        if let Some(hook) = BEFORE_LEAVE_CAS.with(|hook| hook.borrow_mut().take()) {
            hook(&self.view);
        }
        self.view.control_block().leave_session(self.claim)
    }
}

impl Drop for SharedClient {
    /// Уход L (ревизия 3, INTEROP R15): записи состояния -- только пока
    /// сессия наша, освобождение claim -- последним.
    ///
    /// R14: сначала своё состояние -> `IDLE`, потом `DISCONNECT`.
    /// `client_pid` не трогаем: наш PID сервер уже забрал (S6), а следующий
    /// клиент мог записать туда свой. `generation` сменился -- сессия уже не
    /// наша: ни состояния, ни заголовков, ни `DISCONNECT`.
    ///
    /// Решение AB: переход состояния -- ОДИН 64-битный CAS пары
    /// `(client_state, slot_claim)`: `(SERVER_READY, T) -> (IDLE, T)`
    /// (`ControlBlock::leave_session`). Сверка `generation` сама по себе не
    /// защищала: живого «не заметившего» клиента сессии, закрытой сервером,
    /// сервер лишает claim-а по `RESERVE_TIMEOUT` (`observe_idle_claim`), и
    /// следующий клиент мог пройти рукопожатие между нашей сверкой и
    /// 32-битным CAS `client_state` -- тот снимал бы ЧУЖУЮ сессию. CAS пары
    /// проходит, только пока claim -- наш токен `T`; у следующей сессии claim
    /// другой (токены уникальны), поэтому её он не трогает ни при каком
    /// вытеснении потока. `NotConnected`-уход сервера (T11b: `disconnect`/
    /// `Drop` сервера уже перевели `client_state` в `IDLE`) CAS тоже не
    /// проходит -- `DISCONNECT` не сигналим: сессии, которую он закрыл бы,
    /// уже нет, а сигнал остался бы для следующей (INTEROP §d.2 шаг 7).
    /// Отключён (`mark_disconnected`: сервер ушёл сам) -- только claim;
    /// исключение -- смерть сервера, выведенная, а не наблюдённая
    /// (`leave_on_drop`, N1 повторного ревью T11b): полный уход L.
    fn drop(&mut self) {
        if (self.connected || self.leave_on_drop) && self.is_session_current() && self.leave_cas() {
            let (header_a, header_b) = self.view.headers();
            header_a
                .handshake_state
                .store(HANDSHAKE_IDLE, Ordering::Release);
            header_b
                .handshake_state
                .store(HANDSHAKE_IDLE, Ordering::Release);
            let _ = self.events.disconnect.set();
        }
        self.connected = false;
        // Task 12 (R20): claim -- последним, с уведомлением ждущих.
        if self.owns_claim {
            release_claim(&self.view, self.claim, self.free.as_deref());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::SharedServer;

    /// Отзыв заявки против фиксации сервера: CAS решает однозначно.
    #[test]
    fn withdraw_hello_loses_only_to_a_committed_handshake() {
        let name = format!("XSHM_WITHDRAW_{}", std::process::id());
        let server = SharedServer::start(&name).unwrap();
        let view = server.view();
        let control = view.control_block();

        // Никто не ответил -- отзыв удался.
        let before = control.generation.load(Ordering::Acquire);
        control
            .client_state
            .store(HANDSHAKE_CLIENT_HELLO, Ordering::Release);
        assert!(!withdraw_hello(view, before, None));
        assert_eq!(control.client_state.load(Ordering::Acquire), HANDSHAKE_IDLE);

        // Сервер успел: generation опубликован, CAS HELLO -> READY выигран.
        control
            .client_state
            .store(HANDSHAKE_CLIENT_HELLO, Ordering::Release);
        control.generation.fetch_add(1, Ordering::AcqRel);
        control
            .client_state
            .compare_exchange(
                HANDSHAKE_CLIENT_HELLO,
                HANDSHAKE_SERVER_READY,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .unwrap();
        assert!(withdraw_hello(view, before, None), "рукопожатие состоялось");
        assert_eq!(
            control.client_state.load(Ordering::Acquire),
            HANDSHAKE_SERVER_READY
        );

        // Уходящий сервер снял заявку без рукопожатия (generation тот же).
        let before = control.generation.load(Ordering::Acquire);
        control
            .client_state
            .store(HANDSHAKE_IDLE, Ordering::Release);
        assert!(!withdraw_hello(view, before, None));
    }

    /// Task 18 (Task 16 N1): сервер умер раньше, чем `connect_async` открыл
    /// его процесс. Окно воспроизводится детерминированно: в сегменте --
    /// PID и время создания уже завершившегося процесса (сегмент и события
    /// живы, как если бы их держал кто-то ещё). Future обязан ответить
    /// `PeerDied` с откатом заявки на первом же опросе, а не ждать вечно
    /// `S2C_CONNECT`, который никто не пошлёт.
    #[test]
    fn connect_async_sees_server_dead_before_open() {
        let name = format!("XSHM_ASYNC_DEAD_SERVER_{}", std::process::id());
        let server = SharedServer::start(&name).unwrap();
        let control = server.view().control_block();

        // Завершившийся процесс: PID и время создания, затем ни одного handle.
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "exit"])
            .spawn()
            .unwrap();
        let pid = child.id();
        let watch = ProcessWatch::open(pid).unwrap();
        let start = watch.start_time().unwrap();
        child.wait().unwrap();
        drop(watch);
        drop(child);
        let (lo, hi) = crate::layout::split_time(start);
        control.server_start_lo.store(lo, Ordering::Release);
        control.server_start_hi.store(hi, Ordering::Release);
        control.server_pid.store(pid, Ordering::Release);

        let mut future = std::pin::pin!(SharedClient::connect_async(&name));
        let mut cx = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(Err(ShmError::PeerDied)) => {}
            Poll::Ready(other) => panic!("ожидался PeerDied: {:?}", other.map(drop)),
            Poll::Pending => panic!("смерть сервера до open_peer не замечена"),
        }
        // Откат, как у таймаута: заявки и PID клиента в сегменте нет.
        assert_eq!(control.client_state.load(Ordering::Acquire), HANDSHAKE_IDLE);
        assert_eq!(control.client_pid.load(Ordering::Acquire), 0);
    }

    // ─── Ревизия 3 (Task 22): исключительная заявка ─────────────────────────

    fn unique(tag: &str) -> String {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("XSHM_CLM_{tag}_{}_{}", std::process::id(), ts % 1_000_000)
    }

    /// Снимок полей заявки: `client_pid`, `client_start`, `client_state`,
    /// `slot_claim`, `slot_owner_pid`.
    fn claim_fields(control: &crate::layout::ControlBlock) -> [u32; 6] {
        [
            control.client_pid.load(Ordering::Acquire),
            control.client_start_lo.load(Ordering::Acquire),
            control.client_start_hi.load(Ordering::Acquire),
            control.client_state.load(Ordering::Acquire),
            control.slot_claim.load(Ordering::Acquire),
            control.slot_owner_pid.load(Ordering::Acquire),
        ]
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

    /// §7.1 п. 1: проигравший захват ничего не пишет в сегмент, получает
    /// `AlreadyConnected` сразу и толкает сервер (`CONNECT_REQ`).
    #[test]
    fn loser_gets_already_connected_and_writes_nothing() {
        let name = unique("LOSER");
        let server = SharedServer::start(&name).unwrap();
        let control = server.view().control_block();
        let req = &server.events().connect_req;
        let holder = Hello::post(None, &name, true, ClaimMode::Take, FreeLink::Open).unwrap();
        let before = claim_fields(control);
        assert!(
            !req.wait(Some(Duration::ZERO)).unwrap(),
            "заявка без сигнала"
        );

        let started = Instant::now();
        let err = SharedClient::connect(&name, Duration::from_millis(500)).unwrap_err();
        let elapsed = started.elapsed();
        assert_eq!(err, ShmError::AlreadyConnected);
        assert!(elapsed < Duration::from_millis(50), "{elapsed:?}");
        assert_eq!(claim_fields(control), before, "проигравший писал в сегмент");
        assert_ne!(control.slot_claim.load(Ordering::Acquire), 0);
        assert!(req.wait(Some(Duration::ZERO)).unwrap(), "толчка нет");

        assert!(!holder.withdraw());
        assert_eq!(control.slot_claim.load(Ordering::Acquire), 0);
        assert_eq!(control.client_state.load(Ordering::Acquire), HANDSHAKE_IDLE);
    }

    /// §7.1 п. 2: два клиента одного процесса (PID и время совпадают)
    /// одновременно -- ровно один принят, второй `AlreadyConnected`; claim
    /// -- токен победителя.
    #[test]
    fn same_process_competition_single_acceptance() {
        for round in 0..200 {
            let name = unique(&format!("RACE{round}"));
            let mut server = SharedServer::start(&name).unwrap();
            let waiter = std::thread::spawn(move || {
                let result = server.wait_for_client(Some(Duration::from_secs(5)));
                (result, server)
            });
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let racers: Vec<_> = (0..2)
                .map(|_| {
                    let (barrier, name) = (barrier.clone(), name.clone());
                    std::thread::spawn(move || {
                        barrier.wait();
                        SharedClient::connect(&name, Duration::from_secs(2))
                    })
                })
                .collect();
            let results: Vec<_> = racers.into_iter().map(|r| r.join().unwrap()).collect();
            let (accepted, server) = waiter.join().unwrap();
            assert_eq!(accepted, Ok(()), "раунд {round}");
            let winners: Vec<&SharedClient> =
                results.iter().filter_map(|r| r.as_ref().ok()).collect();
            assert_eq!(winners.len(), 1, "раунд {round}: {results:?}");
            assert!(
                results
                    .iter()
                    .any(|r| matches!(r, Err(ShmError::AlreadyConnected))),
                "раунд {round}: {results:?}"
            );
            let claim = server
                .view()
                .control_block()
                .slot_claim
                .load(Ordering::Acquire);
            assert_ne!(claim, 0, "раунд {round}");
            assert_eq!(claim, winners[0].claim(), "раунд {round}");
        }
    }

    /// §7.1 п. 3: `HELLO` ставится только CAS-ом из `IDLE` -- чужая заявка
    /// (черновой клиент без claim) или чужая сессия не перетираются, а
    /// отказ не снимает их отзывом.
    #[test]
    fn withdraw_never_removes_foreign_hello() {
        for foreign in [HANDSHAKE_CLIENT_HELLO, HANDSHAKE_SERVER_READY] {
            let name = unique("FOREIGN");
            let server = SharedServer::start(&name).unwrap();
            let control = server.view().control_block();
            control.client_state.store(foreign, Ordering::Release);
            let err = SharedClient::connect(&name, Duration::from_millis(300)).unwrap_err();
            assert_eq!(err, ShmError::AlreadyConnected, "чужое состояние {foreign}");
            assert_eq!(control.client_state.load(Ordering::Acquire), foreign);
            assert_eq!(control.slot_claim.load(Ordering::Acquire), 0);
            assert_eq!(control.client_pid.load(Ordering::Acquire), 0);
        }
    }

    /// §7.1 п. 4: клиент, чью сессию сервер закрыл и чей claim отнят,
    /// уходя после подключения следующего клиента, не трогает чужую сессию.
    #[test]
    fn ghost_drop_does_not_tear_next_session() {
        let name = unique("GHOST");
        let server = SharedServer::start(&name).unwrap();
        let (mut server, ghost) = accept(server, &name);
        // Отъём: сервер отключил A (A не заметил), claim снят.
        server
            .view()
            .control_block()
            .slot_claim
            .store(0, Ordering::Release);
        server.mark_disconnected();
        let (server, next) = accept(server, &name);
        let control = server.view().control_block();
        drop(ghost);
        assert_eq!(
            control.client_state.load(Ordering::Acquire),
            HANDSHAKE_SERVER_READY
        );
        assert!(
            !server
                .events()
                .disconnect
                .wait(Some(Duration::ZERO))
                .unwrap(),
            "призрак взвёл DISCONNECT чужой сессии"
        );
        assert_eq!(control.slot_claim.load(Ordering::Acquire), next.claim());
        assert!(server.is_connected() && next.is_connected());
    }

    /// Final review M1: сессия сменилась (`generation` другой) -- отправка
    /// и ожидание места отвечают `NotConnected`, а не пишут в кольцо чужой
    /// сессии; уход такого клиента чужую сессию не трогает.
    #[test]
    fn stale_session_refuses_to_send() {
        let name = unique("STALE_SEND");
        let server = SharedServer::start(&name).unwrap();
        let (server, client) = accept(server, &name);
        client.send_to_server(b"current-session").unwrap();
        let mut buf = Vec::new();
        assert_eq!(server.receive_from_client(&mut buf).unwrap(), 15);

        let control = server.view().control_block();
        control.generation.fetch_add(1, Ordering::AcqRel);
        assert_eq!(
            client.send_to_server(b"stale-session").err(),
            Some(ShmError::NotConnected)
        );
        assert_eq!(
            client.try_send_to_server(b"stale-session").err(),
            Some(ShmError::NotConnected)
        );
        assert_eq!(
            client.wait_for_space(8, Some(Duration::ZERO)).err(),
            Some(ShmError::NotConnected)
        );
        assert!(!server.has_unread_from_client(), "запись в чужую сессию");
        drop(client);
        assert_eq!(
            control.client_state.load(Ordering::Acquire),
            HANDSHAKE_SERVER_READY
        );
    }

    // ─── Task 6 надёжности: «чужая сессия» с настоящим вторым клиентом ─────

    /// Сервер с коротким дедлайном протухшего захвата и подключённый A.
    fn stale_pair(tag: &str) -> (String, SharedServer, SharedClient) {
        let name = unique(tag);
        let mut server = SharedServer::start(&name).unwrap();
        server.set_reserve_timeout(Duration::from_millis(150));
        let (server, a) = accept(server, &name);
        (name, server, a)
    }

    /// Следующая сессия на канале, чей claim держит «не заметивший» A:
    /// сервер (поток) ждёт заявку, B повторяет `connect` -- первые попытки
    /// `AlreadyConnected` с толчком `CONNECT_REQ`, после дедлайна по толчку
    /// сервер отнимает claim A, и B подключается.
    fn take_over(mut server: SharedServer, name: &str) -> (SharedServer, SharedClient) {
        let waiter = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while let Err(err) = server.wait_for_client(Some(Duration::from_secs(5))) {
                assert!(Instant::now() < deadline, "сервер не принял B: {err:?}");
            }
            server
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        let b = loop {
            match SharedClient::connect(name, Duration::from_secs(1)) {
                Ok(b) => break b,
                Err(ShmError::AlreadyConnected) => {
                    assert!(Instant::now() < deadline, "B не подключился за 3 с");
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(err) => panic!("B: {err:?}"),
            }
        };
        (waiter.join().unwrap(), b)
    }

    /// Снимок сегмента: заголовки обоих колец и поля сессии `ControlBlock`.
    fn segment_snapshot(server: &SharedServer) -> Vec<u32> {
        let (s2c, c2s) = server.view().headers();
        let control = server.view().control_block();
        let mut snapshot = Vec::new();
        for header in [s2c, c2s] {
            snapshot.extend([
                header.write_pos.load(Ordering::Acquire),
                header.read_pos.load(Ordering::Acquire),
                header.message_count.load(Ordering::Acquire),
                header.connection_gen.load(Ordering::Acquire),
                header.handshake_state.load(Ordering::Acquire),
            ]);
        }
        snapshot.extend([
            control.client_state.load(Ordering::Acquire),
            control.server_state.load(Ordering::Acquire),
            control.slot_claim.load(Ordering::Acquire),
            control.generation.load(Ordering::Acquire),
            control.client_pid.load(Ordering::Acquire),
        ]);
        snapshot
    }

    /// Все пути записи «не заметившего» клиента -- `Err(NotConnected)`.
    fn assert_refuses_to_write(client: &mut SharedClient) {
        assert_eq!(
            client.send_to_server(b"stale").err(),
            Some(ShmError::NotConnected)
        );
        assert_eq!(
            client.try_send_to_server(b"stale").err(),
            Some(ShmError::NotConnected)
        );
        assert_eq!(
            client.send_with(8, |buf| buf.fill(b'S')).err(),
            Some(ShmError::NotConnected)
        );
        assert_eq!(
            client.wait_for_space(8, Some(Duration::ZERO)).err(),
            Some(ShmError::NotConnected)
        );
    }

    /// Task 6: сервер ушёл (`server_state = IDLE`, `DISCONNECT`), а клиент
    /// событий не ждал -- `generation` прежний, но запись уже отвергается:
    /// кольцо, которое S3 следующей сессии сбросит, не получает кадров.
    #[test]
    fn client_refuses_to_write_after_server_left() {
        let (_name, mut server, mut client) = stale_pair("LEFT_WRITE");
        let write_pos =
            |server: &SharedServer| server.view().header_b().write_pos.load(Ordering::Acquire);
        let before = write_pos(&server);
        server.disconnect_local();
        assert_refuses_to_write(&mut client);
        assert_eq!(
            write_pos(&server),
            before,
            "запись в закрытую сервером сессию"
        );
    }

    /// Task 6: настоящий второй клиент. Сервер отключил A (A не заметил),
    /// B получил claim через дедлайн по толчку и подключился. Ни одна
    /// операция A (запись, чтение, ожидание, уход) не меняет сегмент и не
    /// сигналит `DISCONNECT`; B и сервер обмениваются как ни в чём не бывало.
    #[test]
    fn stale_client_neither_writes_nor_signals_into_new_session() {
        let (name, mut server, mut a) = stale_pair("STALE_NEW");
        server.disconnect_local();
        let (mut server, b) = take_over(server, &name);
        let snapshot = segment_snapshot(&server);

        assert_refuses_to_write(&mut a);
        let mut buf = Vec::new();
        assert_eq!(
            a.receive_from_server(&mut buf).err(),
            Some(ShmError::NotConnected)
        );
        assert_eq!(
            a.poll_server(Some(Duration::ZERO)).err(),
            Some(ShmError::NotConnected)
        );
        drop(a);

        assert_eq!(segment_snapshot(&server), snapshot, "A тронул чужую сессию");
        assert!(
            !server
                .events()
                .disconnect
                .wait(Some(Duration::ZERO))
                .unwrap(),
            "A взвёл DISCONNECT чужой сессии"
        );

        b.send_to_server(b"b1").unwrap();
        assert_eq!(server.receive_from_client(&mut buf).unwrap(), 2);
        assert_eq!(buf, b"b1");
        assert!(!server.has_unread_from_client(), "лишнее в C2S сессии B");
        server.send_to_client(b"s1").unwrap();
        assert_eq!(b.receive_from_server(&mut buf).unwrap(), 2);
        assert_eq!(buf, b"s1");
        assert!(server.poll_client(Some(Duration::ZERO)).is_ok());
    }

    /// Task 6: «не заметивший» A читает, когда в S2C новой сессии лежит
    /// кадр сервера для B. Без хука -- сверка до чтения; с хуком
    /// `BEFORE_READ` сессия сменяется ровно между сверкой и чтением, и
    /// кадр отвергает сверка перед фиксацией: A -- `Err(NotConnected)` без
    /// данных, кадр остаётся B.
    #[test]
    fn stale_reader_does_not_deliver_foreign_frame() {
        // Без хука: сессия сменилась до вызова.
        let (name, mut server, a) = stale_pair("STALE_READ");
        server.disconnect_local();
        let (server, b) = take_over(server, &name);
        server.send_to_client(b"for-b").unwrap();
        let mut buf = Vec::new();
        assert_eq!(
            a.receive_from_server(&mut buf).err(),
            Some(ShmError::NotConnected)
        );
        assert_eq!(b.receive_from_server(&mut buf).unwrap(), 5);
        assert_eq!(buf, b"for-b");
        drop((a, b, server));

        // С хуком: сессия сменяется между сверкой на входе и чтением.
        let (name, mut server, a) = stale_pair("STALE_READ_HOOK");
        let next = std::rc::Rc::new(std::cell::RefCell::new(None));
        {
            let next = next.clone();
            BEFORE_READ.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    server.disconnect_local();
                    let (server, b) = take_over(server, &name);
                    server.send_to_client(b"for-b").unwrap();
                    *next.borrow_mut() = Some((server, b));
                }));
            });
        }
        let mut buf = Vec::new();
        assert_eq!(
            a.receive_from_server(&mut buf).err(),
            Some(ShmError::NotConnected)
        );
        assert!(buf.is_empty(), "A отдал кадр чужой сессии: {buf:?}");
        let (server, b) = next.borrow_mut().take().expect("хук не сработал");
        assert_eq!(
            b.receive_from_server(&mut buf).unwrap(),
            5,
            "кадр B украден"
        );
        assert_eq!(buf, b"for-b");
        drop((a, b, server));
    }

    /// Fix round 1 (находка 1 ревью): перехват claim-а при брошенном
    /// мьютексе лобби -- только из `IDLE`. Умерший держатель D оставил
    /// стоящий `CLIENT_HELLO`: перехват не трогает ни claim, ни его
    /// самоописание (иначе S2 его заявку уже не снял бы), а толкает сервер;
    /// S2 снимает заявку D. Затем claim D без заявки (`IDLE`) перехватывается.
    #[test]
    fn takeover_only_from_idle() {
        let name = unique("TAKEOVER");
        let mut server = SharedServer::start(&name).unwrap();
        // Завершившийся процесс D: PID и время создания.
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "exit"])
            .spawn()
            .unwrap();
        let pid = child.id();
        let watch = ProcessWatch::open(pid).unwrap();
        let start = watch.start_time().unwrap();
        child.wait().unwrap();
        drop(watch);
        drop(child);
        const T_D: u32 = 0x0DEA_D7A1;
        {
            let control = server.view().control_block();
            control.slot_claim.store(T_D, Ordering::Release);
            control.announce_owner_start(start);
            control.slot_owner_pid.store(pid, Ordering::Release);
            control.announce_client_start(start);
            control.client_pid.store(pid, Ordering::Release);
            control
                .client_state
                .store(HANDSHAKE_CLIENT_HELLO, Ordering::Release);
        }
        let before = claim_fields(server.view().control_block());
        let err = Hello::post(
            None,
            &name,
            true,
            ClaimMode::TakeOverAbandoned,
            FreeLink::Open,
        )
        .unwrap_err();
        assert_eq!(err, ShmError::AlreadyConnected);
        assert_eq!(claim_fields(server.view().control_block()), before);
        // Толчок разбудил сервер, S2 снял заявку умершего D и его claim.
        assert_eq!(
            server.wait_for_client(Some(Duration::from_secs(1))),
            Err(ShmError::HandshakeFailed)
        );
        let control = server.view().control_block();
        assert_eq!(control.client_state.load(Ordering::Acquire), HANDSHAKE_IDLE);
        assert_eq!(control.slot_claim.load(Ordering::Acquire), CLAIM_FREE);

        // Claim D без заявки -- перехватывается.
        control.slot_claim.store(T_D, Ordering::Release);
        let hello = Hello::post(
            None,
            &name,
            true,
            ClaimMode::TakeOverAbandoned,
            FreeLink::Open,
        )
        .unwrap();
        assert_eq!(control.slot_claim.load(Ordering::Acquire), hello.claim);
        assert!(!hello.withdraw());
        assert_eq!(control.slot_claim.load(Ordering::Acquire), CLAIM_FREE);
    }

    /// §7.1 п. 12: `connect_async` при занятом канале -- `AlreadyConnected`
    /// на первом опросе.
    #[test]
    fn connect_async_loser_already_connected() {
        let name = unique("ALOSER");
        let server = SharedServer::start(&name).unwrap();
        let holder = Hello::post(None, &name, true, ClaimMode::Take, FreeLink::Open).unwrap();
        let mut future = std::pin::pin!(SharedClient::connect_async(&name));
        let mut cx = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(Err(ShmError::AlreadyConnected)) => {}
            Poll::Ready(other) => panic!("ожидался AlreadyConnected: {:?}", other.map(drop)),
            Poll::Pending => panic!("проигравший ждёт чужого рукопожатия"),
        }
        assert!(!holder.withdraw());
        drop(server);
    }

    /// §7.1 п. 12: брошенный до ответа future освобождает claim.
    #[test]
    fn connect_async_drop_releases_claim() {
        let name = unique("ADROP");
        let server = SharedServer::start(&name).unwrap();
        let control = server.view().control_block();
        let mut future = Box::pin(SharedClient::connect_async(&name));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(future.as_mut().poll(&mut cx).is_pending());
        assert_ne!(
            control.slot_claim.load(Ordering::Acquire),
            0,
            "claim не взят"
        );
        drop(future);
        assert_eq!(control.slot_claim.load(Ordering::Acquire), 0);
        assert_eq!(control.client_state.load(Ordering::Acquire), HANDSHAKE_IDLE);
        assert_eq!(control.client_pid.load(Ordering::Acquire), 0);
    }

    /// Task 3 надёжности: `connect_async_in` -- тот же future, что
    /// `connect_async`: заявка в пространстве при первом опросе, брошенный
    /// до ответа -- откат и освобождение claim; пространство future держит
    /// до `Drop` и отпускает.
    #[test]
    fn connect_async_in_drop_releases_claim_and_namespace() {
        use crate::security::ObjectSecurity;
        use crate::server::ChannelConfig;

        fn assert_send_static<T: Send + 'static>(_: &T) {}

        let ns = PrivateNamespace::open_or_create(
            &unique("ANSDROP_B"),
            "xshm",
            &ObjectSecurity::default(),
        )
        .unwrap();
        let name = unique("ANSDROP");
        let server =
            SharedServer::start_with(&name, &ChannelConfig::default().namespace(Arc::clone(&ns)))
                .unwrap();
        let held = Arc::strong_count(&ns);
        let control = server.view().control_block();
        let mut future = Box::pin(SharedClient::connect_async_in(Arc::clone(&ns), &name));
        assert_send_static(&future);
        assert_eq!(
            Arc::strong_count(&ns),
            held + 1,
            "future держит пространство"
        );
        let mut cx = Context::from_waker(Waker::noop());
        assert!(future.as_mut().poll(&mut cx).is_pending());
        assert_ne!(
            control.slot_claim.load(Ordering::Acquire),
            0,
            "claim в пространстве не взят"
        );
        assert_eq!(
            control.client_state.load(Ordering::Acquire),
            HANDSHAKE_CLIENT_HELLO
        );
        drop(future);
        assert_eq!(
            Arc::strong_count(&ns),
            held,
            "Drop future отпускает пространство"
        );
        assert_eq!(control.slot_claim.load(Ordering::Acquire), 0);
        assert_eq!(control.client_state.load(Ordering::Acquire), HANDSHAKE_IDLE);
        assert_eq!(control.client_pid.load(Ordering::Acquire), 0);
    }

    /// §7.1 п. 12: future, брошенный после фиксации сервером, уходит штатно
    /// (L): `IDLE`, `DISCONNECT`, claim -- последним.
    #[test]
    fn connect_async_drop_after_commit_leaves() {
        let name = unique("ACOMMIT");
        let mut server = SharedServer::start(&name).unwrap();
        let mut future = Box::pin(SharedClient::connect_async(&name));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(future.as_mut().poll(&mut cx).is_pending());
        server
            .wait_for_client(Some(Duration::from_secs(1)))
            .unwrap();
        let control = server.view().control_block();
        assert_ne!(
            control.slot_claim.load(Ordering::Acquire),
            0,
            "claim не взят"
        );
        drop(future);
        assert_eq!(control.client_state.load(Ordering::Acquire), HANDSHAKE_IDLE);
        assert_eq!(control.slot_claim.load(Ordering::Acquire), 0);
        assert!(
            server
                .events()
                .disconnect
                .wait(Some(Duration::ZERO))
                .unwrap()
        );
        assert!(Endpoint::peer_left(&server));
    }

    // ─── Task 4 надёжности: регистрации `connect_async` до отпуска claim-а ──

    /// Опрос future до готовности (пауза 5 мс, не дольше 2 с); future не
    /// бросается -- исполнитель вправе держать его и после `Ready`.
    fn poll_until_ready(future: &mut ConnectFuture) -> Result<SharedClient> {
        let mut cx = Context::from_waker(Waker::noop());
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Poll::Ready(result) = Pin::new(&mut *future).poll(&mut cx) {
                return result;
            }
            assert!(Instant::now() < deadline, "future не завершился за 2 с");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Следующий клиент (заявка C1-C9 вручную) получает своё
    /// подтверждение `S2C_CONNECT`: ни одна чужая регистрация не потребила
    /// его. Иначе `wait_ack` -- `Timeout` при уже зафиксированной сервером
    /// сессии.
    fn next_client_gets_ack(server: &mut SharedServer, name: &str) {
        let hello = Hello::post(None, name, true, ClaimMode::Take, FreeLink::Open).unwrap();
        std::thread::scope(|s| {
            let accepting = s.spawn(|| server.wait_for_client(Some(Duration::from_secs(5))));
            hello.events.connect_req.set().unwrap();
            let waited = hello.wait_ack(Duration::from_secs(2), None);
            accepting.join().unwrap().unwrap();
            match waited {
                AckWait::Accepted => {}
                AckWait::Abandoned(err) => panic!(
                    "подтверждение следующего клиента съедено брошенной регистрацией: {err:?}"
                ),
            }
        });
        drop(hello.finish());
    }

    /// Сервер умер во время рукопожатия (`fired[PEER]`): future отвечает
    /// `PeerDied` и отпускает claim -- регистрация на `S2C_CONNECT` к этому
    /// моменту уже снята, иначе она съела бы подтверждение следующего
    /// клиента (автосброс), даже если future держат после `Ready`.
    #[test]
    fn connect_async_peer_death_frees_ack_before_claim() {
        let name = unique("APEERACK");
        let mut server = SharedServer::start(&name).unwrap();
        let control = server.view().control_block();
        let real = (
            control.server_pid.load(Ordering::Acquire),
            control.server_start_lo.load(Ordering::Acquire),
            control.server_start_hi.load(Ordering::Acquire),
        );

        // «Сервер» -- живой дочерний процесс (ждёт stdin).
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "pause"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let start = ProcessWatch::open(pid).unwrap().start_time().unwrap();
        let (lo, hi) = crate::layout::split_time(start);
        control.server_start_lo.store(lo, Ordering::Release);
        control.server_start_hi.store(hi, Ordering::Release);
        control.server_pid.store(pid, Ordering::Release);

        let mut future = ConnectFuture::new(None, &name);
        let mut cx = Context::from_waker(Waker::noop());
        assert!(Pin::new(&mut future).poll(&mut cx).is_pending());
        child.kill().unwrap();
        child.wait().unwrap();
        match poll_until_ready(&mut future) {
            Err(ShmError::PeerDied) => {}
            other => panic!("ожидался PeerDied: {:?}", other.map(drop)),
        }
        assert_eq!(control.slot_claim.load(Ordering::Acquire), CLAIM_FREE);

        // Настоящий сервер снова «жив»; future всё ещё не брошен.
        control.server_start_lo.store(real.1, Ordering::Release);
        control.server_start_hi.store(real.2, Ordering::Release);
        control.server_pid.store(real.0, Ordering::Release);
        next_client_gets_ack(&mut server, &name);
        assert!(future.waits.is_empty(), "регистрации пережили Ready");
        drop(future);
    }

    /// Отказ регистрации процесса сервера (инъекция): заявка отозвана на
    /// первом опросе, регистрация `S2C_CONNECT` снята до отпуска claim-а.
    #[test]
    fn connect_async_register_failure_frees_ack_before_claim() {
        let name = unique("AREGACK");
        let mut server = SharedServer::start(&name).unwrap();
        crate::asynch::FAIL_REGISTER.with(|fail| fail.set(Some(PEER)));
        let mut future = ConnectFuture::new(None, &name);
        let mut cx = Context::from_waker(Waker::noop());
        let polled = Pin::new(&mut future).poll(&mut cx);
        crate::asynch::FAIL_REGISTER.with(|fail| fail.set(None));
        match polled {
            Poll::Ready(Err(ShmError::WindowsError {
                context: "register (injected)",
                ..
            })) => {}
            Poll::Ready(other) => panic!("ожидался отказ регистрации: {:?}", other.map(drop)),
            Poll::Pending => panic!("инъекция отказа не сработала"),
        }
        assert_eq!(
            server
                .view()
                .control_block()
                .slot_claim
                .load(Ordering::Acquire),
            CLAIM_FREE
        );
        next_client_gets_ack(&mut server, &name);
        assert!(future.waits.is_empty(), "регистрации пережили Ready");
        drop(future);
    }

    /// Успешное подключение: после `Ready(Ok)` в наборе нет регистраций
    /// (процесс сервера в том числе), а заявка с handle процесса сервера
    /// перешла подключённому клиенту (Task 10: тот же handle, без второго
    /// открытия).
    #[test]
    fn connect_async_ready_clears_all_registrations() {
        let name = unique("AREADY");
        let mut server = SharedServer::start(&name).unwrap();
        let mut future = ConnectFuture::new(None, &name);
        let client = std::thread::scope(|s| {
            let accepting = s.spawn(|| server.wait_for_client(Some(Duration::from_secs(5))));
            let client = poll_until_ready(&mut future).unwrap();
            accepting.join().unwrap().unwrap();
            client
        });
        assert!(future.waits.is_empty(), "регистрации пережили Ready");
        assert!(future.hello.is_none(), "заявка не отпущена");
        assert_eq!(client.is_peer_alive(), Some(true), "сервер не наблюдается");
        drop(future);
        drop(client);
    }

    /// Сервер записал два кадра и ушёл (`disconnect_local`: состояния ->
    /// `IDLE`, потом `DISCONNECT`): клиент дочитывает их, затем
    /// `poll_server` -- `NotConnected`, сторона помечена отключённой, и
    /// `Drop` только снимает claim -- без ответного `DISCONNECT`.
    fn poll_server_sees_server_leave_with(spin: SpinPolicy) {
        let name = unique("POLL_LEAVE");
        let server = SharedServer::start(&name).unwrap();
        let (mut server, mut client) = accept(server, &name);
        let frames: [&[u8]; 2] = [b"alpha", b"beta-frame"];
        for frame in frames {
            server.send_to_client(frame).unwrap();
        }
        server.disconnect_local();

        let mut buf = Vec::new();
        for frame in frames {
            assert_eq!(
                client.poll_server_spin(Some(Duration::from_secs(5)), spin),
                Ok(true)
            );
            assert_eq!(client.receive_from_server(&mut buf), Ok(frame.len()));
            assert_eq!(buf, frame);
        }
        let started = Instant::now();
        assert_eq!(
            client.poll_server_spin(Some(Duration::from_secs(5)), spin),
            Err(ShmError::NotConnected)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(!client.is_connected());
        assert_eq!(
            client.poll_server_spin(Some(Duration::from_secs(5)), spin),
            Err(ShmError::NotConnected)
        );

        // Второй handle того же `DISCONNECT`: после `Drop` клиента взвод
        // ровно один -- сигнал сервера (возвращённый `poll`), ответного нет.
        let events = SharedEvents::open(&name).unwrap();
        drop(client);
        let control = server.view().control_block();
        assert_eq!(control.slot_claim.load(Ordering::Acquire), CLAIM_FREE);
        assert_eq!(control.client_state.load(Ordering::Acquire), HANDSHAKE_IDLE);
        assert!(events.disconnect.wait(Some(Duration::ZERO)).unwrap());
        assert!(!events.disconnect.wait(Some(Duration::ZERO)).unwrap());
    }

    /// Главный путь: клиент уже спит в `poll_server(None)`, когда сервер
    /// уходит (`disconnect_local`). Одно ожидание в ядре, `NotConnected`,
    /// сторона помечена, `DISCONNECT` возвращён взведённым, `Drop` --
    /// без ответного сигнала.
    #[test]
    fn blocked_poll_server_wakes_on_server_leave() {
        let name = unique("POLL_BLOCKED");
        let server = SharedServer::start(&name).unwrap();
        let (mut server, mut client) = accept(server, &name);
        let poller = std::thread::spawn(move || {
            let waits = || crate::ring::DATA_KERNEL_WAITS.with(std::cell::Cell::get);
            let before = waits();
            let result = client.poll_server(None);
            (result, waits() - before, Instant::now(), client)
        });
        std::thread::sleep(Duration::from_millis(100)); // клиент уже спит
        let left_at = Instant::now();
        server.disconnect_local();
        let (result, waits, woke, client) = poller.join().unwrap();
        assert_eq!(result, Err(ShmError::NotConnected));
        assert_eq!(waits, 1, "одно ожидание в ядре");
        assert!(woke.duration_since(left_at) < Duration::from_secs(1));
        assert!(!client.is_connected());
        drop(client);
        let disconnect = &server.events().disconnect;
        assert!(
            disconnect.wait(Some(Duration::ZERO)).unwrap(),
            "сигнал отключения возвращён взведённым"
        );
        assert!(!disconnect.wait(Some(Duration::ZERO)).unwrap());
        assert_eq!(
            server
                .view()
                .control_block()
                .slot_claim
                .load(Ordering::Acquire),
            CLAIM_FREE
        );
    }

    #[test]
    fn poll_server_sees_server_leave() {
        poll_server_sees_server_leave_with(SpinPolicy::NONE);
    }

    #[test]
    fn poll_server_spin_sees_server_leave() {
        poll_server_sees_server_leave_with(SpinPolicy { iterations: 1000 });
    }

    // ─── Task 10: приоритет в наборе ожидания `S2C_CONNECT` ────────────────

    /// Поток-атакующий: взводит `event` без пауз, пока `run` истинно.
    fn flood(
        event: Arc<EventHandle>,
        run: Arc<std::sync::atomic::AtomicBool>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            while run.load(Ordering::Acquire) {
                let _ = event.set();
            }
        })
    }

    /// Подключение с прерыванием: `wake` -- событие вызывающего,
    /// `running == false` -- остановка.
    fn connect_with_wake(
        name: &str,
        timeout: Duration,
        wake: &EventHandle,
        running: &std::sync::atomic::AtomicBool,
    ) -> Result<SharedClient> {
        let mut should_stop = || !running.load(Ordering::Acquire);
        SharedClient::connect_interruptible(
            None,
            name,
            timeout,
            ClaimMode::Take,
            FreeLink::Open,
            Interrupt {
                wake: wake.as_handle(),
                should_stop: &mut should_stop,
            },
        )
    }

    /// П1: поток устаревших `S2C_CONNECT` (сервер не принимает; атакующий
    /// без пауз и «противник, всегда выигрывающий гонку» --
    /// `ADVERSARY_MARK`) не голодит остановку -- `Err(NotReady)` вскоре
    /// после `wake`, а не по `connect_timeout`; заявка отозвана, claim
    /// освобождён. 20 повторов.
    #[test]
    fn connect_stop_not_starved_by_ack_flood() {
        use std::sync::atomic::AtomicBool;
        let name = unique("ACKFLOOD");
        let server = SharedServer::start(&name).unwrap();
        let control = server.view().control_block();
        control
            .slot_count
            .store(crate::server::ADVERSARY_MARK, Ordering::Release);
        let ack = Arc::new(SharedEvents::open(&name).unwrap().connect_ack);
        let run = Arc::new(AtomicBool::new(true));
        let attacker = flood(Arc::clone(&ack), Arc::clone(&run));
        let mut worst = Duration::ZERO;
        for round in 0..20 {
            let wake = Arc::new(EventHandle::create_unnamed(false).unwrap());
            let running = Arc::new(AtomicBool::new(true));
            let (client_wake, client_running, client_name) =
                (Arc::clone(&wake), Arc::clone(&running), name.clone());
            let connecting = std::thread::spawn(move || {
                connect_with_wake(
                    &client_name,
                    Duration::from_secs(10),
                    &client_wake,
                    &client_running,
                )
                .map(drop)
            });
            std::thread::sleep(Duration::from_millis(50));
            let started = Instant::now();
            running.store(false, Ordering::Release);
            wake.set().unwrap();
            let result = connecting.join().unwrap();
            worst = worst.max(started.elapsed());
            assert_eq!(result, Err(ShmError::NotReady), "повтор {round}");
            assert_eq!(control.slot_claim.load(Ordering::Acquire), CLAIM_FREE);
            assert_eq!(control.client_state.load(Ordering::Acquire), HANDSHAKE_IDLE);
        }
        run.store(false, Ordering::Release);
        attacker.join().unwrap();
        assert!(worst <= Duration::from_millis(200), "{worst:?}");
    }

    /// П2 (страж): поток `wake` от `send` приложения (`should_stop` --
    /// ложь) не голодит подтверждение -- нулевое ожидание `S2C_CONNECT`
    /// после индекса 0 видит ответ сервера.
    #[test]
    fn connect_accepts_under_send_wake_storm() {
        use std::sync::atomic::AtomicBool;
        let name = unique("SENDSTORM");
        let mut server = SharedServer::start(&name).unwrap();
        let wake = Arc::new(EventHandle::create_unnamed(false).unwrap());
        let run = Arc::new(AtomicBool::new(true));
        let storm = flood(Arc::clone(&wake), Arc::clone(&run));
        let accepting = std::thread::spawn(move || {
            let result = server.wait_for_client(Some(Duration::from_secs(5)));
            (result, server)
        });
        std::thread::sleep(Duration::from_millis(30));
        let started = Instant::now();
        // Fix round 1 (M1): `should_stop` сам снова взводит `wake` -- к
        // каждому ожиданию `wake` сигнален. Без нулевого ожидания
        // `[S2C_CONNECT, процесс]` после индекса 0 ответ сервера не был бы
        // виден никогда (таймаут).
        let mut should_stop = || {
            wake.set().unwrap();
            false
        };
        let client = SharedClient::connect_interruptible(
            None,
            &name,
            Duration::from_secs(5),
            ClaimMode::Take,
            FreeLink::Open,
            Interrupt {
                wake: wake.as_handle(),
                should_stop: &mut should_stop,
            },
        );
        let elapsed = started.elapsed();
        run.store(false, Ordering::Release);
        storm.join().unwrap();
        let (accepted, server) = accepting.join().unwrap();
        assert_eq!(accepted, Ok(()));
        assert!(client.unwrap().is_connected() && server.is_connected());
        assert!(elapsed <= Duration::from_millis(500), "{elapsed:?}");
    }

    /// Ожидание `S2C_CONNECT` наблюдает процесс сервера: сервер умер, не
    /// ответив, -- `Err(PeerDied)` сразу (а не `Timeout` через весь
    /// таймаут), заявка отозвана, claim освобождён.
    #[test]
    fn connect_sees_server_death_before_timeout() {
        let name = unique("ACKDEATH");
        let server = SharedServer::start(&name).unwrap();
        let control = server.view().control_block();
        // «Сервер» -- живой дочерний процесс (ждёт stdin).
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "pause"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let start = ProcessWatch::open(pid).unwrap().start_time().unwrap();
        let (lo, hi) = crate::layout::split_time(start);
        control.server_start_lo.store(lo, Ordering::Release);
        control.server_start_hi.store(hi, Ordering::Release);
        control.server_pid.store(pid, Ordering::Release);

        let client_name = name;
        let connecting = std::thread::spawn(move || {
            SharedClient::connect(&client_name, Duration::from_secs(10)).map(drop)
        });
        // Заявка выставлена -- клиент ждёт ответа.
        let deadline = Instant::now() + Duration::from_secs(2);
        while control.client_state.load(Ordering::Acquire) != HANDSHAKE_CLIENT_HELLO {
            assert!(Instant::now() < deadline, "заявка не выставлена");
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(Duration::from_millis(20));
        let started = Instant::now();
        child.kill().unwrap();
        child.wait().unwrap();
        let result = connecting.join().unwrap();
        let elapsed = started.elapsed();
        assert_eq!(result, Err(ShmError::PeerDied));
        assert!(elapsed <= Duration::from_secs(1), "{elapsed:?}");
        assert_eq!(control.slot_claim.load(Ordering::Acquire), CLAIM_FREE);
        assert_eq!(control.client_state.load(Ordering::Acquire), HANDSHAKE_IDLE);
    }

    // ─── Fix round 1 (решение U, M2) ─────────────────────────────────────────

    /// Сегмент «сервера» -- PID и время создания уже завершившегося
    /// процесса; `keep_handle` -- держать `Child` (handle процесса жив:
    /// ветка «открылся и сигнален»), иначе все handle закрыты (ветка
    /// `STATUS_INVALID_CID`). Возвращает сервер и, если держится, `Child`.
    fn dead_server(
        tag: &str,
        keep_handle: bool,
    ) -> (String, SharedServer, Option<std::process::Child>) {
        let name = unique(tag);
        let server = SharedServer::start(&name).unwrap();
        let control = server.view().control_block();
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "exit"])
            .spawn()
            .unwrap();
        let pid = child.id();
        let watch = ProcessWatch::open(pid).unwrap();
        let start = watch.start_time().unwrap();
        child.wait().unwrap();
        drop(watch);
        let (lo, hi) = crate::layout::split_time(start);
        control.server_start_lo.store(lo, Ordering::Release);
        control.server_start_hi.store(hi, Ordering::Release);
        control.server_pid.store(pid, Ordering::Release);
        (name, server, keep_handle.then_some(child))
    }

    /// Решение U: синхронный `connect` к серверу, чей процесс завершился до
    /// открытия, -- `Err(PeerDied)` сразу (двойник
    /// `connect_async_sees_server_dead_before_open`), откат заявки.
    fn sync_connect_sees_server_dead_before_open(keep_handle: bool) {
        let (name, server, child) = dead_server("SYNCDEAD", keep_handle);
        let control = server.view().control_block();
        let started = Instant::now();
        let result = SharedClient::connect(&name, Duration::from_secs(10)).map(drop);
        let elapsed = started.elapsed();
        assert_eq!(result, Err(ShmError::PeerDied));
        assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
        assert_eq!(control.slot_claim.load(Ordering::Acquire), CLAIM_FREE);
        assert_eq!(control.client_state.load(Ordering::Acquire), HANDSHAKE_IDLE);
        assert_eq!(control.client_pid.load(Ordering::Acquire), 0);
        drop(child);
    }

    #[test]
    fn connect_sees_server_dead_before_open() {
        sync_connect_sees_server_dead_before_open(false);
    }

    #[test]
    fn connect_sees_server_dead_before_open_with_handle_held() {
        sync_connect_sees_server_dead_before_open(true);
    }

    /// M2: поток устаревших `S2C_CONNECT` («противник» `ADVERSARY_MARK` --
    /// сигнал снова взведён к каждому ожиданию) не скрывает смерть сервера:
    /// процесс стоит в наборе после `S2C_CONNECT`, но проверяется на каждом
    /// пустом сигнале -- `PeerDied` ≤ 1 с, а не `Timeout`.
    #[test]
    fn server_death_seen_under_ack_flood() {
        let name = unique("ACKFLOODDEATH");
        let server = SharedServer::start(&name).unwrap();
        let control = server.view().control_block();
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "pause"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let start = ProcessWatch::open(pid).unwrap().start_time().unwrap();
        let (lo, hi) = crate::layout::split_time(start);
        control.server_start_lo.store(lo, Ordering::Release);
        control.server_start_hi.store(hi, Ordering::Release);
        control.server_pid.store(pid, Ordering::Release);
        control
            .slot_count
            .store(crate::server::ADVERSARY_MARK, Ordering::Release);
        let ack = SharedEvents::open(&name).unwrap().connect_ack;

        let client_name = name;
        let connecting = std::thread::spawn(move || {
            SharedClient::connect(&client_name, Duration::from_secs(10)).map(drop)
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while control.client_state.load(Ordering::Acquire) != HANDSHAKE_CLIENT_HELLO {
            assert!(Instant::now() < deadline, "заявка не выставлена");
            std::thread::sleep(Duration::from_millis(1));
        }
        // Запуск потока: первый устаревший сигнал, дальше его взводит
        // сам цикл (противник).
        ack.set().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let started = Instant::now();
        child.kill().unwrap();
        child.wait().unwrap();
        let result = connecting.join().unwrap();
        let elapsed = started.elapsed();
        control.slot_count.store(0, Ordering::Release);
        assert_eq!(result, Err(ShmError::PeerDied));
        assert!(elapsed <= Duration::from_secs(1), "{elapsed:?}");
        assert_eq!(control.slot_claim.load(Ordering::Acquire), CLAIM_FREE);
        assert_eq!(control.client_state.load(Ordering::Acquire), HANDSHAKE_IDLE);
    }

    /// Task 12 fix round 1 (решение AE, I1; воспроизведение ревью): claim
    /// держателя отнят R-O между C1 и C6, а его заявку сервер зафиксировал
    /// по чужому толчку до C8. Отзыв на C8 проигран: клиент обязан вернуть
    /// claim и уйти L (`IDLE`, `DISCONNECT`), а не оставить сессию-призрак --
    /// до исправления `client_state` оставался `SERVER_READY`, и каждый
    /// следующий клиент получал `AlreadyConnected` до выхода процесса.
    #[test]
    fn robbed_committed_hello_leaves_no_ghost() {
        let name = format!("XSHM_ROB_GHOST_{}", std::process::id());
        let mut server = SharedServer::start(&name).unwrap();
        let hook_name = name.clone();
        AFTER_HELLO.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |view: &SharedView| {
                let control = view.control_block();
                // Отъём R-O: claim снят, заявка стоит.
                control.slot_claim.store(CLAIM_FREE, Ordering::Release);
                // Толчок другого клиента -- сервер фиксирует заявку.
                EventHandle::open(ObjectPath::event(
                    None,
                    &hook_name,
                    crate::naming::Direction::ClientToServer,
                    crate::constants::EVENT_CONNECT_REQ_SUFFIX,
                ))
                .unwrap()
                .set()
                .unwrap();
                let deadline = Instant::now() + Duration::from_secs(3);
                while control.client_state.load(Ordering::Acquire) != HANDSHAKE_SERVER_READY {
                    assert!(Instant::now() < deadline, "сервер не зафиксировал заявку");
                    std::thread::sleep(Duration::from_millis(1));
                }
            }));
        });
        let acceptor = std::thread::spawn(move || {
            let accepted = server.wait_for_client(Some(Duration::from_secs(5)));
            (server, accepted)
        });
        let robbed = SharedClient::connect(&name, Duration::from_secs(2));
        let (mut server, accepted) = acceptor.join().unwrap();
        assert_eq!(robbed.err(), Some(ShmError::AlreadyConnected));
        assert_eq!(accepted, Ok(()), "сервер зафиксировал заявку");
        let control = server.view().control_block();
        assert_eq!(
            control.client_state.load(Ordering::Acquire),
            HANDSHAKE_IDLE,
            "сессия-призрак"
        );
        assert_eq!(control.slot_claim.load(Ordering::Acquire), CLAIM_FREE);
        assert_eq!(control.slot_owner_pid.load(Ordering::Acquire), 0);
        // Сервер разбирает уход и принимает следующего.
        assert_eq!(
            server.poll_client(Some(Duration::from_secs(2))),
            Err(ShmError::NotConnected)
        );
        let next_name = name;
        let next = std::thread::spawn(move || {
            SharedClient::connect(&next_name, Duration::from_secs(3)).map(drop)
        });
        server
            .wait_for_client(Some(Duration::from_secs(3)))
            .unwrap();
        assert_eq!(next.join().unwrap(), Ok(()));
    }

    /// Task 12 fix round 1 (m7): откат C6 (claim свободен, а канал занят
    /// сессией клиента без claim) освобождает взятый claim с уведомлением.
    #[test]
    fn c6_rollback_notifies() {
        let name = format!("XSHM_C6_FREE_{}", std::process::id());
        let server = SharedServer::start(&name).unwrap();
        let control = server.view().control_block();
        control
            .client_state
            .store(HANDSHAKE_SERVER_READY, Ordering::Release);
        let free = FreeNotifier::open(None, &name).unwrap();
        let g0 = free.free_gen();
        assert_eq!(
            Hello::post(None, &name, true, ClaimMode::Take, FreeLink::Open).err(),
            Some(ShmError::AlreadyConnected)
        );
        assert_eq!(control.slot_claim.load(Ordering::Acquire), CLAIM_FREE);
        assert_eq!(
            free.free_gen().wrapping_sub(g0),
            1,
            "откат C6 без уведомления"
        );
    }

    /// Task 12 fix round 1 (решение AE, I2): освобождающий без R20 (сырой
    /// store `0` без уведомления) -- `wait_connectable` находит свободный
    /// канал страховочной проверкой `fallback`; с `fallback: None` --
    /// только события, и такого освобождения он не видит.
    #[test]
    fn wait_connectable_fallback_finds_silent_release() {
        for (tag, fallback, found) in [
            ("CAP", Some(Duration::from_millis(300)), true),
            ("NOCAP", None, false),
        ] {
            let name = format!("XSHM_WAIT_SILENT_{tag}_{}", std::process::id());
            let server = SharedServer::start(&name).unwrap();
            let control = server.view().control_block();
            control.slot_claim.store(0x5111_E701, Ordering::Release);
            let waiter_name = name.clone();
            let waiter = std::thread::spawn(move || {
                let options = crate::DiscoveryOptions::default().fallback(fallback);
                let started = Instant::now();
                let result = SharedClient::wait_connectable_with(
                    &waiter_name,
                    Some(Duration::from_millis(1500)),
                    &options,
                );
                (result, started.elapsed())
            });
            // Освобождать -- только когда ждущий уже спит (учтён в
            // `free_waiters`): иначе его первая проверка застанет канал
            // свободным, и без страховки он ответит `true`.
            let free = FreeNotifier::open(None, &name).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while free.waiters() == 0 {
                assert!(Instant::now() < deadline, "{tag}: ждущий не встал");
                std::thread::sleep(Duration::from_millis(1));
            }
            std::thread::sleep(Duration::from_millis(20));
            control.slot_claim.store(CLAIM_FREE, Ordering::Release);
            let (result, took) = waiter.join().unwrap();
            assert_eq!(result, Ok(found), "{tag}: {took:?}");
            if found {
                assert!(took <= Duration::from_millis(1000), "{tag}: {took:?}");
            }
            drop(server);
        }
    }

    /// Отъём + фиксация как в `robbed_committed_hello_leaves_no_ghost`, но
    /// claim после отъёма занят держателем `holder` (его поля владельца --
    /// `owner`).
    fn robbed_with_holder(
        tag: &str,
        holder: u32,
        owner: (u32, u64),
        budget: Duration,
    ) -> (Duration, u32) {
        let name = format!("XSHM_ROB_{tag}_{}", std::process::id());
        let mut server = SharedServer::start(&name).unwrap();
        let hook_name = name.clone();
        AFTER_HELLO.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |view: &SharedView| {
                let control = view.control_block();
                // Отъём R-O, затем claim занял держатель (до своего C6).
                control.announce_owner_start(owner.1);
                control.slot_owner_pid.store(owner.0, Ordering::Release);
                control.slot_claim.store(holder, Ordering::Release);
                EventHandle::open(ObjectPath::event(
                    None,
                    &hook_name,
                    crate::naming::Direction::ClientToServer,
                    crate::constants::EVENT_CONNECT_REQ_SUFFIX,
                ))
                .unwrap()
                .set()
                .unwrap();
                let deadline = Instant::now() + Duration::from_secs(3);
                while control.client_state.load(Ordering::Acquire) != HANDSHAKE_SERVER_READY {
                    assert!(Instant::now() < deadline, "сервер не зафиксировал заявку");
                    std::thread::sleep(Duration::from_millis(1));
                }
            }));
        });
        let acceptor = std::thread::spawn(move || {
            let accepted = server.wait_for_client(Some(Duration::from_secs(5)));
            (server, accepted)
        });
        let started = Instant::now();
        let robbed = SharedClient::connect(&name, budget);
        let took = started.elapsed();
        let (mut server, accepted) = acceptor.join().unwrap();
        assert_eq!(robbed.err(), Some(ShmError::AlreadyConnected), "{tag}");
        assert_eq!(accepted, Ok(()), "{tag}: сервер зафиксировал заявку");
        assert_eq!(
            server
                .view()
                .control_block()
                .client_state
                .load(Ordering::Acquire),
            HANDSHAKE_IDLE,
            "{tag}: сессия-призрак ({took:?})"
        );
        assert_eq!(
            server.poll_client(Some(Duration::from_secs(2))),
            Err(ShmError::NotConnected),
            "{tag}: сервер не увидел ухода"
        );
        let claim = server
            .view()
            .control_block()
            .slot_claim
            .load(Ordering::Acquire);
        (took, claim)
    }

    /// Follow-up (решение AG, N1): claim после отъёма держит умерший до C6
    /// держатель -- ограбленный перехватывает его claim и уходит обычным L;
    /// claim и поля владельца после ухода свободны.
    #[test]
    fn robbed_leave_takes_over_dead_holder() {
        let dead = crate::test_support::dead_process();
        let (took, claim) =
            robbed_with_holder("TAKEOVER", 0xDEAD_7777, dead, Duration::from_secs(3));
        // Перехват -- сразу, не по истечении бюджета (запасной уход).
        assert!(took < Duration::from_secs(1), "перехвата не было: {took:?}");
        assert_eq!(claim, CLAIM_FREE, "claim умершего не перехвачен");
    }

    /// Follow-up (N1): claim держит живой держатель, не дошедший до C6, всё
    /// время бюджета (здесь -- таймаут подключения 300 мс): запасной уход
    /// по `generation` закрывает сессию-призрак (до follow-up -- уход без
    /// записей, призрак навсегда).
    #[test]
    fn robbed_leave_falls_back_by_generation() {
        let live = crate::test_support::LiveChild::spawn();
        let (took, claim) = robbed_with_holder(
            "FALLBACK",
            0x1111_7777,
            (live.pid, live.start),
            Duration::from_millis(300),
        );
        assert!(took >= Duration::from_millis(250), "{took:?}");
        assert_eq!(claim, 0x1111_7777, "claim живого держателя тронут");
        drop(live);
    }

    /// Follow-up (решение AG, N2; воспроизведение re-review): заявка
    /// умершего клиента при свободном claim-е. Раньше канал был занят
    /// навсегда: C1 удаётся (толчка проигравшего нет), C6 проваливается без
    /// толчка, а ждущий не толкал (ключ 0) -- S2 сервера не запускался.
    /// Теперь откат C6, увидевший заявку, толкает сервер, S2 снимает её, и
    /// следующая попытка подключается.
    #[test]
    fn dead_hello_with_free_claim_is_cleared_by_nudge() {
        let name = format!("XSHM_DEAD_HELLO_FREE_{}", std::process::id());
        let mut server = SharedServer::start(&name).unwrap();
        let (pid, start) = crate::test_support::dead_process();
        {
            let control = server.view().control_block();
            control.announce_owner_start(start);
            control.slot_owner_pid.store(pid, Ordering::Release);
            control.announce_client_start(start);
            control.client_pid.store(pid, Ordering::Release);
            control
                .client_state
                .store(HANDSHAKE_CLIENT_HELLO, Ordering::Release);
        }
        let acceptor = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match server.wait_for_client(Some(Duration::from_secs(5))) {
                    Ok(()) => return server,
                    Err(ShmError::HandshakeFailed) if Instant::now() < deadline => {}
                    Err(err) => panic!("сервер: {err:?}"),
                }
            }
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        let client = loop {
            match SharedClient::connect(&name, Duration::from_secs(2)) {
                Ok(client) => break client,
                Err(ShmError::AlreadyConnected) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) => panic!("канал занят навсегда: {err:?}"),
            }
        };
        let server = acceptor.join().unwrap();
        assert!(client.is_connected());
        drop(client);
        drop(server);
    }

    /// Follow-up (N2): то же для ждущего -- занятость «заявка без claim-а»
    /// получила ненулевой ключ, ждущий толкает сервер, и `wait_connectable`
    /// дожидается свободного канала (до follow-up -- `Ok(false)`).
    #[test]
    fn wait_connectable_nudges_dead_hello() {
        let name = format!("XSHM_WAIT_DEAD_HELLO_{}", std::process::id());
        let mut server = SharedServer::start(&name).unwrap();
        let (pid, start) = crate::test_support::dead_process();
        {
            let control = server.view().control_block();
            control.announce_owner_start(start);
            control.slot_owner_pid.store(pid, Ordering::Release);
            control.announce_client_start(start);
            control.client_pid.store(pid, Ordering::Release);
            control
                .client_state
                .store(HANDSHAKE_CLIENT_HELLO, Ordering::Release);
        }
        let acceptor = std::thread::spawn(move || {
            assert_eq!(
                server.wait_for_client(Some(Duration::from_secs(3))),
                Err(ShmError::HandshakeFailed),
                "S2 снимает заявку умершего"
            );
            server
        });
        let options = crate::DiscoveryOptions::default().fallback(None);
        assert_eq!(
            SharedClient::wait_connectable_with(&name, Some(Duration::from_secs(2)), &options),
            Ok(true)
        );
        drop(acceptor.join().unwrap());
    }

    /// Follow-up (решение AG, N3): отзыв снимает своё самоописание CAS-ом
    /// от своих значений -- самоописание следующего заявителя (его C5 после
    /// отъёма нашего claim-а) не затирается.
    #[test]
    fn withdraw_keeps_next_claimant_self_description() {
        let name = format!("XSHM_RETRACT_{}", std::process::id());
        let server = SharedServer::start(&name).unwrap();
        let control = server.view().control_block();
        let hello = Hello::post(None, &name, true, ClaimMode::Take, FreeLink::Open).unwrap();
        assert_eq!(
            control.client_pid.load(Ordering::Acquire),
            std::process::id()
        );
        // Следующий заявитель записал своё самоописание (C5).
        control.announce_client_start(0x1234_5678_9ABC);
        control.client_pid.store(4242, Ordering::Release);
        assert!(!hello.withdraw());
        assert_eq!(
            control.client_pid.load(Ordering::Acquire),
            4242,
            "PID затёрт"
        );
        assert_eq!(
            control.client_start_time(),
            0x1234_5678_9ABC,
            "время затёрто"
        );
        // Своё -- снимается.
        control.client_pid.store(0, Ordering::Release);
        control.announce_client_start(0);
        let hello = Hello::post(None, &name, true, ClaimMode::Take, FreeLink::Open).unwrap();
        assert!(!hello.withdraw());
        assert_eq!(control.client_pid.load(Ordering::Acquire), 0);
        assert_eq!(control.client_start_time(), 0);
    }

    /// Follow-up (решение AG, N4): своё освобождение очищает поля владельца
    /// CAS-ом от своей личности -- поля, уже переписанные следующим
    /// держателем (окно между сверкой claim-а и очисткой), целы.
    #[test]
    fn own_release_keeps_foreign_owner_fields() {
        let name = format!("XSHM_OWNER_KEEP_{}", std::process::id());
        let server = SharedServer::start(&name).unwrap();
        let control = server.view().control_block();
        let hello = Hello::post(None, &name, true, ClaimMode::Take, FreeLink::Open).unwrap();
        // Окно: claim ещё наш, а поля уже следующего держателя (его C2).
        control.announce_owner_start(0x0BAD_0000_0001);
        control.slot_owner_pid.store(4242, Ordering::Release);
        assert!(!hello.withdraw());
        assert_eq!(control.slot_claim.load(Ordering::Acquire), CLAIM_FREE);
        assert_eq!(
            control.slot_owner_pid.load(Ordering::Acquire),
            4242,
            "поля следующего"
        );
        assert_eq!(control.owner_start_time(), 0x0BAD_0000_0001);
    }

    /// Fix round 1 (R18): клиент перед освобождением своего claim-а очищает
    /// поля владельца -- после отката, отзыва и ухода `slot_owner_pid == 0`.
    #[test]
    fn own_release_clears_owner_fields() {
        let name = format!("XSHM_OWNER_CLEAR_{}", std::process::id());
        let mut server = SharedServer::start(&name).unwrap();
        let owner = |server: &SharedServer| {
            let control = server.view().control_block();
            (
                control.slot_owner_pid.load(Ordering::Acquire),
                control.owner_start_time(),
                control.slot_claim.load(Ordering::Acquire),
            )
        };
        // Отзыв (заявка без ответа).
        let hello = Hello::post(None, &name, true, ClaimMode::Take, FreeLink::Open).unwrap();
        assert_eq!(owner(&server).0, std::process::id());
        assert!(!hello.withdraw());
        assert_eq!(owner(&server), (0, 0, CLAIM_FREE), "отзыв");
        drop(hello);
        // Уход L подключённого.
        let client_name = name;
        let client =
            std::thread::spawn(move || SharedClient::connect(&client_name, Duration::from_secs(3)));
        server
            .wait_for_client(Some(Duration::from_secs(3)))
            .unwrap();
        let client = client.join().unwrap().unwrap();
        assert_ne!(owner(&server).0, 0);
        drop(client);
        assert_eq!(owner(&server), (0, 0, CLAIM_FREE), "уход");
    }

    /// Task 12 (R21): `wait_connectable` спит, пока канал занят, и
    /// возвращает `true` сразу после освобождения claim-а (здесь -- отзыв
    /// заявки A, уведомление R20); claim он не берёт. Занят до конца
    /// таймаута -- `false`; канала нет -- ошибка открытия.
    #[test]
    fn wait_connectable_returns_on_release() {
        let name = format!("XSHM_WAIT_CONNECTABLE_{}", std::process::id());
        let server = SharedServer::start(&name).unwrap();
        let a = Hello::post(None, &name, true, ClaimMode::Take, FreeLink::Open).unwrap();

        let started = Instant::now();
        assert_eq!(
            SharedClient::wait_connectable(&name, Some(Duration::from_millis(200))),
            Ok(false),
            "занят -- таймаут"
        );
        assert!(started.elapsed() >= Duration::from_millis(190));

        let waiter_name = name.clone();
        let waiter = std::thread::spawn(move || {
            let result = SharedClient::wait_connectable(&waiter_name, Some(Duration::from_secs(5)));
            (result, Instant::now())
        });
        std::thread::sleep(Duration::from_millis(200));
        let released = Instant::now();
        assert!(!a.withdraw(), "отзыв удался");
        let (result, returned) = waiter.join().unwrap();
        assert_eq!(result, Ok(true));
        let took = returned.saturating_duration_since(released);
        // m6: событие против страховки 10 с -- запас под нагрузкой.
        assert!(took <= Duration::from_millis(400), "{took:?}");
        let control = server.view().control_block();
        assert_eq!(
            control.slot_claim.load(Ordering::Acquire),
            CLAIM_FREE,
            "ждущий claim не берёт"
        );
        drop(a);
        assert_eq!(
            SharedClient::wait_connectable(&name, Some(Duration::ZERO)),
            Ok(true),
            "свободен"
        );
        drop(server);
        assert!(SharedClient::wait_connectable(&name, Some(Duration::ZERO)).is_err());
    }

    /// Task 12 (резерв R-M): канал без уведомителя (`<имя>_free` занят
    /// автосбросным событием -- как пир без R20) -- `wait_connectable`
    /// проверяет раз в 250 мс и всё равно дожидается освобождения.
    #[test]
    fn wait_connectable_falls_back_without_free_event() {
        let name = format!("XSHM_WAIT_CONNECTABLE_FB_{}", std::process::id());
        let _squatter = EventHandle::create(
            ObjectPath::scoped(None, &name, "_free"),
            &crate::ObjectSecurity::default().attributes().unwrap(),
        )
        .unwrap();
        let server = SharedServer::start(&name).unwrap();
        assert!(
            FreeNotifier::open(None, &name).is_err(),
            "подменённый _free"
        );
        let a = Hello::post(None, &name, true, ClaimMode::Take, FreeLink::Open).unwrap();
        let waiter = std::thread::spawn(move || {
            SharedClient::wait_connectable(&name, Some(Duration::from_secs(5)))
        });
        std::thread::sleep(Duration::from_millis(100));
        let released = Instant::now();
        assert!(!a.withdraw());
        assert_eq!(waiter.join().unwrap(), Ok(true));
        let took = released.elapsed();
        assert!(took <= Duration::from_millis(600), "{took:?}");
        drop(a);
        drop(server);
    }
}

#[cfg(test)]
mod presence_tests {
    //! Task 11 надёжности (R19): синхронное ожидание сервера на маяке.

    use super::*;
    use crate::presence::PRESENCE_KERNEL_WAITS;
    use crate::server::SharedServer;

    fn unique(tag: &str) -> String {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        format!(
            "CLI_PRES_{tag}_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn kernel_waits() -> u32 {
        PRESENCE_KERNEL_WAITS.with(std::cell::Cell::get)
    }

    /// Ждущий просыпается по взводу маяка сервера (≤ 100 мс), без сервера
    /// -- `Ok(false)` по таймауту после ОДНОГО ожидания в ядре.
    #[test]
    fn wait_for_server_returns_on_start() {
        let name = unique("START");
        let waiter = {
            let name = name.clone();
            std::thread::spawn(move || {
                let waited = SharedClient::wait_for_server(&name, Some(Duration::from_secs(5)));
                (waited, Instant::now(), kernel_waits())
            })
        };
        std::thread::sleep(Duration::from_millis(200));
        let started = Instant::now();
        let server = SharedServer::start(&name).unwrap();
        let (waited, woke_at, waits) = waiter.join().unwrap();
        assert_eq!(waited, Ok(true));
        let took = woke_at - started;
        assert!(
            took <= Duration::from_millis(100),
            "проснулся через {took:?}"
        );
        assert_eq!(waits, 1, "ожиданий в ядре: {waits}");
        // Сервер на месте -- сразу.
        assert_eq!(
            SharedClient::wait_for_server(&name, Some(Duration::ZERO)),
            Ok(true)
        );
        drop(server);

        let absent = unique("ABSENT");
        let before = kernel_waits();
        let started = Instant::now();
        assert_eq!(
            SharedClient::wait_for_server(&absent, Some(Duration::from_millis(200))),
            Ok(false)
        );
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(190), "{took:?}");
        assert_eq!(kernel_waits() - before, 1, "одно ожидание в ядре");
    }
}

#[cfg(test)]
mod exited_server_tests {
    //! Решение X: сервер зафиксировал рукопожатие и умер раньше, чем клиент
    //! открыл его процесс; отзыв заявки проигран -- клиент подключён, а
    //! наблюдать некого. Он обязан узнать о смерти сервера (`PeerDied`
    //! после дочитывания кольца), а не ждать вечно.

    use super::*;
    use crate::server::SharedServer;
    use crate::test_support::dead_process;

    /// Канал, чей сервер в сегменте -- процесс `(pid, start)`.
    fn forged_server(tag: &str, (pid, start): (u32, u64)) -> (String, SharedServer) {
        let name = format!("CLI_EXITED_{tag}_{}", std::process::id());
        let server = SharedServer::start(&name).unwrap();
        let control = server.view().control_block();
        control.server_start_lo.store(
            u32::try_from(start & 0xFFFF_FFFF).unwrap(),
            Ordering::Release,
        );
        control
            .server_start_hi
            .store(u32::try_from(start >> 32).unwrap(), Ordering::Release);
        control.server_pid.store(pid, Ordering::Release);
        (name, server)
    }

    /// Хук после заявки: «сервер» пишет прощание, публикует сессию (S3) и
    /// фиксирует заявку (S5) -- и умирает (больше ничего не делает).
    fn commit_and_die_after_hello() {
        AFTER_HELLO.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(|view: &SharedView| {
                view.ring_a().write_message(b"farewell").unwrap();
                let control = view.control_block();
                control.generation.fetch_add(1, Ordering::AcqRel);
                control
                    .server_state
                    .store(HANDSHAKE_SERVER_READY, Ordering::Release);
                control
                    .client_state
                    .compare_exchange(
                        HANDSHAKE_CLIENT_HELLO,
                        HANDSHAKE_SERVER_READY,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .unwrap();
            }));
        });
    }

    /// Минимальный исполнитель: опрос с паузой (регистрации реактора живут
    /// между опросами, waker не нужен).
    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let mut cx = Context::from_waker(Waker::noop());
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
                return output;
            }
            assert!(Instant::now() < deadline, "future не завершился за 5 с");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// `recv_async`: прощание дочитано, затем `PeerDied` -- без вечного
    /// ожидания (M8 ревью T11b: регистрация на всегда сигнальном
    /// наблюдении срабатывает один раз, без цикла).
    fn assert_async_drain_then_peer_died(client: &mut SharedClient) {
        let started = Instant::now();
        let mut buffer = Vec::new();
        let len = block_on(client.recv_async(&mut buffer)).unwrap();
        assert_eq!(&buffer[..len], b"farewell", "прощание дочитано");
        assert_eq!(
            block_on(client.recv_async(&mut buffer)),
            Err(ShmError::PeerDied)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn committed_then_dead_server_is_reported_after_drain() {
        let (name, server) = forged_server("SYNC", dead_process());
        commit_and_die_after_hello();
        let mut client = SharedClient::connect(&name, Duration::from_secs(2))
            .expect("отзыв проигран -- подключён");
        assert_eq!(
            client.is_peer_alive(),
            Some(false),
            "смерть сервера известна"
        );
        let started = Instant::now();
        assert_eq!(client.poll_server(Some(Duration::from_secs(2))), Ok(true));
        let mut buffer = Vec::new();
        let len = client.receive_from_server(&mut buffer).unwrap();
        assert_eq!(&buffer[..len], b"farewell", "прощание дочитано");
        assert_eq!(
            client.poll_server(Some(Duration::from_secs(2))),
            Err(ShmError::PeerDied)
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "без ожидания таймаута"
        );
        drop(client);
        drop(server);
    }

    /// M8: тот же исход синхронного `connect`, потребитель -- `recv_async`.
    #[test]
    fn committed_then_dead_server_is_reported_to_recv_async() {
        let (name, server) = forged_server("SYNC_ASYNC", dead_process());
        commit_and_die_after_hello();
        let mut client = SharedClient::connect(&name, Duration::from_secs(2))
            .expect("отзыв проигран -- подключён");
        assert_async_drain_then_peer_died(&mut client);
        drop(client);
        drop(server);
    }

    /// M1 ревью T11b: `connect_async` бросает заявку по широкому
    /// `server_gone` -- PID сервера занят живым процессом с другим временем
    /// создания (сервер умер, PID переиспользован). Отзыв проигран -- клиент
    /// подключён и тоже получает наблюдение «уже завершился»: `recv_async`
    /// дочитывает и сообщает `PeerDied`, а не ждёт вечно.
    #[test]
    fn async_gone_server_with_lost_withdraw_is_reported_after_drain() {
        let reused = (
            std::process::id(),
            crate::win::current_process_start_time() ^ 0x5A5A,
        );
        let (name, server) = forged_server("ASYNC_GONE", reused);
        commit_and_die_after_hello();
        let mut client =
            block_on(SharedClient::connect_async(&name)).expect("отзыв проигран -- подключён");
        assert_eq!(client.is_peer_alive(), Some(false));
        assert_async_drain_then_peer_died(&mut client);
        drop(client);
        drop(server);
    }

    /// N1 повторного ревью T11b: тот же исход у ЖИВОГО сервера, чьё время в
    /// сегменте испорчено (`connect_async` вывел смерть по несовпадению
    /// времени, а сервер успел зафиксировать). Клиент сообщает `PeerDied`
    /// (решение V), но уходит полным L: сервер видит уход (`IDLE` и
    /// `DISCONNECT`), claim свободен, и следующий клиент принимается -- сервер
    /// не застревает в сессии с `AlreadyConnected` для всех.
    #[test]
    fn async_inferred_death_still_leaves_live_server_fully() {
        let corrupted = (
            std::process::id(),
            crate::win::current_process_start_time() ^ 0x5A5A,
        );
        let (name, mut server) = forged_server("ASYNC_LIVE", corrupted);
        commit_and_die_after_hello();
        let mut client =
            block_on(SharedClient::connect_async(&name)).expect("отзыв проигран -- подключён");
        assert_async_drain_then_peer_died(&mut client);
        drop(client);
        let control = server.view().control_block();
        assert_eq!(
            control.client_state.load(Ordering::Acquire),
            HANDSHAKE_IDLE,
            "живой сервер остался в сессии ушедшего клиента"
        );
        assert!(
            server
                .events()
                .disconnect
                .wait(Some(Duration::ZERO))
                .unwrap(),
            "сервер не получил DISCONNECT ухода"
        );
        assert_eq!(control.slot_claim.load(Ordering::Acquire), CLAIM_FREE);
        // Следующий клиент принимается (не `AlreadyConnected`).
        let next =
            { std::thread::spawn(move || SharedClient::connect(&name, Duration::from_secs(5))) };
        server
            .wait_for_client(Some(Duration::from_secs(5)))
            .unwrap();
        assert!(next.join().unwrap().is_ok());
    }
}

#[cfg(test)]
mod stale_leave_tests {
    //! T11b: уход клиента после того, как сервер сам закрыл сессию, не
    //! оставляет устаревший `DISCONNECT` следующей сессии.

    use super::*;
    use crate::server::SharedServer;

    #[test]
    fn drop_after_server_closed_session_sends_no_disconnect() {
        let name = format!("CLI_STALE_LEAVE_{}", std::process::id());
        let mut server = SharedServer::start(&name).unwrap();
        let client = {
            let name = name.clone();
            std::thread::spawn(move || SharedClient::connect(&name, Duration::from_secs(5)))
        };
        server
            .wait_for_client(Some(Duration::from_secs(5)))
            .unwrap();
        let client = client.join().unwrap().unwrap();
        server.disconnect().unwrap();
        let disconnect_fired = |server: &SharedServer| {
            server
                .events()
                .disconnect
                .wait(Some(Duration::ZERO))
                .unwrap()
        };
        // Сигнал сервера клиенту -- разобран (клиент его так и не увидел).
        assert!(disconnect_fired(&server));
        drop(client);
        assert!(
            !disconnect_fired(&server),
            "клиент взвёл DISCONNECT закрытой сервером сессии"
        );
        assert_eq!(
            server
                .view()
                .control_block()
                .slot_claim
                .load(Ordering::Acquire),
            CLAIM_FREE,
            "claim снят"
        );

        // Уход клиента из живой сессии сигналит, как прежде.
        let client =
            std::thread::spawn(move || SharedClient::connect(&name, Duration::from_secs(5)));
        server
            .wait_for_client(Some(Duration::from_secs(5)))
            .unwrap();
        drop(client.join().unwrap().unwrap());
        assert!(disconnect_fired(&server), "штатный уход");
    }
}

#[cfg(test)]
mod reclaimed_leave_tests {
    //! M5 ревью T11b: claim «не заметившего» клиента отнят (и, возможно,
    //! уже взят следующим), а `generation` ещё прежний -- уход не трогает
    //! состояние и не сигналит `DISCONNECT`.

    use super::*;
    use crate::server::SharedServer;

    #[test]
    fn leave_after_claim_was_taken_writes_nothing() {
        let name = format!("CLI_RECLAIMED_{}", std::process::id());
        let mut server = SharedServer::start(&name).unwrap();
        let client =
            { std::thread::spawn(move || SharedClient::connect(&name, Duration::from_secs(5))) };
        server
            .wait_for_client(Some(Duration::from_secs(5)))
            .unwrap();
        let client = client.join().unwrap().unwrap();
        let control = server.view().control_block();
        // Claim отнят и взят следующим клиентом (его рукопожатие ещё не
        // опубликовало новое поколение).
        control.slot_claim.store(0x0BAD_C1A1, Ordering::Release);
        drop(client);
        assert_eq!(
            control.client_state.load(Ordering::Acquire),
            HANDSHAKE_SERVER_READY,
            "уход тронул состояние чужого claim-а"
        );
        assert!(
            !server
                .events()
                .disconnect
                .wait(Some(Duration::ZERO))
                .unwrap(),
            "уход взвёл DISCONNECT"
        );
        assert_eq!(control.slot_claim.load(Ordering::Acquire), 0x0BAD_C1A1);
    }

    /// Решение AB: поток уходящего клиента вытеснен между сверкой
    /// `generation` и CAS -- за это время сервер отнял его claim (по
    /// `RESERVE_TIMEOUT`) и зафиксировал сессию следующего клиента B. CAS
    /// пары `(client_state, slot_claim)` не проходит (claim уже B), и сессия
    /// B остаётся нетронутой: ни `IDLE`, ни `DISCONNECT`. С 32-битным CAS
    /// одного `client_state` уход снял бы сессию B.
    #[test]
    fn leave_racing_reclaim_and_next_commit_spares_new_session() {
        const B_TOKEN: u32 = 0x0BAD_B0B0;
        let name = format!("CLI_RACE_LEAVE_{}", std::process::id());
        let mut server = SharedServer::start(&name).unwrap();
        let client =
            { std::thread::spawn(move || SharedClient::connect(&name, Duration::from_secs(5))) };
        server
            .wait_for_client(Some(Duration::from_secs(5)))
            .unwrap();
        let client = client.join().unwrap().unwrap();
        // Сигнал сервера отсутствует -- проверяем ровно сигнал ухода.
        assert!(
            !server
                .events()
                .disconnect
                .wait(Some(Duration::ZERO))
                .unwrap()
        );
        BEFORE_LEAVE_CAS.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(|view: &SharedView| {
                // Сверка `generation` уже пройдена. «Сервер»: отключил
                // клиента, отнял claim, принял B (S3 -- новое поколение, S5 --
                // фиксация заявки B).
                let control = view.control_block();
                control
                    .client_state
                    .store(HANDSHAKE_IDLE, Ordering::Release);
                control.slot_claim.store(B_TOKEN, Ordering::Release);
                control.generation.fetch_add(1, Ordering::AcqRel);
                control
                    .client_state
                    .store(HANDSHAKE_SERVER_READY, Ordering::Release);
            }));
        });
        drop(client);
        let control = server.view().control_block();
        assert_eq!(
            control.client_state.load(Ordering::Acquire),
            HANDSHAKE_SERVER_READY,
            "уход снял сессию следующего клиента"
        );
        assert!(
            !server
                .events()
                .disconnect
                .wait(Some(Duration::ZERO))
                .unwrap(),
            "уход взвёл DISCONNECT в сессии следующего клиента"
        );
        assert_eq!(control.slot_claim.load(Ordering::Acquire), B_TOKEN);
    }
}

#[cfg(test)]
mod stale_disconnect_tests {
    //! T11c (решение AD): синхронные ожидания (`poll_*`, `wait_for_space`)
    //! принимают `DISCONNECT` за уход только при ушедшем пире -- по тем же
    //! предикатам, что worker-ы и async (`client_left`/`server_left`).

    use super::*;
    use crate::server::SharedServer;
    use std::time::Instant;

    fn unique(tag: &str) -> String {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        format!(
            "CLI_STALE_DISC_{tag}_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

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

    /// `poll_client`/`poll_server`: устаревший сигнал поглощается (событие
    /// не возвращено, ожидание дошло до таймаута), сессия жива; настоящий
    /// уход сервера -- `NotConnected`.
    #[test]
    fn sync_poll_absorbs_stale_disconnect() {
        let name = unique("POLL");
        let (mut server, mut client) = accept(SharedServer::start(&name).unwrap(), &name);
        let events = SharedEvents::open(&name).unwrap();

        events.disconnect.set().unwrap();
        assert_eq!(
            server.poll_client(Some(Duration::from_millis(100))),
            Ok(false)
        );
        assert!(
            !events.disconnect.wait(Some(Duration::ZERO)).unwrap(),
            "устаревший сигнал возвращён событию"
        );
        events.disconnect.set().unwrap();
        assert_eq!(
            client.poll_server(Some(Duration::from_millis(100))),
            Ok(false)
        );
        assert!(server.is_connected() && client.is_connected());

        server.disconnect().unwrap();
        assert_eq!(
            client.poll_server(Some(Duration::from_secs(2))),
            Err(ShmError::NotConnected)
        );
    }

    /// `wait_for_space` клиента: сервер сменил сессию (`generation`), не
    /// выходя из `SERVER_READY` (принял другого клиента), и `DISCONNECT`
    /// пришёл -- это уход для нашей сессии: `NotConnected` сразу, как у
    /// остальных потребителей (`server_left`). До T11c здесь сверялось
    /// только `server_state`, и ожидание тянулось до таймаута.
    #[test]
    fn client_wait_for_space_sees_foreign_session() {
        let name = unique("SPACE_GEN");
        let config = crate::server::ChannelConfig::new(ChannelLayout::symmetric(
            crate::layout::RingConfig::new(4096),
        ));
        let (server, client) = accept(SharedServer::start_with(&name, &config).unwrap(), &name);
        let payload = [1u8; 1000];
        while client.try_send_to_server(&payload).is_ok() {}
        let events = SharedEvents::open(&name).unwrap();
        let switcher = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            // Сессия сменилась до сигнала (как S3 следующего рукопожатия).
            server
                .view()
                .control_block()
                .generation
                .fetch_add(1, Ordering::AcqRel);
            events.disconnect.set().unwrap();
            server
        });
        let started = Instant::now();
        let result = client.wait_for_space(payload.len(), Some(Duration::from_secs(3)));
        let elapsed = started.elapsed();
        drop(switcher.join().unwrap());
        assert_eq!(result, Err(ShmError::NotConnected));
        assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
    }
}
