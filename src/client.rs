use std::future::Future;
use std::marker::PhantomData;
use std::os::windows::io::{AsHandle, BorrowedHandle};
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use crate::asynch::{Endpoint, OP, PEER, RecvFuture, SendFuture, WaitSet};
use crate::constants::{
    CLAIM_FREE, HANDSHAKE_CLIENT_HELLO, HANDSHAKE_IDLE, HANDSHAKE_SERVER_READY,
};
use crate::error::{Result, ShmError};
use crate::events::SharedEvents;
use crate::layout::ChannelLayout;
use crate::namespace::PrivateNamespace;
use crate::naming::ObjectPath;
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
/// PID/время клиента и заголовки -- в `IDLE`. Claim здесь не трогается --
/// его освобождает `Hello::withdraw` (после отката, R18).
fn withdraw_hello(view: &SharedView, generation_before: u32) -> bool {
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
    // `ControlBlock::client_pid`). Claim ещё наш -- чужих записей здесь нет.
    control.client_pid.store(0, Ordering::Release);
    control.announce_client_start(0);
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

/// Прерывание ожидания `S2C_CONNECT` (ревизия 2): событие своего процесса
/// (`wake` worker-а) и проверка «пора остановиться». Сработал `wake`, а
/// `should_stop()` вернул `true` -- заявка отзывается, `connect` возвращает
/// `Err(NotReady)`; иначе (например, `wake` от `send`) ожидание продолжается.
pub(crate) struct Interrupt<'a> {
    pub(crate) wake: BorrowedHandle<'a>,
    pub(crate) should_stop: &'a mut dyn FnMut() -> bool,
}

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
    pub fn connect(name: &str, timeout: Duration) -> Result<Self> {
        Self::connect_impl(None, name, timeout, true, ClaimMode::Take, None)
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
        Self::connect_impl(Some(ns), name, timeout, true, ClaimMode::Take, None)
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
        interrupt: Interrupt<'_>,
    ) -> Result<Self> {
        Self::connect_impl(namespace, name, timeout, true, claim, Some(interrupt))
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
        mut interrupt: Option<Interrupt<'_>>,
    ) -> Result<Self> {
        let hello = Hello::post(namespace, name, announce_pid, claim)?;
        // C9: сигнал серверу, C10: ожидание ответа.
        let waited = match hello.events.connect_req.set() {
            Ok(()) => hello.wait_ack(timeout, interrupt.as_mut()),
            Err(err) => AckWait::Abandoned(err),
        };
        // Отзыв проиграл CAS сервера -- рукопожатие состоялось: продолжаем
        // как подключённые (сервер ждёт нас; если уже отключил -- worker
        // увидит `DISCONNECT` и дочитает кольцо).
        if let AckWait::Abandoned(err) = waited
            && !hello.withdraw()
        {
            return Err(err);
        }
        Ok(hello.finish())
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
        ConnectFuture {
            name: name.to_owned(),
            hello: None,
            started: false,
            server: None,
            waits: WaitSet::default(),
        }
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

/// Освободить свой claim (CAS `token -> CLAIM_FREE`): если он уже не наш
/// (отъём по дедлайну), -- no-op.
fn release_claim(view: &SharedView, token: u32) {
    let _ = view.control_block().slot_claim.compare_exchange(
        token,
        CLAIM_FREE,
        Ordering::AcqRel,
        Ordering::Acquire,
    );
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
    pub(crate) fn post(
        namespace: Option<&PrivateNamespace>,
        name: &str,
        announce_pid: bool,
        claim: ClaimMode,
    ) -> Result<Self> {
        // C0: открыть всё, не трогая память (ошибки -- без побочных эффектов).
        let mapping = Mapping::open(ObjectPath::mapping(namespace, name))?;
        // SAFETY: отображение длиной `mapping.size()` живёт, пока жив
        // `mapping`, а он переезжает в возвращаемую структуру вместе с view;
        // адрес view выровнен на гранулу выделения.
        let view = unsafe { SharedView::attach(mapping.as_ptr(), mapping.size()) }?;
        let control = view.control_block();
        let events = SharedEvents::open_in(namespace, name)?;

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
                release_claim(&view, token);
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
        if announce_pid {
            control.announce_client_start(announced_start_time());
            control
                .client_pid
                .store(std::process::id(), Ordering::Release);
        }

        // C6: заявка -- только CAS из `IDLE` (ревизия 3): чужие `HELLO` или
        // сессия (клиент без claim) не перетираются.
        let posted = match claim {
            #[cfg(test)]
            ClaimMode::Legacy => {
                control
                    .client_state
                    .store(HANDSHAKE_CLIENT_HELLO, Ordering::Release);
                true
            }
            _ => control
                .client_state
                .compare_exchange(
                    HANDSHAKE_IDLE,
                    HANDSHAKE_CLIENT_HELLO,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok(),
        };
        if !posted {
            if announce_pid {
                control.client_pid.store(0, Ordering::Release);
                control.announce_client_start(0);
            }
            release();
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

        let hello = Self {
            view,
            events,
            generation_before,
            claim: token,
            owns_claim,
            mapping,
        };

        // C8: claim всё ещё наш? Нет -- его отняли по дедлайну (держатель
        // завис между C1 и C6 дольше `RESERVE_TIMEOUT`, нарушение A1):
        // отзыв, а если сервер успел зафиксировать -- немедленный уход.
        if token != CLAIM_FREE
            && hello
                .view
                .control_block()
                .slot_claim
                .load(Ordering::Acquire)
                != token
        {
            if hello.withdraw() {
                drop(hello.finish());
            }
            return Err(ShmError::AlreadyConnected);
        }
        Ok(hello)
    }

    /// Сигнал `S2C_CONNECT` -- ответ на эту заявку (C10).
    fn accepted(&self) -> bool {
        accepted_on_signal(&self.view, self.generation_before)
    }

    /// Ждать `S2C_CONNECT` (и `wake` вызывающего, если задан) до `timeout`
    /// (C10). Устаревший сигнал (`accepted_on_signal == false`) поглощается,
    /// ожидание продолжается.
    pub(crate) fn wait_ack(
        &self,
        timeout: Duration,
        mut interrupt: Option<&mut Interrupt<'_>>,
    ) -> AckWait {
        // Таймаут, непредставимый как `Instant` (`Duration::MAX`), -- ждать без
        // дедлайна, а не паниковать на переполнении.
        let deadline = crate::ring::deadline_of(Some(timeout));
        loop {
            let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
            if left.is_some_and(|left| left.is_zero()) {
                return AckWait::Abandoned(ShmError::Timeout);
            }
            let fired = match interrupt.as_deref_mut() {
                Some(int) => wait_any(&[self.events.connect_ack.as_handle(), int.wake], left),
                None => self
                    .events
                    .connect_ack
                    .wait(left)
                    .map(|signaled| signaled.then_some(0)),
            };
            match fired {
                Ok(Some(0)) => {
                    if self.accepted() {
                        return AckWait::Accepted;
                    }
                }
                Ok(Some(_)) => {
                    if interrupt
                        .as_deref_mut()
                        .is_some_and(|int| (int.should_stop)())
                    {
                        return AckWait::Abandoned(ShmError::NotReady);
                    }
                }
                Ok(None) => return AckWait::Abandoned(ShmError::Timeout),
                Err(err) => return AckWait::Abandoned(err),
            }
        }
    }

    /// Отзыв W: `true` -- поздно, рукопожатие состоялось (продолжать как
    /// подключённые); `false` -- заявка отозвана и откатана, claim (свой)
    /// освобождён последним.
    pub(crate) fn withdraw(&self) -> bool {
        if withdraw_hello(&self.view, self.generation_before) {
            return true;
        }
        if self.owns_claim {
            release_claim(&self.view, self.claim);
        }
        false
    }

    /// Рукопожатие состоялось (шаг A): подключённый клиент.
    pub(crate) fn finish(self) -> SharedClient {
        let Self {
            view,
            events,
            generation_before: _,
            claim,
            owns_claim,
            mapping,
        } = self;
        let control = view.control_block();
        let (header_a, header_b) = view.headers();

        // PID сервера (0 -- сервер старой версии: наблюдения нет, как в 0.7).
        let peer = ProcessWatch::open_peer(
            control.server_pid.load(Ordering::Acquire),
            control.server_start_time(),
        );

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
            _mapping: mapping,
            _not_sync: PhantomData,
        }
    }
}

/// Future `connect_async`: заявка при первом опросе, ожидание
/// `S2C_CONNECT` в реакторе, откат заявки в `Drop`.
struct ConnectFuture {
    name: String,
    /// Выставленная заявка (до подтверждения или отказа).
    hello: Option<Hello>,
    /// Первый опрос уже был (заявка выставлялась).
    started: bool,
    /// Процесс сервера (PID и время создания из `ControlBlock`): его смерть
    /// во время рукопожатия -- `Err(PeerDied)`. `None` -- сервер старой
    /// версии без PID или процесс не открылся (другая сессия без прав):
    /// тогда, как у синхронного `connect`, границу задаёт только таймаут
    /// (у async -- таймаут исполнителя).
    server: Option<ProcessWatch>,
    waits: WaitSet,
}

impl ConnectFuture {
    fn step(&mut self, waker: &Waker) -> Poll<Result<SharedClient>> {
        if !self.started {
            self.started = true;
            let hello = match Hello::post(None, &self.name, true, ClaimMode::Take) {
                Ok(hello) => hello,
                Err(err) => return Poll::Ready(Err(err)),
            };
            if let Err(err) = hello.events.connect_req.set() {
                // Как синхронный путь: отзыв, проигравший CAS сервера, --
                // всё же подключение.
                return Poll::Ready(if hello.withdraw() {
                    Ok(hello.finish())
                } else {
                    Err(err)
                });
            }
            let control = hello.view.control_block();
            let (pid, start) = (
                control.server_pid.load(Ordering::Acquire),
                control.server_start_time(),
            );
            self.server = ProcessWatch::open_peer(pid, start);
            // Task 18 (Task 16 N1): `open_peer` не наблюдает уже мёртвый
            // процесс («неизвестно» -- верно для PID из рукопожатия). Здесь
            // PID -- сервер, владеющий открытым сегментом: подтверждённо
            // ушедший (`is_process_gone`: процесса нет, он завершён или PID
            // занят процессом с другим временем создания) -- `PeerDied` с
            // откатом, как при таймауте. Иначе future ждал бы вечно.
            // Двусмысленность (нет прав, PID 0) -- без наблюдения, как раньше.
            if self.server.is_none() && crate::win::is_process_gone(pid, start) {
                return Poll::Ready(if hello.withdraw() {
                    Ok(hello.finish())
                } else {
                    Err(ShmError::PeerDied)
                });
            }
            self.hello = Some(hello);
        }
        let Some(hello) = self.hello.as_ref() else {
            // Повторный опрос завершённого future.
            return Poll::Ready(Err(ShmError::NotReady));
        };
        let fired = self.waits.harvest(waker);
        // Как `Hello::wait_ack` (C10): подтверждение -- `S2C_CONNECT` при
        // фиксации своей заявки (`accepted_on_signal`); иначе сигнал
        // устаревший, поглощён.
        if fired[OP] && hello.accepted() {
            return match self.hello.take() {
                Some(hello) => Poll::Ready(Ok(hello.finish())),
                None => Poll::Ready(Err(ShmError::NotReady)),
            };
        }
        if fired[PEER] {
            // Сервер умер, не ответив: откат, как у синхронного таймаута
            // (CAS `CLIENT_HELLO -> IDLE`). Отзыв проиграл -- сервер успел
            // зафиксировать рукопожатие: подключены, смерть увидит приём.
            return Poll::Ready(match self.hello.take() {
                Some(hello) if hello.withdraw() => Ok(hello.finish()),
                _ => Err(ShmError::PeerDied),
            });
        }
        let registered = self
            .waits
            .ensure_handle(OP, hello.events.connect_ack.as_handle(), waker)
            .and_then(|()| match self.server.as_ref() {
                Some(server) => self.waits.ensure_handle(PEER, server.as_handle(), waker),
                None => Ok(()),
            });
        match registered {
            Ok(()) => Poll::Pending,
            Err(err) => {
                // Ждать нельзя -- заявку отзываем, как при ошибке ожидания.
                Poll::Ready(match self.hello.take() {
                    Some(hello) if hello.withdraw() => Ok(hello.finish()),
                    _ => Err(err),
                })
            }
        }
    }
}

impl Future for ConnectFuture {
    type Output = Result<SharedClient>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().step(cx.waker())
    }
}

impl Drop for ConnectFuture {
    fn drop(&mut self) {
        // `S2C_CONNECT` не возвращаем: заявка отзывается, а следующий клиент
        // всё равно сбрасывает событие до своей заявки.
        self.waits.clear(None, None);
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

    fn channel_events(&self) -> Option<&SharedEvents> {
        Some(&self.events)
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
    /// Сессия всё ещё наша: `generation` не сменился (ревизия 2). Сменился
    /// -- сервер уже провёл рукопожатие с другим клиентом этого канала/слота
    /// (а наш `DISCONNECT` мог быть сброшен его `complete_handshake`).
    /// Тесты: токен claim-а этого клиента (`CLAIM_FREE` -- без claim).
    #[cfg(test)]
    pub(crate) const fn claim(&self) -> u32 {
        self.claim
    }

    pub(crate) fn is_session_current(&self) -> bool {
        self.view.control_block().generation.load(Ordering::Acquire) == self.generation
    }

    /// Сервер закрыл нашу сессию: `server_state` уже не `SERVER_READY` или
    /// `generation` сменился. Отличает свежий `DISCONNECT` от устаревшего
    /// (0.10: ожидание ответа лобби Dispatch).
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

    const fn ensure_connected(&self) -> Result<()> {
        if !self.connected {
            Err(ShmError::NotConnected)
        } else {
            Ok(())
        }
    }

    pub fn send_to_server(&self, payload: &[u8]) -> Result<WriteOutcome> {
        self.ensure_connected()?;
        let result = self.ring_tx.write_message(payload)?;
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
        self.ensure_connected()?;
        let result = self.ring_tx.try_write_message(payload)?;
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
        self.ensure_connected()?;
        // SAFETY: контракт `write_with` -- единственный писатель В ПРЕДЕЛАХ
        // этой конечной точки: `&mut self` исключает вложенную запись из `f`
        // и запись из другого потока через этот же клиент (тип ещё и
        // `!Sync`). Второй, устаревший `SharedClient` того же сегмента в
        // этом процессе -- уже не эта точка, а пир, нарушающий протокол
        // (одна сторона на направление); это случай враждебного пира,
        // принятый для неопубликованной области (Ruling 17): страдает только
        // сообщение, которое получит сервер.
        let result = unsafe { self.ring_tx.write_with(len, f) }?;
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
    /// SERVER_READY` -- `Err(NotConnected)`.
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
        self.ensure_connected()?;
        let control = self.view.control_block();
        let peer_left = || control.server_state.load(Ordering::Acquire) != HANDSHAKE_SERVER_READY;
        let disconnect = DisconnectWatch {
            event: &self.events.disconnect,
            peer_left: &peer_left,
        };
        self.ring_tx.wait_for_space(
            payload_len,
            Some(&self.events.c2s.space),
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
        let len = self.ring_rx.read_message(buffer)?;
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
        let result = unsafe { self.ring_rx.read_with(f) };
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
    pub fn poll_server(&self, timeout: Option<Duration>) -> Result<bool> {
        self.poll_server_spin(timeout, SpinPolicy::NONE)
    }

    /// `poll_server` со спином перед ожиданием в ядре (0.10; семантика --
    /// `SharedServer::poll_client_spin`).
    pub fn poll_server_spin(&self, timeout: Option<Duration>, spin: SpinPolicy) -> Result<bool> {
        self.ensure_connected()?;
        self.ring_rx
            .wait_for_data(&self.events.s2c.data, self.peer.as_ref(), timeout, spin)
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

impl Drop for SharedClient {
    /// Уход L (ревизия 3, INTEROP R15): записи состояния -- только пока
    /// сессия наша (`generation == G`), освобождение claim -- последним.
    ///
    /// R14: сначала своё состояние -> `IDLE` (CAS `SERVER_READY -> IDLE`,
    /// не затирает `CLIENT_HELLO` следующего клиента), потом `DISCONNECT`.
    /// `client_pid` не трогаем: наш PID сервер уже забрал (S6), а следующий
    /// клиент мог записать туда свой. `generation` сменился -- сессия уже не
    /// наша (сервер отключил нас, claim отнят, канал у следующего клиента):
    /// ни состояния, ни заголовков, ни `DISCONNECT`. Пока claim держим,
    /// новой сессии нет (I3), поэтому сверка и записи не разделены чужим
    /// рукопожатием. Отключён (`mark_disconnected`: сервер ушёл сам) --
    /// ответный `DISCONNECT` не сигналим, только освобождаем claim.
    fn drop(&mut self) {
        if self.connected && self.is_session_current() {
            let _ = self.view.control_block().client_state.compare_exchange(
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
            let _ = self.events.disconnect.set();
        }
        self.connected = false;
        if self.owns_claim {
            release_claim(&self.view, self.claim);
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
        assert!(!withdraw_hello(view, before));
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
        assert!(withdraw_hello(view, before), "рукопожатие состоялось");
        assert_eq!(
            control.client_state.load(Ordering::Acquire),
            HANDSHAKE_SERVER_READY
        );

        // Уходящий сервер снял заявку без рукопожатия (generation тот же).
        let before = control.generation.load(Ordering::Acquire);
        control
            .client_state
            .store(HANDSHAKE_IDLE, Ordering::Release);
        assert!(!withdraw_hello(view, before));
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
        let req = &server.events().unwrap().connect_req;
        let holder = Hello::post(None, &name, true, ClaimMode::Take).unwrap();
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
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
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
                .unwrap()
                .disconnect
                .wait(Some(Duration::ZERO))
                .unwrap(),
            "призрак взвёл DISCONNECT чужой сессии"
        );
        assert_eq!(control.slot_claim.load(Ordering::Acquire), next.claim());
        assert!(server.is_connected() && next.is_connected());
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
        let err = Hello::post(None, &name, true, ClaimMode::TakeOverAbandoned).unwrap_err();
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
        let hello = Hello::post(None, &name, true, ClaimMode::TakeOverAbandoned).unwrap();
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
        let holder = Hello::post(None, &name, true, ClaimMode::Take).unwrap();
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
                .unwrap()
                .disconnect
                .wait(Some(Duration::ZERO))
                .unwrap()
        );
        assert!(Endpoint::peer_left(&server));
    }
}
