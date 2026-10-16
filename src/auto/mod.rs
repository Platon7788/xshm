use std::collections::VecDeque;
use std::os::windows::io::{AsHandle, BorrowedHandle};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::client::{Interrupt, SharedClient};
use crate::constants::{MAX_RING_CAPACITY, MIN_MESSAGE_SIZE};
use crate::error::{DisconnectReason, Result, ShmError};
use crate::layout::{ChannelLayout, max_payload_for};
use crate::ring::{FreeSpace, WriteOutcome, frame_size};
use crate::server::SharedServer;
use crate::sync_ext::{LockExt, join_worker};
use crate::wait_delay_or;
use crate::win::{self, EventHandle};

pub(crate) fn map_spawn_error(err: std::io::Error, context: &'static str) -> ShmError {
    // Код Win32 -- беззнаковый по смыслу (`GetLastError`): битовый каст.
    let code = err.raw_os_error().map_or(u32::MAX, i32::cast_unsigned);
    ShmError::WindowsError { code, context }
}

/// Джойнит worker-поток, если это безопасно; при self-join -- отпускает
/// `JoinHandle` без блокировки.
///
/// Если `stop()`/`Drop` вызывается СИНХРОННО из собственного worker-потока
/// (пользовательский `AutoHandler` останавливает или дропает сервер/клиент
/// прямо внутри своего же callback'а -- `on_disconnect`, `on_message` и т.п.
/// вызываются ИМЕННО на worker-потоке), безусловный `handle.join()` был бы
/// self-join deadlock: поток ждал бы сам себя навсегда (аудит 2026-07-10).
///
/// При обнаружении self-join `JoinHandle` просто дропается без join: поток
/// уже видит `running=false` (устанавливается до этого вызова) и завершится
/// сам, как только колбэк вернёт управление.
///
/// `Err(WorkerPanicked)` -- worker завершился паникой (см. `join_worker`).
pub(crate) fn join_unless_self(handle: JoinHandle<()>) -> Result<()> {
    if handle.thread().id() == thread::current().id() {
        return Ok(());
    }
    join_worker(handle)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelKind {
    ServerToClient,
    ClientToServer,
}

/// Callback-интерфейс `AutoServer`/`AutoClient`.
///
/// Все методы вызываются СИНХРОННО из собственного worker-потока
/// `AutoServer`/`AutoClient`. После возврата `stop()` (или `Drop`) из
/// другого потока ни один метод больше не вызывается. Если реализация
/// останавливает/дропает тот же объект изнутри своего колбэка, ожидания
/// самого себя нет: worker завершится сразу после возврата из колбэка.
pub trait AutoHandler: Send + Sync + 'static {
    fn on_connect(&self) {}
    fn on_disconnect(&self) {}
    /// Отключение с причиной (0.8+). По умолчанию вызывает `on_disconnect()` --
    /// существующие реализации работают как раньше; переопределите, чтобы
    /// отличать штатное отключение от смерти процесса пира
    /// (`DisconnectReason::PeerDied`). Worker вызывает ТОЛЬКО этот метод.
    fn on_disconnect_reason(&self, reason: DisconnectReason) {
        let _ = reason;
        self.on_disconnect();
    }
    fn on_message(&self, _direction: ChannelKind, _payload: &[u8]) {}
    /// Потеряны сообщения направления `direction`: `count` вытеснено из
    /// кольца перезаписью или из очереди отправки при её переполнении
    /// (`max_send_queue`). Вызывается раз на пачку, а не на каждое
    /// сообщение.
    fn on_overflow(&self, _direction: ChannelKind, _count: u32) {}
    /// Читатель освободил место в исходящем кольце (событие SPACE: кольцо
    /// опустело или места хватает ждущему lossless-сообщению). К моменту
    /// вызова worker уже дописал в кольцо из своей очереди всё, что влезло,
    /// — `try_send`, получивший `QueueFull`, стоит повторить.
    fn on_space_available(&self, _direction: ChannelKind) {}
    fn on_error(&self, _err: ShmError) {}
}

#[derive(Clone, Debug)]
pub struct AutoOptions {
    /// Страховочный таймаут ожидания worker-а. `None` (по умолчанию, 0.9+)
    /// -- только события: worker спит в ядре, пока не придут данные, место,
    /// отключение, смерть пира, новая команда (`send`/`try_send`/`stop`
    /// будят его своим событием) -- ни одного пробуждения в простое.
    /// `Some(t)` -- дополнительно просыпаться не реже `t` (не нужно для
    /// корректности; оставлено для диагностики).
    pub poll_timeout: Option<Duration>,
    /// Пауза между попытками переподключения `AutoClient` (`AutoServer` не
    /// использует).
    pub reconnect_delay: Duration,
    /// Сколько `AutoClient` ждёт ответа сервера на одну попытку
    /// подключения (`AutoServer` не использует).
    pub connect_timeout: Duration,
    /// Лимит очереди отправки (сообщения, принятые `send`/`try_send`, но
    /// ещё не записанные в кольцо); `0` трактуется как `1`. Переполнение:
    /// `send` вытесняет самое старое обычное сообщение (`on_overflow`),
    /// `try_send` получает `QueueFull`.
    pub max_send_queue: usize,
    /// Сколько сообщений worker доставляет за один проход до проверки
    /// остальных событий.
    pub recv_batch: usize,
    /// Раскладка канала (размер колец, лимиты сообщений по каждому
    /// направлению). Использует только `AutoServer`: клиент получает её из
    /// сегмента (`AutoClient::layout`).
    pub layout: ChannelLayout,
}

impl Default for AutoOptions {
    fn default() -> Self {
        Self {
            poll_timeout: None,
            reconnect_delay: Duration::from_millis(250),
            connect_timeout: Duration::from_secs(2),
            max_send_queue: 256,
            recv_batch: 32,
            layout: ChannelLayout::DEFAULT,
        }
    }
}

#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct AutoStatsSnapshot {
    pub sent_messages: u64,
    pub send_overflows: u64,
    pub received_messages: u64,
    pub receive_overflows: u64,
}

#[derive(Default, Debug)]
struct AutoStats {
    sent_messages: AtomicU64,
    send_overflows: AtomicU64,
    received_messages: AtomicU64,
    receive_overflows: AtomicU64,
    /// Возвраты worker-а из ожидания в ядре (0.9) -- тесты доказывают по
    /// нему отсутствие пробуждений в простое. В снимок не входит.
    wakeups: AtomicU64,
}

impl AutoStats {
    // Relaxed во всех счётчиках: статистика, ничего не публикует.
    fn wake(&self) {
        self.wakeups.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> AutoStatsSnapshot {
        AutoStatsSnapshot {
            sent_messages: self.sent_messages.load(Ordering::Relaxed),
            send_overflows: self.send_overflows.load(Ordering::Relaxed),
            received_messages: self.received_messages.load(Ordering::Relaxed),
            receive_overflows: self.receive_overflows.load(Ordering::Relaxed),
        }
    }
}

/// Исходящее сообщение в очереди.
#[derive(Debug)]
struct Outgoing {
    data: Vec<u8>,
    /// `true` -- пришло через `try_send`: пишется в кольцо только
    /// `try_write_message` (без перезаписи) и никогда не вытесняется из
    /// очереди; `false` -- обычный `send` с политикой overwrite-oldest.
    lossless: bool,
}

impl Outgoing {
    /// Байты, которые сообщение займёт в кольце.
    const fn frame(&self) -> usize {
        frame_size(self.data.len())
    }
}

type SendQueue = VecDeque<Outgoing>;

/// Очередь отправки, общая для API-потоков и worker-а.
///
/// Отправители кладут в хвост под коротким lock-ом и сами соблюдают
/// `max_send_queue` (вытесняя старейшее обычное сообщение) -- память
/// ограничена даже когда worker надолго занят колбэком. Worker снимает
/// голову, пишет в кольцо без lock-а и возвращает её обратно, если кольцо
/// полно (порядок FIFO не нарушается: голову снимает только он).
#[derive(Debug, Default)]
struct Outbox {
    queue: Mutex<SendQueue>,
    /// Вытеснено отправителями при переполнении очереди и ещё не сообщено:
    /// `on_overflow` зовёт worker (колбэки -- только на его потоке).
    evicted: AtomicU32,
}

impl Outbox {
    fn lock(&self) -> std::sync::MutexGuard<'_, SendQueue> {
        // Паника внутри lock-а невозможна (только операции VecDeque), но
        // отравленный mutex не должен валить поток отправителя.
        self.queue.lock_clean()
    }

    fn pop_front(&self) -> Option<Outgoing> {
        self.lock().pop_front()
    }

    fn push_front(&self, msg: Outgoing) {
        self.lock().push_front(msg);
    }
}

/// Счётчики исходящего направления, общие для API-потоков и worker-а.
///
/// `pending_*` -- принятые `send`/`try_send`, но ещё не записанные в кольцо
/// (в `Outbox` или в руках worker-а); `ring_free_*` -- последний снимок
/// `free_space()` кольца, снятый worker-ом (он же единственный писатель
/// кольца, поэтому снимок -- нижняя граница до его следующей записи).
///
/// Порядок, на котором держится консервативность `estimate()`: worker
/// сначала публикует новый (меньший) снимок кольца, и только потом
/// (`Release`) уменьшает `pending_*`; `estimate()` читает `pending_*`
/// (`Acquire`) ДО снимка. Увидел уменьшенный `pending` -- увидит и
/// уменьшенный снимок; не увидел -- вычтет ещё не списанное сообщение из
/// снимка, в котором его ещё нет. В обоих случаях оценка не завышена.
///
/// `peer_*` -- состояние наблюдения за процессом пира, которое публикует
/// worker (он единственный держит `ProcessWatch`): PID и статус
/// `PEER_UNKNOWN`/`PEER_ALIVE`/`PEER_DEAD`.
///
/// `tx_max_message` -- лимит размера сообщения исходящего кольца (`0` --
/// ещё неизвестен: клиент до первого подключения); `layout` -- раскладка
/// текущего (у клиента -- последнего) подключения.
#[derive(Debug, Default)]
struct ChannelState {
    pending_msgs: AtomicUsize,
    pending_bytes: AtomicUsize,
    ring_free_bytes: AtomicUsize,
    ring_free_msgs: AtomicU32,
    peer_pid: AtomicU32,
    peer_status: AtomicU8,
    /// Сервер: PID клиента, сверенный по ненулевому времени создания
    /// (`0` -- не сверен); только для сверки PID канала Dispatch.
    peer_verified_pid: AtomicU32,
    tx_max_message: AtomicUsize,
    layout: Mutex<Option<ChannelLayout>>,
}

const PEER_UNKNOWN: u8 = 0;
const PEER_ALIVE: u8 = 1;
const PEER_DEAD: u8 = 2;

impl ChannelState {
    /// Раскладка канала известна (сервер -- при старте, клиент -- при
    /// каждом подключении); `tx` -- исходящее направление этой стороны.
    fn set_layout(&self, layout: ChannelLayout, tx: ChannelKind) {
        let ring = match tx {
            ChannelKind::ServerToClient => layout.server_to_client,
            ChannelKind::ClientToServer => layout.client_to_server,
        };
        self.tx_max_message
            .store(ring.max_message_size, Ordering::Release);
        *self.layout.lock_clean() = Some(layout);
    }

    fn layout(&self) -> Option<ChannelLayout> {
        *self.layout.lock_clean()
    }

    /// Проверка длины сообщения до постановки в очередь: против лимита
    /// исходящего кольца, если он уже известен, иначе -- против
    /// абсолютного максимума формата.
    fn check_len(&self, len: usize) -> Result<()> {
        if len < MIN_MESSAGE_SIZE {
            return Err(ShmError::MessageTooSmall);
        }
        let limit = match self.tx_max_message.load(Ordering::Acquire) {
            0 => max_payload_for(MAX_RING_CAPACITY),
            limit => limit,
        };
        if len > limit {
            return Err(ShmError::MessageTooLarge);
        }
        Ok(())
    }

    /// Worker: соединение поднято; `pid` -- наблюдаемый пир (если есть).
    fn peer_connected(&self, pid: Option<u32>) {
        self.peer_pid.store(pid.unwrap_or(0), Ordering::Release);
        let status = if pid.is_some() {
            PEER_ALIVE
        } else {
            PEER_UNKNOWN
        };
        self.peer_status.store(status, Ordering::Release);
    }

    /// Worker: соединение разорвано. После смерти пира PID сохраняется
    /// (для диагностики), статус -- `PEER_DEAD` до следующего подключения.
    fn peer_disconnected(&self, reason: DisconnectReason) {
        self.peer_verified_pid.store(0, Ordering::Release);
        if reason == DisconnectReason::PeerDied {
            self.peer_status.store(PEER_DEAD, Ordering::Release);
        } else {
            self.peer_status.store(PEER_UNKNOWN, Ordering::Release);
            self.peer_pid.store(0, Ordering::Release);
        }
    }

    fn peer_alive(&self) -> Option<bool> {
        match self.peer_status.load(Ordering::Acquire) {
            PEER_ALIVE => Some(true),
            PEER_DEAD => Some(false),
            _ => None,
        }
    }

    fn peer_pid(&self) -> Option<u32> {
        Some(self.peer_pid.load(Ordering::Acquire)).filter(|&pid| pid != 0)
    }

    /// Принять сообщение в учёт.
    fn acquire(&self, frame: usize) {
        self.pending_bytes.fetch_add(frame, Ordering::AcqRel);
        self.pending_msgs.fetch_add(1, Ordering::AcqRel);
    }

    /// То же, но только если в очереди есть место (`try_send`): CAS-цикл,
    /// чтобы несколько отправителей не превысили `max_send_queue`.
    fn try_acquire(&self, frame: usize, max_send_queue: usize) -> bool {
        let admitted = self
            .pending_msgs
            .try_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                (cur < max_send_queue).then_some(cur + 1)
            })
            .is_ok();
        if admitted {
            self.pending_bytes.fetch_add(frame, Ordering::AcqRel);
        }
        admitted
    }

    /// Списать сообщение из учёта (записано в кольцо или выброшено).
    fn release(&self, frame: usize) {
        self.pending_bytes.fetch_sub(frame, Ordering::AcqRel);
        self.pending_msgs.fetch_sub(1, Ordering::AcqRel);
    }

    /// Опубликовать снимок свободного места кольца (только worker).
    fn publish_ring(&self, space: FreeSpace) {
        self.ring_free_bytes.store(space.bytes, Ordering::Release);
        self.ring_free_msgs.store(space.messages, Ordering::Release);
    }

    /// Консервативная оценка места для НОВОГО сообщения: кольцо минус всё,
    /// что уже принято, но ещё не записано; слоты ограничены и кольцом, и
    /// свободным местом очереди `max_send_queue`.
    fn estimate(&self, max_send_queue: usize) -> FreeSpace {
        let pending_msgs = self.pending_msgs.load(Ordering::Acquire);
        let pending_bytes = self.pending_bytes.load(Ordering::Acquire);
        let ring_msgs = self.ring_free_msgs.load(Ordering::Acquire) as usize;
        let ring_bytes = self.ring_free_bytes.load(Ordering::Acquire);
        let queue_room = max_send_queue.saturating_sub(pending_msgs);
        let ring_room = ring_msgs.saturating_sub(pending_msgs);
        let bytes = ring_bytes.saturating_sub(pending_bytes);
        let messages = queue_room.min(ring_room) as u32;
        if bytes == 0 || messages == 0 {
            return FreeSpace::ZERO;
        }
        FreeSpace {
            bytes,
            messages,
            max_message_size: self.tx_max_message.load(Ordering::Acquire),
        }
    }
}

/// Постановка сообщения в очередь (общая для `AutoServer`/`AutoClient`);
/// принятое сообщение будит worker его событием `wake`.
fn enqueue(
    outbox: &Outbox,
    wake: &EventHandle,
    running: &AtomicBool,
    gauge: &ChannelState,
    data: &[u8],
    lossless: bool,
    max_send_queue: usize,
) -> Result<()> {
    if !running.load(Ordering::Acquire) {
        return Err(ShmError::NotReady);
    }
    // Длину проверяем синхронно: иначе ошибка ушла бы в on_error
    // асинхронно, а вызывающий считал бы сообщение принятым.
    gauge.check_len(data.len())?;
    let cap = max_send_queue.max(1);
    let msg = Outgoing {
        data: data.to_vec(),
        lossless,
    };
    let frame = msg.frame();
    {
        let mut queue = outbox.lock();
        if lossless {
            if !gauge.try_acquire(frame, cap) {
                return Err(ShmError::QueueFull);
            }
        } else if queue.len() >= cap {
            // Переполнение: вытесняем самое старое ОБЫЧНОЕ сообщение; если
            // в очереди одни lossless -- выбрасывается само новое.
            let Some(pos) = queue.iter().position(|m| !m.lossless) else {
                outbox.evicted.fetch_add(1, Ordering::AcqRel);
                drop(queue);
                let _ = wake.set();
                return Ok(());
            };
            if let Some(evicted) = queue.remove(pos) {
                gauge.release(evicted.frame());
            }
            outbox.evicted.fetch_add(1, Ordering::AcqRel);
            gauge.acquire(frame);
        } else {
            gauge.acquire(frame);
        }
        queue.push_back(msg);
    }
    // Ошибка сигнала -- только испорченный дескриптор; сообщение всё равно
    // уйдёт при следующем пробуждении worker-а.
    let _ = wake.set();
    Ok(())
}

/// Общая часть `AutoServer`/`AutoClient` на стороне API.
#[derive(Debug)]
struct Endpoint {
    outbox: Arc<Outbox>,
    /// Будит worker: новая команда (`send`/`try_send`/`stop`/Drop).
    wake: Arc<EventHandle>,
    join: Mutex<Option<JoinHandle<()>>>,
    stats: Arc<AutoStats>,
    running: Arc<AtomicBool>,
    gauge: Arc<ChannelState>,
    max_send_queue: usize,
}

/// Всё, что нужно worker-у (общее с `Endpoint`).
struct Worker {
    handler: Arc<dyn AutoHandler>,
    options: AutoOptions,
    outbox: Arc<Outbox>,
    wake: Arc<EventHandle>,
    stats: Arc<AutoStats>,
    running: Arc<AtomicBool>,
    gauge: Arc<ChannelState>,
    /// Исходящее направление этой стороны.
    tx: ChannelKind,
}

impl Endpoint {
    /// Поднять worker-поток: `body` получает контекст и работает, пока
    /// `running`.
    fn spawn(
        handler: Arc<dyn AutoHandler>,
        options: AutoOptions,
        tx: ChannelKind,
        thread_name: impl FnOnce() -> String,
        layout: Option<ChannelLayout>,
        body: impl FnOnce(&Worker) + Send + 'static,
    ) -> Result<Self> {
        let wake = EventHandle::create_unnamed(false).map(Arc::new)?;
        let endpoint = Self {
            outbox: Arc::default(),
            wake,
            join: Mutex::new(None),
            stats: Arc::default(),
            running: Arc::new(AtomicBool::new(true)),
            gauge: Arc::default(),
            max_send_queue: options.max_send_queue,
        };
        if let Some(layout) = layout {
            endpoint.gauge.set_layout(layout, tx);
        }
        let worker = Worker {
            handler,
            options,
            outbox: Arc::clone(&endpoint.outbox),
            wake: Arc::clone(&endpoint.wake),
            stats: Arc::clone(&endpoint.stats),
            running: Arc::clone(&endpoint.running),
            gauge: Arc::clone(&endpoint.gauge),
            tx,
        };
        // Имя потока -- только в debug (короткий непрозрачный тег), в release
        // поток безымянный: имя канала не светится в Process Explorer.
        #[cfg_attr(
            not(debug_assertions),
            expect(unused_mut, reason = "имя -- только в debug")
        )]
        let mut builder = thread::Builder::new();
        #[cfg(debug_assertions)]
        {
            builder = builder.name(thread_name());
        }
        #[cfg(not(debug_assertions))]
        let _ = thread_name;
        let join = crate::thread_hook::spawn(builder, move || body(&worker))
            .map_err(|err| map_spawn_error(err, "spawn auto worker"))?;
        *endpoint.join.lock_clean() = Some(join);
        Ok(endpoint)
    }

    fn send(&self, data: &[u8], lossless: bool) -> Result<()> {
        enqueue(
            &self.outbox,
            &self.wake,
            &self.running,
            &self.gauge,
            data,
            lossless,
            self.max_send_queue,
        )
    }

    fn free_space(&self) -> FreeSpace {
        self.gauge.estimate(self.max_send_queue.max(1))
    }

    /// Синхронная остановка: worker дописывает очередь в кольцо (сколько
    /// влезет) и выходит, поток джойнится -- после возврата колбэков нет.
    /// Из собственного колбэка -- без ожидания себя (`join_unless_self`).
    fn stop(&self) {
        // Паника worker-а здесь сообщать некуда: обработчик живёт только в
        // контексте worker-а (`Worker::handler`), а держать его ещё и
        // здесь -- риск цикла `Arc` (прокси Dispatch ссылаются на свои
        // каналы). Текст паники уже напечатал panic hook потребителя;
        // паниковать из `stop()`/`Drop` нельзя. Кому итог нужен (Dispatch,
        // у которого есть свой обработчик), зовёт `stop_joined`.
        let _panicked = self.stop_joined();
    }

    /// `stop`, возвращающий итог join-а: `Err(WorkerPanicked)` -- worker
    /// завершился паникой (колбэк обработчика). Повторный вызов -- `Ok`.
    fn stop_joined(&self) -> Result<()> {
        self.running.store(false, Ordering::Release);
        let _ = self.wake.set();
        let handle = self.join.lock_clean().take();
        handle.map_or(Ok(()), join_unless_self)
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Worker {
    fn notify_disconnect(&self, reason: DisconnectReason) {
        // Сначала статус пира (его видят `is_peer_alive()`), затем колбэк.
        self.gauge.peer_disconnected(reason);
        self.handler.on_disconnect_reason(reason);
    }

    fn running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// Сообщить о сообщениях, вытесненных отправителями из очереди.
    fn report_evicted(&self) {
        let evicted = self.outbox.evicted.swap(0, Ordering::AcqRel);
        if evicted > 0 {
            self.handler.on_overflow(self.tx, evicted);
        }
    }

    /// Пишет очередь в кольцо, пока пишется (см. `process_send_queue`).
    /// `false` -- кольцо испорчено пиром, соединение надо разорвать.
    fn flush<E: SendEndpoint>(&self, endpoint: &E) -> bool {
        self.report_evicted();
        process_send_queue(
            endpoint,
            &self.outbox,
            &self.handler,
            &self.stats,
            &self.gauge,
            self.tx,
        )
    }

    /// Доставить до `recv_batch` сообщений.
    fn receive<R: ReceiveEndpoint>(&self, endpoint: &R, buffer: &mut Vec<u8>) -> ReceiveOutcome {
        process_receive_queue(
            endpoint,
            &self.handler,
            &self.stats,
            buffer,
            self.options.recv_batch,
            self.tx.reverse(),
        )
    }

    /// Пир отключился или умер -- новых данных не будет, но всё, что он
    /// успел записать до этого, доставляем ДО `on_disconnect_reason`.
    fn drain_remaining<R: ReceiveEndpoint>(&self, endpoint: &R, buffer: &mut Vec<u8>) {
        loop {
            let outcome = self.receive(endpoint, buffer);
            if outcome.fatal || !outcome.more_pending || !self.running() {
                break;
            }
        }
    }

    /// Обработать `SPACE`: сначала дописать очередь в освободившееся место,
    /// потом сообщить -- `try_send` после `on_space_available` находит место
    /// и в очереди.
    fn on_space<E: SendEndpoint>(&self, endpoint: &E) -> bool {
        let ok = self.flush(endpoint);
        if ok {
            self.handler.on_space_available(self.tx);
        }
        ok
    }

    /// Обработать результат ожидания подключённого worker-а. `None` --
    /// соединение живо, `Some(reason)` -- разорвано (уже доставлено всё,
    /// что пир успел записать; колбэк отключения -- за вызывающим).
    fn handle_wait<E: SendEndpoint + ReceiveEndpoint>(
        &self,
        endpoint: &E,
        set: &WaitSet<'_>,
        space: BorrowedHandle<'_>,
        result: Result<Option<usize>>,
        buffer: &mut Vec<u8>,
    ) -> Option<DisconnectReason> {
        match result {
            Ok(Some(DISCONNECT_INDEX)) => {
                // Штатное отключение: всё, что пир успел записать (например,
                // прощальное сообщение), доставляем ДО `on_disconnect`.
                self.drain_remaining(endpoint, buffer);
                Some(DisconnectReason::Graceful)
            }
            Ok(Some(DATA_INDEX)) => {
                // Данные прочитаем на следующем проходе. SPACE (автосброс,
                // больший индекс) мог прийти одновременно: при непрерывном
                // входящем потоке NT отдавал бы DATA каждый раз, и
                // `on_space_available` откладывался бы бесконечно.
                // Lock очереди -- отдельным выражением: `on_space` берёт его снова.
                let queued = !self.outbox.lock().is_empty();
                let space_too =
                    queued && matches!(win::wait_any(&[space], Some(Duration::ZERO)), Ok(Some(0)));
                if space_too && !self.on_space(endpoint) {
                    return Some(DisconnectReason::Error);
                }
                None
            }
            Ok(Some(SPACE_INDEX)) => (!self.on_space(endpoint)).then_some(DisconnectReason::Error),
            Ok(Some(i)) if Some(i) == set.peer => {
                self.drain_remaining(endpoint, buffer);
                Some(DisconnectReason::PeerDied)
            }
            // `wake` (новая команда / остановка) или страховочный таймаут.
            Ok(Some(_) | None) => None,
            Err(err) => {
                self.handler.on_error(err);
                Some(DisconnectReason::Error)
            }
        }
    }
}

impl ChannelKind {
    const fn reverse(self) -> Self {
        match self {
            Self::ServerToClient => Self::ClientToServer,
            Self::ClientToServer => Self::ServerToClient,
        }
    }
}

#[derive(Debug)]
pub struct AutoServer {
    endpoint: Endpoint,
    layout: ChannelLayout,
}

impl AutoServer {
    /// Поднять канал `name` (раскладка -- `options.layout`) и worker,
    /// который принимает клиентов и обслуживает их по событиям.
    pub fn start(name: &str, handler: Arc<dyn AutoHandler>, options: AutoOptions) -> Result<Self> {
        let mut server = SharedServer::start_with_layout(name, options.layout)?;
        let layout = *server.layout();
        let endpoint = Endpoint::spawn(
            handler,
            options,
            ChannelKind::ServerToClient,
            || format!("xsa-{name}"),
            Some(layout),
            move |worker| server_worker(&mut server, worker),
        )?;
        Ok(Self { endpoint, layout })
    }

    /// Раскладка канала.
    #[must_use]
    pub const fn layout(&self) -> &ChannelLayout {
        &self.layout
    }

    /// Асинхронная отправка с политикой overwrite-oldest: при переполнении
    /// очереди вытесняется самое старое обычное сообщение, при переполнении
    /// кольца -- самые старые непрочитанные (`on_overflow`).
    /// `Err(MessageTooSmall | MessageTooLarge)` -- длина вне
    /// `MIN_MESSAGE_SIZE..=layout.server_to_client.max_message_size`
    /// (проверяется синхронно); `Err(NotReady)` -- сервер остановлен.
    pub fn send(&self, data: &[u8]) -> Result<()> {
        self.endpoint.send(data, false)
    }

    /// Асинхронная отправка **без потерь** (backpressure).
    ///
    /// - `Ok(())` -- сообщение принято и будет записано в кольцо
    ///   `try_write_message`-ом, то есть без перезаписи чего-либо; из очереди
    ///   оно не вытесняется (ни `send`, ни `try_send`), порядок -- FIFO вместе
    ///   с остальными сообщениями канала;
    /// - `Err(QueueFull)` -- в очереди уже `max_send_queue` непереданных
    ///   сообщений; ничего не принято, повторите позже (`free_space`,
    ///   `on_space_available`);
    /// - `Err(MessageTooSmall | MessageTooLarge)` -- проверяется синхронно;
    /// - `Err(NotReady)` -- сервер остановлен.
    ///
    /// Гарантия «без потерь» действует в пределах одного подключения: при
    /// переподключении handshake сбрасывает кольца, и уже записанные, но не
    /// прочитанные сообщения пропадают (неотправленные остаются в очереди и
    /// уйдут новому клиенту). Смешивать с `send` можно, но сообщения `send`
    /// пишутся с перезаписью и могут вытеснить из КОЛЬЦА ранее записанные
    /// сообщения `try_send` -- для строго lossless-канала используйте только
    /// `try_send`.
    pub fn try_send(&self, data: &[u8]) -> Result<()> {
        self.endpoint.send(data, true)
    }

    /// Консервативная оценка места под новое сообщение: свободное место
    /// кольца Server -> Client (снимок worker-а) минус всё принятое, но ещё
    /// не записанное; `messages` дополнительно ограничено свободным местом
    /// очереди. Если `free_space().fits(n)`, то `try_send` payload-а длины
    /// `n` будет принят и записан в кольцо без ожидания читателя (при одном
    /// отправляющем потоке). Вне подключения -- `FreeSpace::ZERO`.
    #[must_use]
    pub fn free_space(&self) -> FreeSpace {
        self.endpoint.free_space()
    }

    /// Жив ли процесс пира по данным worker-а: `Some(true)` -- подключён и
    /// наблюдается через удерживаемый handle; `Some(false)` -- последнее
    /// отключение было `PeerDied` (до следующего подключения); `None` -- нет
    /// подключения или пир не наблюдается (нет прав).
    #[must_use]
    pub fn is_peer_alive(&self) -> Option<bool> {
        self.endpoint.gauge.peer_alive()
    }

    /// PID наблюдаемого пира (после `PeerDied` -- PID умершего процесса).
    #[must_use]
    pub fn peer_pid(&self) -> Option<u32> {
        self.endpoint.gauge.peer_pid()
    }

    /// PID текущего клиента, сверенный по ненулевому времени создания
    /// процесса (`SharedServer::verified_peer_pid`); публикуется worker-ом
    /// до `on_connect`. Сверка самоописания, не аутентификация.
    pub(crate) fn verified_peer_pid(&self) -> Option<u32> {
        Some(
            self.endpoint
                .gauge
                .peer_verified_pid
                .load(Ordering::Acquire),
        )
        .filter(|&p| p != 0)
    }

    /// Остановить сервер синхронно: очередь дописывается в кольцо (сколько
    /// влезет), клиент получает `DISCONNECT`, worker-поток джойнится -- после
    /// возврата колбэков нет. Повторный вызов и `Drop` после него -- no-op.
    pub fn stop(&self) {
        self.endpoint.stop();
    }

    /// `stop` с итогом join-а worker-а: `Err(WorkerPanicked)` -- worker
    /// завершился паникой (колбэк обработчика). Dispatch сообщает её в
    /// `on_error` своего обработчика с id клиента.
    pub(crate) fn stop_joined(&self) -> Result<()> {
        self.endpoint.stop_joined()
    }

    #[must_use]
    pub fn stats(&self) -> AutoStatsSnapshot {
        self.endpoint.stats.snapshot()
    }

    /// Число пробуждений worker-а (тесты: ноль в простое).
    #[cfg(test)]
    pub(crate) fn wakeups(&self) -> u64 {
        self.endpoint.stats.wakeups.load(Ordering::Relaxed)
    }
}

fn server_worker(server: &mut SharedServer, worker: &Worker) {
    let mut buffer = Vec::new();
    // Anonymous-сервер не имеет событий, а auto-mode построен на ожидании
    // событий. Недостижимо: AutoServer::start всегда поднимает named-сервер.
    if server.events().is_none() {
        worker.handler.on_error(ShmError::InvalidConfig(
            "anonymous server is not supported in auto-mode",
        ));
        return;
    }
    let gauge = &worker.gauge;
    let mut connected = false;

    while worker.running() {
        if !connected {
            // Вне подключения писать некуда: оценка места -- ноль.
            gauge.publish_ring(FreeSpace::ZERO);
            worker.report_evicted();
            // Ждём клиента ИЛИ команду/остановку (событие `wake`) -- без
            // опроса по таймеру.
            let accepted =
                server.wait_for_client_or(worker.wake.as_handle(), worker.options.poll_timeout);
            worker.stats.wake();
            match accepted {
                Ok(true) => {
                    connected = true;
                    gauge.peer_connected(server.peer_pid());
                    gauge
                        .peer_verified_pid
                        .store(server.verified_peer_pid().unwrap_or(0), Ordering::Release);
                    worker.handler.on_connect();
                }
                Ok(false) => continue,
                Err(err) => {
                    worker.handler.on_error(err);
                    continue;
                }
            }
        }

        let reason = 'session: {
            if !worker.flush(server) {
                break 'session Some(DisconnectReason::Error);
            }
            let outcome = worker.receive(server, &mut buffer);
            if outcome.fatal {
                break 'session Some(DisconnectReason::Error);
            }
            if outcome.more_pending || !worker.running() {
                // Ещё есть данные (или остановка) -- не блокируемся.
                break 'session None;
            }
            // Набор -- заимствование событий сервера на одно ожидание
            // (`wait_for_client_or`/`mark_disconnected` требуют `&mut`).
            let Some(events) = server.events() else {
                // Недостижимо: события проверены на входе в worker.
                break 'session Some(DisconnectReason::Error);
            };
            let space = events.s2c.space.as_handle();
            let set = WaitSet::new(
                [
                    events.disconnect.as_handle(),
                    events.c2s.data.as_handle(),
                    space,
                ],
                server.peer_wait_handle(),
                worker.wake.as_handle(),
            );
            let result = win::wait_any(set.handles(), worker.options.poll_timeout);
            worker.stats.wake();
            worker.handle_wait(server, &set, space, result, &mut buffer)
        };
        if let Some(reason) = reason {
            worker.notify_disconnect(reason);
            server.mark_disconnected();
            connected = false;
        }
    }

    // Остановка: принятое до `stop()` (например, прощальное сообщение)
    // дописываем в кольцо, сколько влезет, -- клиент дочитает его до
    // события отключения.
    if connected {
        let _ = worker.flush(server);
    }
}

/// Индексы событий канала в наборе ожидания worker-а. NT при одновременном
/// сигнале возвращает наименьший индекс: штатный DISCONNECT и данные имеют
/// приоритет над «процесс пира завершился», а тот -- над `wake`.
const DISCONNECT_INDEX: usize = 0;
const DATA_INDEX: usize = 1;
const SPACE_INDEX: usize = 2;

/// Набор ожидания worker-а: DISCONNECT, DATA, SPACE, handle процесса пира
/// (если наблюдается), `wake` (последним).
struct WaitSet<'a> {
    /// Хвост за `count` -- заполнитель (копия `wake`), в ожидание не уходит.
    handles: [BorrowedHandle<'a>; 5],
    count: usize,
    /// Индекс handle процесса пира.
    peer: Option<usize>,
}

impl<'a> WaitSet<'a> {
    const fn new(
        base: [BorrowedHandle<'a>; 3],
        peer: Option<BorrowedHandle<'a>>,
        wake: BorrowedHandle<'a>,
    ) -> Self {
        match peer {
            Some(peer) => Self {
                handles: [base[0], base[1], base[2], peer, wake],
                count: 5,
                peer: Some(3),
            },
            None => Self {
                handles: [base[0], base[1], base[2], wake, wake],
                count: 4,
                peer: None,
            },
        }
    }

    fn handles(&self) -> &[BorrowedHandle<'a>] {
        &self.handles[..self.count]
    }
}

#[derive(Debug)]
pub struct AutoClient {
    endpoint: Endpoint,
}

impl AutoClient {
    /// Клиент канала `name`: worker подключается и переподключается сам
    /// (раз в `reconnect_delay`), раскладку канала берёт из сегмента.
    pub fn connect(
        name: &str,
        handler: Arc<dyn AutoHandler>,
        options: AutoOptions,
    ) -> Result<Self> {
        Self::connect_with(name, handler, options, ClientMode::Reconnect)
    }

    /// Клиент выделенного канала Dispatch (ревизия 2): не переподключается.
    /// Первое подключение не удалось или канал разорван -- `on_error` (для
    /// неудачного подключения), `on_disconnect_reason(..)` и выход worker-а,
    /// без повторов к каналу, которого больше нет. PID сервера из
    /// рукопожатия публикуется в `server_pid` и НЕ стирается при разрыве.
    pub(crate) fn connect_dedicated(
        name: &str,
        handler: Arc<dyn AutoHandler>,
        options: AutoOptions,
        server_pid: Arc<AtomicU32>,
    ) -> Result<Self> {
        Self::connect_with(name, handler, options, ClientMode::Dedicated(server_pid))
    }

    fn connect_with(
        name: &str,
        handler: Arc<dyn AutoHandler>,
        options: AutoOptions,
        mode: ClientMode,
    ) -> Result<Self> {
        let name_owned = name.to_owned();
        let endpoint = Endpoint::spawn(
            handler,
            options,
            ChannelKind::ClientToServer,
            || format!("xsc-{name}"),
            None,
            move |worker| client_worker(&name_owned, worker, &mode),
        )?;
        Ok(Self { endpoint })
    }

    /// Раскладка канала текущего (или последнего) подключения; `None` --
    /// ещё ни разу не подключался.
    #[must_use]
    pub fn layout(&self) -> Option<ChannelLayout> {
        self.endpoint.gauge.layout()
    }

    /// Асинхронная отправка с политикой overwrite-oldest
    /// (см. `AutoServer::send`). Длина проверяется против лимита кольца
    /// Client -> Server, когда он известен (после первого подключения).
    pub fn send(&self, data: &[u8]) -> Result<()> {
        self.endpoint.send(data, false)
    }

    /// Асинхронная отправка без потерь с backpressure (`Err(QueueFull)`,
    /// когда в очереди `max_send_queue` непереданных сообщений). Полная
    /// семантика -- `AutoServer::try_send`.
    pub fn try_send(&self, data: &[u8]) -> Result<()> {
        self.endpoint.send(data, true)
    }

    /// Консервативная оценка места под новое сообщение в направлении
    /// Client -> Server (см. `AutoServer::free_space`).
    #[must_use]
    pub fn free_space(&self) -> FreeSpace {
        self.endpoint.free_space()
    }

    /// Жив ли процесс пира (см. `AutoServer::is_peer_alive`).
    #[must_use]
    pub fn is_peer_alive(&self) -> Option<bool> {
        self.endpoint.gauge.peer_alive()
    }

    /// PID наблюдаемого пира (после `PeerDied` -- PID умершего процесса).
    #[must_use]
    pub fn peer_pid(&self) -> Option<u32> {
        self.endpoint.gauge.peer_pid()
    }

    /// Остановить клиента синхронно (см. `AutoServer::stop`): сервер
    /// получает `DISCONNECT` после того, как очередь дописана в кольцо.
    pub fn stop(&self) {
        self.endpoint.stop();
    }

    #[must_use]
    pub fn stats(&self) -> AutoStatsSnapshot {
        self.endpoint.stats.snapshot()
    }

    /// Число пробуждений worker-а (тесты: ноль в простое).
    #[cfg(test)]
    pub(crate) fn wakeups(&self) -> u64 {
        self.endpoint.stats.wakeups.load(Ordering::Relaxed)
    }
}

/// Режим клиентского worker-а.
enum ClientMode {
    /// `AutoClient::connect`: переподключаться раз в `reconnect_delay`.
    Reconnect,
    /// Выделенный канал Dispatch: одно подключение; PID сервера -- сюда.
    Dedicated(Arc<AtomicU32>),
}

fn client_worker(name: &str, worker: &Worker, mode: &ClientMode) {
    let mut buffer = Vec::new();
    let gauge = &worker.gauge;
    let dedicated = matches!(mode, ClientMode::Dedicated(_));

    while worker.running() {
        // Ревизия 2: ожидание `S2C_CONNECT` прерывается `wake` -- `stop`/Drop
        // не ждут `connect_timeout`; `send` ожидание не прерывает.
        let mut should_stop = || !worker.running();
        let connected = SharedClient::connect_interruptible(
            name,
            worker.options.connect_timeout,
            Interrupt {
                wake: worker.wake.as_handle(),
                should_stop: &mut should_stop,
            },
        );
        let mut client = match connected {
            Ok(client) => client,
            Err(_) if !worker.running() => break,
            Err(err) => {
                worker.handler.on_error(err);
                if dedicated {
                    // Выделенный канал не поднялся -- повторять не к чему
                    // (сервер его уже снёс или не дождался нас).
                    worker.notify_disconnect(DisconnectReason::Error);
                    break;
                }
                worker.report_evicted();
                // Отступ перед новой попыткой; остановка прерывает его сразу
                // (событие `wake`), без сна-циклов.
                if !wait_delay_or(
                    &worker.running,
                    &worker.wake,
                    worker.options.reconnect_delay,
                ) {
                    break;
                }
                continue;
            }
        };

        gauge.set_layout(*client.layout(), worker.tx);
        gauge.peer_connected(client.peer_pid());
        if let ClientMode::Dedicated(server_pid) = mode {
            server_pid.store(client.peer_pid().unwrap_or(0), Ordering::Release);
        }
        worker.handler.on_connect();
        let client_events = client.events();
        let space = client_events.c2s.space.as_handle();
        let base_handles = [
            client_events.disconnect.as_handle(),
            client_events.s2c.data.as_handle(),
            space,
        ];

        let reason = loop {
            if !worker.running() {
                // Остановка: принятое до `stop()` (например, прощальное
                // сообщение) дописываем в кольцо, сколько влезет, -- сервер
                // дочитает его до события отключения (Drop клиента).
                let _ = worker.flush(&client);
                break None;
            }
            if !worker.flush(&client) {
                break Some(DisconnectReason::Error);
            }
            let outcome = worker.receive(&client, &mut buffer);
            if outcome.fatal {
                break Some(DisconnectReason::Error);
            }
            if outcome.more_pending {
                continue;
            }
            let set = WaitSet::new(
                base_handles,
                client.peer_wait_handle(),
                worker.wake.as_handle(),
            );
            let result = win::wait_any(set.handles(), worker.options.poll_timeout);
            worker.stats.wake();
            if let Some(reason) = worker.handle_wait(&client, &set, space, result, &mut buffer) {
                break Some(reason);
            }
        };
        if let Some(reason) = reason {
            worker.notify_disconnect(reason);
            client.mark_disconnected();
        }

        // Соединение потеряно: писать некуда, оценка места -- ноль.
        gauge.publish_ring(FreeSpace::ZERO);
        drop(client);
        if dedicated || !worker.running() {
            break;
        }
        if !wait_delay_or(
            &worker.running,
            &worker.wake,
            worker.options.reconnect_delay,
        ) {
            break;
        }
    }
}

/// Пишет очередь в кольцо, пока пишется. `false` -- кольцо испорчено
/// (`Corrupted`): соединение надо разорвать.
///
/// Обычные сообщения -- `write_message` (overwrite-oldest), lossless --
/// `try_write_message`. Если lossless-сообщение в голове очереди не влезает,
/// worker оставляет в кольце заявку (`arm_space_waiter`) и ОДИН раз
/// перепроверяет -- после этого либо запись прошла, либо читатель гарантированно
/// увидит заявку и просигналит `SPACE`, который будит `wait_any` worker-а.
/// Порядок FIFO сохраняется: пока голова ждёт места, остальные тоже ждут.
fn process_send_queue<E>(
    endpoint: &E,
    outbox: &Outbox,
    handler: &Arc<dyn AutoHandler>,
    stats: &Arc<AutoStats>,
    gauge: &ChannelState,
    direction: ChannelKind,
) -> bool
where
    E: SendEndpoint,
{
    // `armed` -- заявка поставлена в ЭТОМ проходе; `cleared` -- заявка,
    // возможно оставшаяся с прошлого прохода (читатель её не снял: места
    // было мало), уже снята. Снимаем на первой же успешной записи -- одна
    // запись в заголовок за проход.
    let mut armed = false;
    let mut cleared = false;
    let mut healthy = true;
    while let Some(msg) = outbox.pop_front() {
        let result = if msg.lossless {
            endpoint.try_write(&msg.data)
        } else {
            endpoint.write(&msg.data)
        };
        match result {
            Ok(outcome) => {
                if armed || !cleared {
                    endpoint.disarm_space_waiter();
                    armed = false;
                    cleared = true;
                }
                // Сначала снимок кольца (уже с этим сообщением), потом
                // списание из pending -- порядок описан у `ChannelState`.
                gauge.publish_ring(endpoint.free_space());
                gauge.release(msg.frame());
                // Relaxed: статистика.
                stats.sent_messages.fetch_add(1, Ordering::Relaxed);
                if outcome.overwritten > 0 {
                    stats
                        .send_overflows
                        .fetch_add(u64::from(outcome.overwritten), Ordering::Relaxed);
                    handler.on_overflow(direction, outcome.overwritten);
                }
            }
            Err(ShmError::QueueFull) if msg.lossless && !armed => {
                endpoint.arm_space_waiter(msg.frame() as u32);
                armed = true;
                outbox.push_front(msg);
            }
            Err(ShmError::QueueFull) => {
                outbox.push_front(msg);
                break;
            }
            Err(err @ (ShmError::MessageTooSmall | ShmError::MessageTooLarge)) => {
                // Сообщение невалидно для этого кольца навсегда (клиент
                // принял его до того, как узнал лимит канала) -- не
                // блокировать им очередь.
                handler.on_error(err);
                gauge.release(msg.frame());
            }
            Err(err) => {
                healthy = !matches!(err, ShmError::Corrupted);
                handler.on_error(err);
                outbox.push_front(msg);
                break;
            }
        }
    }
    gauge.publish_ring(endpoint.free_space());
    healthy
}

/// Результат обработки приёмной очереди за один проход.
struct ReceiveOutcome {
    /// Фатальная ошибка — соединение надо сбросить.
    fatal: bool,
    /// Батч упёрся в лимит, в кольце вероятно ещё есть данные — не спать.
    more_pending: bool,
}

/// Обрабатывает до `batch` сообщений за вызов.
fn process_receive_queue<R>(
    endpoint: &R,
    handler: &Arc<dyn AutoHandler>,
    stats: &Arc<AutoStats>,
    buffer: &mut Vec<u8>,
    batch: usize,
    direction: ChannelKind,
) -> ReceiveOutcome
where
    R: ReceiveEndpoint,
{
    for _ in 0..batch.max(1) {
        match endpoint.read(buffer) {
            Ok(len) => {
                // Relaxed: статистика.
                stats.received_messages.fetch_add(1, Ordering::Relaxed);
                handler.on_message(direction, &buffer[..len]);
            }
            Err(
                ShmError::QueueEmpty
                | ShmError::NotConnected
                | ShmError::NotReady
                | ShmError::Timeout,
            ) => {
                return ReceiveOutcome {
                    fatal: false,
                    more_pending: false,
                };
            }
            Err(err @ ShmError::Corrupted) => {
                handler.on_error(err);
                return ReceiveOutcome {
                    fatal: true,
                    more_pending: false,
                };
            }
            Err(err) => {
                handler.on_error(err);
                return ReceiveOutcome {
                    fatal: false,
                    more_pending: false,
                };
            }
        }
    }
    ReceiveOutcome {
        fatal: false,
        more_pending: true,
    }
}

trait SendEndpoint {
    fn write(&self, data: &[u8]) -> Result<WriteOutcome>;
    fn try_write(&self, data: &[u8]) -> Result<WriteOutcome>;
    fn free_space(&self) -> FreeSpace;
    fn arm_space_waiter(&self, frame: u32);
    fn disarm_space_waiter(&self);
}

trait ReceiveEndpoint {
    fn read(&self, buffer: &mut Vec<u8>) -> Result<usize>;
}

impl SendEndpoint for SharedServer {
    fn write(&self, data: &[u8]) -> Result<WriteOutcome> {
        self.send_to_client(data)
    }
    fn try_write(&self, data: &[u8]) -> Result<WriteOutcome> {
        self.try_send_to_client(data)
    }
    fn free_space(&self) -> FreeSpace {
        SharedServer::free_space(self)
    }
    fn arm_space_waiter(&self, frame: u32) {
        SharedServer::arm_space_waiter(self, frame);
    }
    fn disarm_space_waiter(&self) {
        SharedServer::disarm_space_waiter(self);
    }
}

impl ReceiveEndpoint for SharedServer {
    fn read(&self, buffer: &mut Vec<u8>) -> Result<usize> {
        self.receive_from_client(buffer)
    }
}

impl SendEndpoint for SharedClient {
    fn write(&self, data: &[u8]) -> Result<WriteOutcome> {
        self.send_to_server(data)
    }
    fn try_write(&self, data: &[u8]) -> Result<WriteOutcome> {
        self.try_send_to_server(data)
    }
    fn free_space(&self) -> FreeSpace {
        SharedClient::free_space(self)
    }
    fn arm_space_waiter(&self, frame: u32) {
        SharedClient::arm_space_waiter(self, frame);
    }
    fn disarm_space_waiter(&self) {
        SharedClient::disarm_space_waiter(self);
    }
}

impl ReceiveEndpoint for SharedClient {
    fn read(&self, buffer: &mut Vec<u8>) -> Result<usize> {
        self.receive_from_server(buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Handler, который в `on_disconnect` дропает контейнер, содержащий сам
    /// `AutoServer` -- воспроизводит паттерн, который вызвал self-join
    /// deadlock в `dispatch/` (аудит 2026-07-10, до фикса): callback
    /// вызывается СИНХРОННО из собственного worker-потока `AutoServer`, и
    /// если внутри него этот же `AutoServer` дропается, `Drop::drop` пытается
    /// заджойнить `worker_handle`, которым и является текущий поток.
    struct SelfDroppingHandler {
        container: Arc<Mutex<Option<AutoServer>>>,
        disconnect_returned: Arc<AtomicBool>,
    }

    impl AutoHandler for SelfDroppingHandler {
        fn on_disconnect(&self) {
            let taken = self.container.lock().unwrap().take();
            drop(taken); // именно тут раньше был self-join deadlock
            self.disconnect_returned.store(true, Ordering::Release);
        }
    }

    struct NoopHandler;
    impl AutoHandler for NoopHandler {}

    /// Регрессия (аудит 2026-07-10): `Drop for AutoServer`/`AutoClient`
    /// раньше безусловно джойнил `worker_handle` -- если Drop вызывается ИЗ
    /// СОБСТВЕННОГО worker-потока (пользовательский `AutoHandler` синхронно
    /// роняет `AutoServer` внутри своего же callback'а), это self-join
    /// deadlock. `auto` -- публичный модуль, доступный любому внешнему
    /// потребителю библиотеки, поэтому фикс должен быть в самом `Drop`, а не
    /// полагаться на то, что каждый вызывающий код (как `dispatch/`) сам
    /// не наступит на эти грабли.
    #[test]
    fn drop_from_own_worker_callback_does_not_self_join_deadlock() {
        let name = format!("TEST_AUTO_SELFDROP_{}", std::process::id());
        let container: Arc<Mutex<Option<AutoServer>>> = Arc::new(Mutex::new(None));
        let disconnect_returned = Arc::new(AtomicBool::new(false));

        let handler = Arc::new(SelfDroppingHandler {
            container: container.clone(),
            disconnect_returned: disconnect_returned.clone(),
        });

        let server = AutoServer::start(&name, handler, AutoOptions::default()).expect("start");
        *container.lock().unwrap() = Some(server);

        let client = AutoClient::connect(&name, Arc::new(NoopHandler), AutoOptions::default())
            .expect("client connect");

        // Дождаться реального подключения (сервер принял клиента), прежде
        // чем отключать: с ревизии 2 `stop()` во время рукопожатия отзывает
        // заявку (соединения и `on_disconnect` тогда нет), а фиксированная
        // пауза под нагрузкой полного прогона не гарантирует подключения.
        let start = std::time::Instant::now();
        while container
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|s| s.is_peer_alive() != Some(true))
            && start.elapsed() < Duration::from_secs(10)
        {
            thread::sleep(Duration::from_millis(5));
        }
        client.stop();

        // Если self-join deadlock всё ещё существует, on_disconnect зависнет
        // НАВСЕГДА внутри Drop for AutoServer, и флаг не станет true.
        let start = std::time::Instant::now();
        while !disconnect_returned.load(Ordering::Acquire)
            && start.elapsed() < Duration::from_secs(10)
        {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            disconnect_returned.load(Ordering::Acquire),
            "on_disconnect не вернулся за 10с -- self-join deadlock"
        );
    }
}

#[cfg(test)]
mod lossless_tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Default)]
    struct Counters {
        overflows: AtomicU32,
        errors: AtomicU32,
    }
    impl AutoHandler for Counters {
        fn on_overflow(&self, _direction: ChannelKind, count: u32) {
            self.overflows.fetch_add(count, Ordering::Relaxed);
        }
        fn on_error(&self, _err: ShmError) {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Лимит длины мок-кольца.
    const MOCK_MAX: usize = 100;

    /// Поддельный endpoint: кольцо «вмещает» `room` сообщений; `try_write`
    /// при нехватке отвечает QueueFull, `write` -- как overwrite (всегда Ok).
    struct MockEndpoint {
        room: Cell<usize>,
        written: RefCell<Vec<Vec<u8>>>,
        armed: Cell<u32>,
        arms: Cell<u32>,
    }
    impl MockEndpoint {
        fn new(room: usize) -> Self {
            Self {
                room: Cell::new(room),
                written: RefCell::new(Vec::new()),
                armed: Cell::new(0),
                arms: Cell::new(0),
            }
        }
        fn check(data: &[u8]) -> Result<()> {
            if data.len() > MOCK_MAX {
                return Err(ShmError::MessageTooLarge);
            }
            Ok(())
        }
        fn accept(&self, data: &[u8]) -> Result<WriteOutcome> {
            Self::check(data)?;
            self.written.borrow_mut().push(data.to_vec());
            self.room.set(self.room.get().saturating_sub(1));
            Ok(WriteOutcome {
                overwritten: 0,
                was_empty: false,
            })
        }
    }
    impl SendEndpoint for MockEndpoint {
        fn write(&self, data: &[u8]) -> Result<WriteOutcome> {
            self.accept(data)
        }
        fn try_write(&self, data: &[u8]) -> Result<WriteOutcome> {
            Self::check(data)?;
            if self.room.get() == 0 {
                return Err(ShmError::QueueFull);
            }
            self.accept(data)
        }
        fn free_space(&self) -> FreeSpace {
            FreeSpace {
                bytes: self.room.get() * 1000,
                messages: self.room.get() as u32,
                max_message_size: MOCK_MAX,
            }
        }
        fn arm_space_waiter(&self, frame: u32) {
            self.armed.set(frame);
            self.arms.set(self.arms.get() + 1);
        }
        fn disarm_space_waiter(&self) {
            self.armed.set(0);
        }
    }

    struct Harness {
        outbox: Outbox,
        wake: EventHandle,
        running: AtomicBool,
        gauge: ChannelState,
        counters: Arc<Counters>,
        handler: Arc<dyn AutoHandler>,
        stats: Arc<AutoStats>,
    }

    impl Harness {
        fn new() -> Self {
            let counters = Arc::new(Counters::default());
            Self {
                outbox: Outbox::default(),
                wake: EventHandle::create_unnamed(false).unwrap(),
                running: AtomicBool::new(true),
                gauge: ChannelState::default(),
                handler: counters.clone(),
                counters,
                stats: Arc::default(),
            }
        }
        fn send(&self, data: &[u8], lossless: bool, cap: usize) -> Result<()> {
            enqueue(
                &self.outbox,
                &self.wake,
                &self.running,
                &self.gauge,
                data,
                lossless,
                cap,
            )
        }
        fn queued(&self) -> Vec<Vec<u8>> {
            self.outbox.lock().iter().map(|m| m.data.clone()).collect()
        }
        fn flush(&self, ep: &MockEndpoint) -> bool {
            let dir = ChannelKind::ClientToServer;
            process_send_queue(
                ep,
                &self.outbox,
                &self.handler,
                &self.stats,
                &self.gauge,
                dir,
            )
        }
    }

    /// `try_send` ограничен `max_send_queue` на входе; `send` при
    /// переполнении вытесняет только обычные сообщения (сразу, на стороне
    /// отправителя -- память ограничена, даже если worker занят), а если в
    /// очереди одни lossless -- выбрасывается само новое сообщение `send`.
    #[test]
    fn queue_policy_never_evicts_lossless() {
        let h = Harness::new();
        h.send(b"L1", true, 2).unwrap();
        h.send(b"L2", true, 2).unwrap();
        assert_eq!(h.send(b"L3", true, 2), Err(ShmError::QueueFull));
        assert_eq!(
            h.send(b"", true, 2),
            Err(ShmError::MessageTooSmall),
            "длина проверяется синхронно"
        );
        h.send(b"S1", false, 2).unwrap();
        assert_eq!(h.queued(), [b"L1".to_vec(), b"L2".to_vec()]);
        assert_eq!(h.outbox.evicted.load(Ordering::Acquire), 1);
        assert_eq!(h.gauge.pending_msgs.load(Ordering::Acquire), 2);
        assert_eq!(
            h.gauge.pending_bytes.load(Ordering::Acquire),
            2 * frame_size(2)
        );

        // Смешанная очередь: вытесняется самое старое ОБЫЧНОЕ сообщение.
        let h = Harness::new();
        h.send(b"S0", false, 2).unwrap();
        h.send(b"L0", true, 2).unwrap();
        h.send(b"S2", false, 2).unwrap();
        assert_eq!(h.queued(), [b"L0".to_vec(), b"S2".to_vec()]);
        assert_eq!(h.gauge.pending_msgs.load(Ordering::Acquire), 2);
        assert_eq!(h.outbox.evicted.load(Ordering::Acquire), 1);
    }

    /// `max_send_queue = 0` трактуется как 1 одинаково для `send` и
    /// `try_send` (раньше `send` выбрасывал всё).
    #[test]
    fn zero_queue_limit_means_one() {
        let h = Harness::new();
        h.send(b"a1", false, 0).unwrap();
        assert_eq!(h.queued(), [b"a1".to_vec()]);
        assert_eq!(h.send(b"b1", true, 0), Err(ShmError::QueueFull));
    }

    /// Длина проверяется синхронно против лимита кольца, как только он
    /// известен (у сервера -- сразу, у клиента -- после подключения).
    #[test]
    fn send_checks_ring_limit_synchronously() {
        let h = Harness::new();
        h.send(&[0u8; 5000], false, 8).unwrap();
        h.gauge.set_layout(
            ChannelLayout::symmetric(crate::layout::RingConfig::new(8192).max_message_size(1000)),
            ChannelKind::ClientToServer,
        );
        assert_eq!(
            h.send(&[0u8; 1001], false, 8),
            Err(ShmError::MessageTooLarge)
        );
        assert_eq!(
            h.send(&[0u8; 1001], true, 8),
            Err(ShmError::MessageTooLarge)
        );
        h.send(&[0u8; 1000], true, 8).unwrap();
    }

    /// Голова-lossless не влезает: worker ставит заявку ровно один раз,
    /// ничего не теряет и не обгоняет (FIFO); освободилось место --
    /// очередь уходит целиком, заявка снимается, учёт pending обнуляется.
    #[test]
    fn blocked_lossless_head_keeps_fifo_and_arms_waiter() {
        let h = Harness::new();
        for i in 0..5u8 {
            h.send(&[i, i, i], true, 16).unwrap();
        }
        let ep = MockEndpoint::new(2);
        assert!(h.flush(&ep));
        assert_eq!(ep.written.borrow().len(), 2);
        assert_eq!(h.queued().len(), 3);
        assert_eq!(ep.arms.get(), 1, "заявка -- один раз за проход");
        assert_eq!(ep.armed.get(), frame_size(3) as u32);
        assert_eq!(h.gauge.pending_msgs.load(Ordering::Acquire), 3);
        // Оценка места: кольцо пусто (room=0), а 3 сообщения ещё ждут.
        assert_eq!(h.gauge.estimate(16), FreeSpace::ZERO);

        ep.room.set(10);
        assert!(h.flush(&ep));
        assert!(h.queued().is_empty());
        assert_eq!(ep.armed.get(), 0, "успешная запись снимает заявку");
        let written = ep.written.borrow();
        for (i, msg) in written.iter().enumerate() {
            assert_eq!(msg, &vec![i as u8; 3], "порядок нарушен");
        }
        assert_eq!(h.gauge.pending_msgs.load(Ordering::Acquire), 0);
        assert_eq!(h.gauge.pending_bytes.load(Ordering::Acquire), 0);
        assert_eq!(h.stats.sent_messages.load(Ordering::Relaxed), 5);
        assert_eq!(h.counters.overflows.load(Ordering::Relaxed), 0);
    }

    /// Сообщение, принятое клиентом до того, как он узнал лимит канала, и
    /// не влезающее в кольцо, не блокирует очередь: `on_error` и дальше.
    #[test]
    fn oversized_message_does_not_wedge_queue() {
        let h = Harness::new();
        h.send(&[1u8; MOCK_MAX + 1], false, 16).unwrap();
        h.send(b"ok", false, 16).unwrap();
        let ep = MockEndpoint::new(10);
        assert!(h.flush(&ep));
        assert!(h.queued().is_empty());
        assert_eq!(*ep.written.borrow(), [b"ok".to_vec()]);
        assert_eq!(h.counters.errors.load(Ordering::Relaxed), 1);
        assert_eq!(h.gauge.pending_msgs.load(Ordering::Acquire), 0);
    }

    /// Оценка места консервативна: вычитает ещё не записанное и ограничена
    /// свободным местом очереди.
    #[test]
    fn gauge_estimate_is_conservative() {
        let gauge = ChannelState::default();
        assert_eq!(gauge.estimate(8), FreeSpace::ZERO, "до подключения");
        gauge.set_layout(ChannelLayout::DEFAULT, ChannelKind::ServerToClient);
        gauge.publish_ring(FreeSpace {
            bytes: 10_000,
            messages: 100,
            max_message_size: 0,
        });
        assert_eq!(
            gauge.estimate(8),
            FreeSpace {
                bytes: 10_000,
                messages: 8,
                max_message_size: crate::constants::DEFAULT_MAX_MESSAGE_SIZE,
            }
        );
        gauge.acquire(1000);
        gauge.acquire(1000);
        let est = gauge.estimate(8);
        assert_eq!(est.bytes, 8_000);
        assert_eq!(est.messages, 6);
        assert!(!gauge.try_acquire(10, 2), "очередь 2/2");
        gauge.release(1000);
        assert!(gauge.try_acquire(10, 2));
    }

    #[derive(Default)]
    struct Counting {
        connected: AtomicBool,
        received: AtomicU64,
    }

    impl AutoHandler for Counting {
        fn on_connect(&self) {
            self.connected.store(true, Ordering::Release);
        }
        fn on_message(&self, _direction: ChannelKind, _payload: &[u8]) {
            self.received.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn wait_until(what: &str, limit: Duration, mut cond: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + limit;
        while !cond() {
            assert!(std::time::Instant::now() < deadline, "таймаут: {what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// 0.9: подключённые `AutoServer`/`AutoClient` в простое не просыпаются
    /// ни разу (до 0.9 -- каждые 50 мс), а каждое сообщение -- считанные
    /// пробуждения.
    #[test]
    fn idle_worker_does_not_wake_up() {
        let name = format!("AUTO_IDLE_{}", std::process::id());
        let server_h = Arc::new(Counting::default());
        let client_h = Arc::new(Counting::default());
        let server = AutoServer::start(&name, server_h.clone(), AutoOptions::default()).unwrap();
        let client = AutoClient::connect(&name, client_h.clone(), AutoOptions::default()).unwrap();
        wait_until("подключение", Duration::from_secs(5), || {
            server_h.connected.load(Ordering::Acquire) && client_h.connected.load(Ordering::Acquire)
        });
        client.send(b"ping").unwrap();
        wait_until("доставка", Duration::from_secs(2), || {
            server_h.received.load(Ordering::Acquire) == 1
        });
        thread::sleep(Duration::from_millis(200));

        let (s0, c0) = (server.wakeups(), client.wakeups());
        thread::sleep(Duration::from_secs(1));
        assert_eq!(server.wakeups() - s0, 0, "сервер просыпался в простое");
        assert_eq!(client.wakeups() - c0, 0, "клиент просыпался в простое");

        // Одно сообщение -- ограниченное число пробуждений, не тик.
        client.send(b"ping").unwrap();
        wait_until("доставка", Duration::from_secs(2), || {
            server_h.received.load(Ordering::Acquire) == 2
        });
        thread::sleep(Duration::from_millis(100));
        assert!(
            server.wakeups() - s0 <= 3,
            "сервер: {}",
            server.wakeups() - s0
        );
        assert!(
            client.wakeups() - c0 <= 3,
            "клиент: {}",
            client.wakeups() - c0
        );
        drop(client);
        drop(server);
    }

    /// Ревизия 2, дефект 4: `stop()`/Drop клиента, ждущего `S2C_CONNECT`
    /// (сервер создал канал, но не принимает), возвращаются сразу -- ожидание
    /// рукопожатия прерывает `wake`, а не `connect_timeout` (2 с). Заявка при
    /// этом отозвана: `client_state` снова `IDLE`.
    #[test]
    fn stop_during_connect_is_prompt() {
        struct Quiet;
        impl AutoHandler for Quiet {}
        const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
        let name = format!("AUTO_STOPCONN_{}", std::process::id());
        // Сервер без `wait_for_client`: заявку никто не примет.
        let server = SharedServer::start(&name).unwrap();
        let client_state = || {
            server
                .view()
                .control_block()
                .client_state
                .load(Ordering::Acquire)
        };
        for use_stop in [true, false] {
            // Лучшая из трёх попыток: единичный всплеск планировщика под
            // нагрузкой полного прогона -- не то, что проверяется; проверяется,
            // что остановка не ждёт `connect_timeout` (30 с).
            let mut best = Duration::MAX;
            for _ in 0..3 {
                let client = AutoClient::connect(
                    &name,
                    Arc::new(Quiet),
                    AutoOptions {
                        connect_timeout: CONNECT_TIMEOUT,
                        ..AutoOptions::default()
                    },
                )
                .unwrap();
                wait_until("заявка подана", Duration::from_secs(5), || {
                    client_state() == crate::constants::HANDSHAKE_CLIENT_HELLO
                });
                // `send` будит worker, но подключение не прерывает.
                client.send(b"queued").unwrap();
                thread::sleep(Duration::from_millis(50));
                assert_eq!(
                    client_state(),
                    crate::constants::HANDSHAKE_CLIENT_HELLO,
                    "`send` прервал рукопожатие"
                );
                let t0 = std::time::Instant::now();
                if use_stop {
                    client.stop();
                }
                drop(client);
                let took = t0.elapsed();
                assert!(
                    took < CONNECT_TIMEOUT / 10,
                    "остановка ждала рукопожатия: {took:?}"
                );
                best = best.min(took);
                assert_eq!(
                    client_state(),
                    crate::constants::HANDSHAKE_IDLE,
                    "заявка не отозвана"
                );
            }
            assert!(
                best < Duration::from_millis(200),
                "stop = {use_stop}: лучшая из трёх {best:?}"
            );
        }
    }
}
