use std::future::Future;
use std::marker::PhantomData;
use std::os::windows::io::{AsHandle, BorrowedHandle};
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use crate::asynch::{Endpoint, OP, RecvFuture, SendFuture, WaitSet};
use crate::constants::{HANDSHAKE_CLIENT_HELLO, HANDSHAKE_IDLE, HANDSHAKE_SERVER_READY};
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

/// Откат клиентской стороны handshake в IDLE (заявка не принята).
///
/// Вызывается на всех неуспешных путях `SharedClient::connect`: без него в
/// shared memory остался бы «висящий» `CLIENT_HELLO`, который сервер принял бы
/// за живую заявку на подключение. `client_state` здесь уже НЕ `CLIENT_HELLO`
/// (его снял CAS в `withdraw_hello`) -- трогаем только PID и заголовки.
fn rollback_handshake(view: &SharedView) {
    // Заявленные PID и время процесса отзываем вместе с HELLO: иначе клиент
    // старой версии, пришедший следом, унаследовал бы их (см.
    // `ControlBlock::client_pid`).
    view.control_block().client_pid.store(0, Ordering::Release);
    view.control_block().announce_client_start(0);
    let (header_a, header_b) = view.headers();
    header_a
        .handshake_state
        .store(HANDSHAKE_IDLE, Ordering::Release);
    header_b
        .handshake_state
        .store(HANDSHAKE_IDLE, Ordering::Release);
}

/// Отозвать заявку `CLIENT_HELLO` (таймаут, остановка, ошибка ожидания).
///
/// Ревизия 2 (25.09.2026): точка фиксации рукопожатия -- `client_state`.
/// Сервер фиксирует его CAS `CLIENT_HELLO -> SERVER_READY`
/// (`SharedServer::complete_handshake`), клиент отзывает CAS `CLIENT_HELLO ->
/// IDLE`; выигрывает ровно один. Раньше обе стороны писали store: клиент,
/// откатившийся в момент ответа сервера, оставлял сервер «подключённым» к
/// никому, а взведённый `S2C_CONNECT` -- следующему клиенту.
///
/// `true` -- отзывать поздно: `client_state` уже не `CLIENT_HELLO`, а
/// `generation` сменился -- сервер принял нас (и, возможно, уже отключил:
/// тогда `DISCONNECT` взведён, worker дочитает кольцо). `false` -- заявка
/// отозвана (или её снял уходящий сервер без рукопожатия), подключения нет.
fn withdraw_hello(view: &SharedView, generation_before: u32) -> bool {
    let control = view.control_block();
    let withdrawn = control
        .client_state
        .compare_exchange(
            HANDSHAKE_CLIENT_HELLO,
            HANDSHAKE_IDLE,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok();
    if !withdrawn && control.generation.load(Ordering::Acquire) != generation_before {
        return true;
    }
    rollback_handshake(view);
    false
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
enum AckWait {
    /// Сервер принял именно нашу заявку (`generation` сменился).
    Accepted,
    /// Таймаут, остановка или ошибка ожидания -- заявку отзывать.
    Abandoned(ShmError),
}

/// Ожидание `S2C_CONNECT` (и `wake` вызывающего, если задан) до `timeout`.
/// Сигнал без смены `generation` -- устаревший (сервер старой версии ответил
/// откатившемуся клиенту) -- поглощается, ожидание продолжается.
fn wait_ack(
    view: &SharedView,
    events: &SharedEvents,
    generation_before: u32,
    timeout: Duration,
    mut interrupt: Option<&mut Interrupt<'_>>,
) -> AckWait {
    let accepted = || view.control_block().generation.load(Ordering::Acquire) != generation_before;
    // Таймаут, непредставимый как `Instant` (`Duration::MAX`), -- ждать без
    // дедлайна, а не паниковать на переполнении.
    let deadline = crate::ring::deadline_of(Some(timeout));
    loop {
        let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
        if left.is_some_and(|left| left.is_zero()) {
            return AckWait::Abandoned(ShmError::Timeout);
        }
        let fired = match interrupt.as_deref_mut() {
            Some(int) => wait_any(&[events.connect_ack.as_handle(), int.wake], left),
            None => events
                .connect_ack
                .wait(left)
                .map(|signaled| signaled.then_some(0)),
        };
        match fired {
            Ok(Some(0)) => {
                if accepted() {
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
    pub fn connect(name: &str, timeout: Duration) -> Result<Self> {
        Self::connect_impl(None, name, timeout, true, None)
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
        Self::connect_impl(Some(ns), name, timeout, true, None)
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
        interrupt: Interrupt<'_>,
    ) -> Result<Self> {
        Self::connect_impl(namespace, name, timeout, true, Some(interrupt))
    }

    /// `announce_pid = false` -- поведение клиента 0.7/0.8.0 без обмена PID
    /// (нужно тестам совместимости версий).
    ///
    /// Порядок (ревизия 2, INTEROP §d.2 шаг 3a, R15): запомнить `generation`
    /// -> сбросить `S2C_CONNECT` -> время процесса (`client_start_*`) -> PID
    /// (`client_pid`) -> `client_state = CLIENT_HELLO` -> заголовки
    /// -> `C2S_CONNECT_REQ` -> ждать `S2C_CONNECT`. Рукопожатие состоялось,
    /// если `generation` сменился -- даже когда сервер уже успел отключить нас (`server_state == IDLE`):
    /// тогда `DISCONNECT` взведён, и worker дочитает кольцо (прощальное
    /// сообщение), а не потеряет его в откате.
    pub(crate) fn connect_impl(
        namespace: Option<&PrivateNamespace>,
        name: &str,
        timeout: Duration,
        announce_pid: bool,
        mut interrupt: Option<Interrupt<'_>>,
    ) -> Result<Self> {
        let hello = Hello::post(namespace, name, announce_pid)?;
        let waited = match hello.events.connect_req.set() {
            Ok(()) => wait_ack(
                &hello.view,
                &hello.events,
                hello.generation_before,
                timeout,
                interrupt.as_mut(),
            ),
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
    /// [`SharedClient::connect`], ревизия 2): future выставляет заявку
    /// (`CLIENT_HELLO` + `C2S_CONNECT_REQ`) при первом опросе и ждёт
    /// `S2C_CONNECT` в реакторе IOCP; подключение засчитывается по смене
    /// `generation`, устаревший сигнал поглощается.
    ///
    /// Таймаута нет -- он у исполнителя. Брошенный future отзывает заявку
    /// так же, как синхронный таймаут: CAS `CLIENT_HELLO -> IDLE` и откат
    /// PID/заголовков. Если сервер успел зафиксировать рукопожатие (его CAS
    /// выиграл), клиент уходит штатно, как `Drop` подключённого: `IDLE`,
    /// затем `DISCONNECT` -- сервер не остаётся «подключённым» к никому.
    /// Всё это -- атомарные операции и `NtSetEvent`, `Drop` не блокируется.
    ///
    /// Ошибки открытия -- как у `connect` (канала нет --
    /// `WindowsError { code: STATUS_OBJECT_NAME_NOT_FOUND }`).
    ///
    /// ```ignore
    /// let client = tokio::time::timeout(
    ///     Duration::from_secs(5),
    ///     xshm::SharedClient::connect_async("MyChannel"),
    /// )
    /// .await??;
    /// ```
    pub fn connect_async(name: &str) -> impl Future<Output = Result<Self>> + Send + 'static {
        ConnectFuture {
            name: name.to_owned(),
            hello: None,
            started: false,
            waits: WaitSet::default(),
        }
    }

    /// Принять сообщение сервера асинхронно (0.10; семантика --
    /// `SharedServer::recv_async`): отключение сервера -- `Err(NotConnected)`,
    /// смерть его процесса -- `Err(PeerDied)`, обе -- после дочитанного
    /// кольца, затем клиент отмечен неподключённым.
    pub fn recv_async<'a>(
        &'a mut self,
        buf: &'a mut Vec<u8>,
    ) -> impl Future<Output = Result<usize>> + Send + 'a {
        RecvFuture::new(self, buf)
    }

    /// Отправить сообщение серверу асинхронно без перезаписи (0.10;
    /// семантика -- `SharedServer::send_async`).
    pub fn send_async<'a>(
        &'a mut self,
        data: &'a [u8],
    ) -> impl Future<Output = Result<()>> + Send + 'a {
        SendFuture::new(self, data)
    }
}

/// Выставленная заявка клиента (`CLIENT_HELLO`), ответа сервера ещё нет:
/// общая часть синхронного и асинхронного подключения.
struct Hello {
    view: SharedView,
    events: SharedEvents,
    /// `generation` до заявки: по его смене узнаём, что сервер принял нас.
    generation_before: u32,
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
            .finish_non_exhaustive()
    }
}

impl Hello {
    /// Открыть канал и выставить заявку (без сигнала `CONNECT_REQ`).
    fn post(namespace: Option<&PrivateNamespace>, name: &str, announce_pid: bool) -> Result<Self> {
        let mapping = Mapping::open(ObjectPath::mapping(namespace, name))?;
        // SAFETY: отображение длиной `mapping.size()` живёт, пока жив
        // `mapping`, а он переезжает в возвращаемую структуру вместе с view;
        // адрес view выровнен на гранулу выделения.
        let view = unsafe { SharedView::attach(mapping.as_ptr(), mapping.size()) }?;
        let control = view.control_block();

        let events = SharedEvents::open_in(namespace, name)?;

        // Ревизия 2: `generation` ДО заявки -- по его смене узнаём, что
        // сервер принял именно нас (новый generation он публикует в
        // `complete_handshake` раньше, чем фиксирует рукопожатие CAS-ом).
        let generation_before = control.generation.load(Ordering::Acquire);
        // Ревизия 2: `S2C_CONNECT`, оставшийся от чужого рукопожатия (сервер
        // старой версии ответил клиенту, который уже откатился), иначе
        // «подтвердил» бы нашу заявку раньше сервера. Сбрасываем ДО
        // `CLIENT_HELLO`: ответ на нашу заявку может прийти только после.
        let _ = events.connect_ack.reset();

        // Протокол 0.8+: PID клиента -- ДО CLIENT_HELLO, чтобы Release-публикация
        // client_state сделала его видимым серверу вместе с заявкой.
        // Находка 23: время создания процесса -- ДО `client_pid`, чью
        // Release-запись сервер забирает AcqRel-`swap`-ом вместе с половинами.
        if announce_pid {
            control.announce_client_start(announced_start_time());
            control
                .client_pid
                .store(std::process::id(), Ordering::Release);
        }

        control
            .client_state
            .store(HANDSHAKE_CLIENT_HELLO, Ordering::Release);

        let (header_a, header_b) = view.headers();
        header_a
            .handshake_state
            .store(HANDSHAKE_CLIENT_HELLO, Ordering::Release);
        header_b
            .handshake_state
            .store(HANDSHAKE_CLIENT_HELLO, Ordering::Release);

        Ok(Self {
            view,
            events,
            generation_before,
            mapping,
        })
    }

    /// Сервер принял именно нашу заявку: `generation` сменился.
    fn accepted(&self) -> bool {
        self.view.control_block().generation.load(Ordering::Acquire) != self.generation_before
    }

    /// Отозвать заявку (`withdraw_hello`): `true` -- поздно, рукопожатие
    /// состоялось (продолжать как подключённые).
    fn withdraw(&self) -> bool {
        withdraw_hello(&self.view, self.generation_before)
    }

    /// Рукопожатие состоялось: подключённый клиент.
    fn finish(self) -> SharedClient {
        let Self {
            view,
            events,
            generation_before: _,
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
    waits: WaitSet,
}

impl ConnectFuture {
    fn step(&mut self, waker: &Waker) -> Poll<Result<SharedClient>> {
        if !self.started {
            self.started = true;
            let hello = match Hello::post(None, &self.name, true) {
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
            self.hello = Some(hello);
        }
        let Some(hello) = self.hello.as_ref() else {
            // Повторный опрос завершённого future.
            return Poll::Ready(Err(ShmError::NotReady));
        };
        // Как `wait_ack`: подтверждение -- только `S2C_CONNECT` со сменой
        // `generation`; сигнал без смены -- устаревший, поглощён.
        if self.waits.harvest(waker)[OP] && hello.accepted() {
            return match self.hello.take() {
                Some(hello) => Poll::Ready(Ok(hello.finish())),
                None => Poll::Ready(Err(ShmError::NotReady)),
            };
        }
        match self
            .waits
            .ensure_handle(OP, hello.events.connect_ack.as_handle(), waker)
        {
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
    fn drop(&mut self) {
        if self.connected {
            // R14: сначала своё состояние -> IDLE, потом `DISCONNECT`.
            // Ревизия 2: CAS `SERVER_READY -> IDLE`, а не store -- не затирает
            // `CLIENT_HELLO` следующего клиента. `client_pid` не трогаем:
            // наш PID сервер уже забрал (`swap` в рукопожатии), а следующий
            // клиент мог записать туда свой.
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
            self.connected = false;
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
}
