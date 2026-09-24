use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use std::sync::mpsc::{self, Receiver, Sender};

use crate::client::SharedClient;
use crate::constants::{MAX_MESSAGE_SIZE, MESSAGE_HEADER_SIZE};
use crate::error::{DisconnectReason, Result, ShmError};
use crate::ring::{FreeSpace, WriteOutcome, frame_len};
use crate::server::SharedServer;
use crate::wait_delay;
use crate::win::{self};

fn map_spawn_error(err: std::io::Error, context: &'static str) -> ShmError {
    let code = err.raw_os_error().map(|c| c as u32).unwrap_or(0xFFFFFFFF);
    ShmError::WindowsError { code, context }
}

/// Джойнит worker-поток, если это безопасно; при self-join -- отпускает
/// `JoinHandle` без блокировки.
///
/// Если `Drop for AutoServer`/`AutoClient` вызывается СИНХРОННО из
/// собственного worker-потока (пользовательский `AutoHandler` дропает
/// сервер/клиент прямо внутри своего же callback'а -- `on_disconnect`,
/// `on_message` и т.п. вызываются ИМЕННО на worker-потоке), безусловный
/// `handle.join()` был бы self-join deadlock: поток ждал бы сам себя
/// навсегда (аудит 2026-07-10 -- тот же паттерн, что вызвал деадлок в
/// `dispatch/`, но здесь он общий для любого потребителя `auto`, публичного
/// модуля, а не только для одного места использования внутри библиотеки).
///
/// При обнаружении self-join `JoinHandle` просто дропается без join: поток
/// уже видит `running=false` (устанавливается до этого вызова) и завершится
/// сам -- безопасный detach силами ОС, не утечка (тред всё равно скоро
/// вернёт управление и выйдет из своего цикла).
fn join_unless_self(handle: JoinHandle<()>) {
    if handle.thread().id() != thread::current().id() {
        let _ = handle.join();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    ServerToClient,
    ClientToServer,
}

/// Callback-интерфейс `AutoServer`/`AutoClient`.
///
/// Все методы вызываются СИНХРОННО из собственного worker-потока
/// `AutoServer`/`AutoClient`. Если реализация синхронно дропает тот же
/// `AutoServer`/`AutoClient`, который её вызвал (например, извлекает его из
/// общей коллекции и роняет прямо в callback'е), `Drop` НЕ будет ждать
/// (`join()`) этот же worker-поток -- вместо самоблокировки поток просто
/// открепляется (detach) и завершится самостоятельно чуть позже (аудит
/// 2026-07-10). Это исключает deadlock, но означает, что к моменту
/// возврата из `Drop` поток может быть ещё не завершён -- если нужна
/// гарантия полного завершения, дропайте объект из ДРУГОГО потока.
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
    fn on_overflow(&self, _direction: ChannelKind, _count: u32) {}
    fn on_space_available(&self, _direction: ChannelKind) {}
    fn on_error(&self, _err: ShmError) {}
}

#[derive(Clone, Debug)]
pub struct AutoOptions {
    /// Интервал опроса в worker loop. Переименовано из `wait_timeout` (0.6.0)
    /// для единообразия с `MultiOptions`/`DispatchOptions`, где то же самое
    /// поле называется `poll_timeout` -- разнобой имён заставлял вручную
    /// перекладывать значения при построении `AutoOptions` внутри `dispatch/`.
    pub poll_timeout: Duration,
    pub reconnect_delay: Duration,
    pub connect_timeout: Duration,
    pub max_send_queue: usize,
    pub recv_batch: usize,
}

impl Default for AutoOptions {
    fn default() -> Self {
        Self {
            poll_timeout: Duration::from_millis(50),
            reconnect_delay: Duration::from_millis(250),
            connect_timeout: Duration::from_secs(2),
            max_send_queue: 256,
            recv_batch: 32,
        }
    }
}

#[derive(Default, Clone, Debug)]
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
}

impl AutoStats {
    fn snapshot(&self) -> AutoStatsSnapshot {
        AutoStatsSnapshot {
            sent_messages: self.sent_messages.load(Ordering::Relaxed),
            send_overflows: self.send_overflows.load(Ordering::Relaxed),
            received_messages: self.received_messages.load(Ordering::Relaxed),
            receive_overflows: self.receive_overflows.load(Ordering::Relaxed),
        }
    }
}

/// Исходящее сообщение в очереди worker-а.
#[derive(Debug)]
struct Outgoing {
    data: Vec<u8>,
    /// `true` -- пришло через `try_send`: пишется в кольцо только
    /// `try_write_message` (без перезаписи) и никогда не вытесняется из
    /// очереди; `false` -- обычный `send` с политикой overwrite-oldest.
    lossless: bool,
}

impl Outgoing {
    /// Байты, которые сообщение займёт в кольце (заголовок + payload).
    const fn frame(&self) -> usize {
        MESSAGE_HEADER_SIZE + self.data.len()
    }
}

#[derive(Debug)]
enum WorkerCommand {
    Send(Outgoing),
    Shutdown,
}

/// Очередь неотправленных сообщений worker-потока.
///
/// Никакой синхронизации: создаётся ВНУТРИ `server_worker`/`client_worker` и
/// видна только этому потоку (внешние отправители кладут сообщения через
/// `mpsc::Sender`, а не сюда). Раньше здесь был `Mutex<VecDeque<..>>` -- три
/// захвата lock-а на каждое сообщение без единого конкурента.
type SendQueue = VecDeque<Outgoing>;

/// Счётчики исходящего направления, общие для API-потоков и worker-а.
///
/// `pending_*` -- принятые `send`/`try_send`, но ещё не записанные в кольцо
/// (в mpsc-канале или в `SendQueue`); `ring_free_*` -- последний снимок
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
#[derive(Debug, Default)]
struct ChannelState {
    pending_msgs: AtomicUsize,
    pending_bytes: AtomicUsize,
    ring_free_bytes: AtomicUsize,
    ring_free_msgs: AtomicU32,
    peer_pid: AtomicU32,
    peer_status: AtomicU8,
}

const PEER_UNKNOWN: u8 = 0;
const PEER_ALIVE: u8 = 1;
const PEER_DEAD: u8 = 2;

impl ChannelState {
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

    /// Принять сообщение в учёт (до отправки команды worker-у).
    fn acquire(&self, frame: usize) {
        self.pending_bytes.fetch_add(frame, Ordering::AcqRel);
        self.pending_msgs.fetch_add(1, Ordering::AcqRel);
    }

    /// То же, но только если в очереди есть место (`try_send`): CAS-цикл,
    /// чтобы несколько отправителей не превысили `max_send_queue`.
    fn try_acquire(&self, frame: usize, max_send_queue: usize) -> bool {
        let admitted = self
            .pending_msgs
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
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
        FreeSpace {
            bytes: ring_bytes.saturating_sub(pending_bytes),
            messages: queue_room.min(ring_room) as u32,
        }
    }
}

/// Общая для `AutoServer`/`AutoClient` постановка сообщения в очередь.
fn enqueue(
    cmd_tx: &Sender<WorkerCommand>,
    running: &AtomicBool,
    gauge: &ChannelState,
    data: &[u8],
    lossless: bool,
    max_send_queue: usize,
) -> Result<()> {
    if !running.load(Ordering::Acquire) {
        return Err(ShmError::NotReady);
    }
    let frame = MESSAGE_HEADER_SIZE + data.len();
    if lossless {
        // Длину проверяем синхронно: иначе ошибка ушла бы в on_error
        // асинхронно, а вызывающий считал бы сообщение принятым.
        frame_len(data.len())?;
        if !gauge.try_acquire(frame, max_send_queue.max(1)) {
            return Err(ShmError::QueueFull);
        }
    } else {
        gauge.acquire(frame);
    }
    let msg = Outgoing {
        data: data.to_vec(),
        lossless,
    };
    cmd_tx.send(WorkerCommand::Send(msg)).map_err(|_| {
        gauge.release(frame);
        ShmError::NotReady
    })
}

#[derive(Debug)]
pub struct AutoServer {
    cmd_tx: Sender<WorkerCommand>,
    join: Mutex<Option<JoinHandle<()>>>,
    stats: Arc<AutoStats>,
    running: Arc<AtomicBool>,
    gauge: Arc<ChannelState>,
    max_send_queue: usize,
}

impl AutoServer {
    pub fn start(name: &str, handler: Arc<dyn AutoHandler>, options: AutoOptions) -> Result<Self> {
        let mut server = SharedServer::start(name)?;
        let (tx, rx) = mpsc::channel();
        let stats = Arc::new(AutoStats::default());
        let running = Arc::new(AtomicBool::new(true));
        let gauge = Arc::new(ChannelState::default());
        let max_send_queue = options.max_send_queue;
        let join_running = running.clone();
        let join_stats = stats.clone();
        let join_handler = handler.clone();
        let join_gauge = gauge.clone();
        // Thread name in debug only (opaque short tag `xsa-{name}` so
        // local traces still line up with the segment), anonymous in
        // release so Process Explorer / Process Hacker doesn't surface
        // "xshm-auto-server-…" as a flashing signpost on the host process.
        #[cfg_attr(not(debug_assertions), allow(unused_mut))]
        let mut builder = thread::Builder::new();
        #[cfg(debug_assertions)]
        {
            builder = builder.name(format!("xsa-{name}"));
        }
        let join = builder
            .spawn(move || {
                server_worker(
                    &mut server,
                    join_handler,
                    options,
                    rx,
                    join_stats,
                    join_running,
                    join_gauge,
                );
            })
            .map_err(|err| map_spawn_error(err, "spawn server worker"))?;
        Ok(Self {
            cmd_tx: tx,
            join: Mutex::new(Some(join)),
            stats,
            running,
            gauge,
            max_send_queue,
        })
    }

    /// Асинхронная отправка с политикой overwrite-oldest: при переполнении
    /// очереди вытесняется самое старое обычное сообщение, при переполнении
    /// кольца -- самые старые непрочитанные (`on_overflow`).
    pub fn send(&self, data: &[u8]) -> Result<()> {
        enqueue(
            &self.cmd_tx,
            &self.running,
            &self.gauge,
            data,
            false,
            self.max_send_queue,
        )
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
        enqueue(
            &self.cmd_tx,
            &self.running,
            &self.gauge,
            data,
            true,
            self.max_send_queue,
        )
    }

    /// Консервативная оценка места под новое сообщение: свободное место
    /// кольца Server -> Client (снимок worker-а) минус всё принятое, но ещё
    /// не записанное; `messages` дополнительно ограничено свободным местом
    /// очереди. Если `free_space().fits(n)`, то `try_send` payload-а длины
    /// `n` будет принят и записан в кольцо без ожидания читателя (при одном
    /// отправляющем потоке). Снимок кольца обновляется на каждой итерации
    /// worker-а (не реже `poll_timeout`), вне подключения -- `FreeSpace::ZERO`.
    #[must_use]
    pub fn free_space(&self) -> FreeSpace {
        self.gauge.estimate(self.max_send_queue.max(1))
    }

    /// Жив ли процесс пира по данным worker-а: `Some(true)` -- подключён и
    /// наблюдается через удерживаемый handle; `Some(false)` -- последнее
    /// отключение было `PeerDied` (до следующего подключения); `None` -- нет
    /// подключения или пир не наблюдается (старая версия / нет прав).
    /// Смерть пира worker замечает сразу (handle в наборе ожидания), но
    /// статус обновляется после доставки оставшихся в кольце сообщений.
    #[must_use]
    pub fn is_peer_alive(&self) -> Option<bool> {
        self.gauge.peer_alive()
    }

    /// PID наблюдаемого пира (после `PeerDied` -- PID умершего процесса).
    #[must_use]
    pub fn peer_pid(&self) -> Option<u32> {
        self.gauge.peer_pid()
    }

    pub fn stop(&self) {
        let _ = self.cmd_tx.send(WorkerCommand::Shutdown);
    }

    pub fn stats(&self) -> AutoStatsSnapshot {
        self.stats.snapshot()
    }
}

impl Drop for AutoServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        let _ = self.cmd_tx.send(WorkerCommand::Shutdown);
        if let Some(handle) = self.join.lock().unwrap().take() {
            join_unless_self(handle);
        }
    }
}

fn server_worker(
    server: &mut SharedServer,
    handler: Arc<dyn AutoHandler>,
    options: AutoOptions,
    cmd_rx: Receiver<WorkerCommand>,
    stats: Arc<AutoStats>,
    running: Arc<AtomicBool>,
    gauge: Arc<ChannelState>,
) {
    let mut send_queue = SendQueue::new();
    let mut buffer = Vec::with_capacity(MAX_MESSAGE_SIZE);
    // Anonymous-сервер не имеет событий, а auto-mode построен на ожидании
    // событий -- сообщаем об этом через handler и выходим, а не паникуем в
    // worker-потоке (аудит 2026-07-28: `expect()` в библиотечном коде).
    // Практически недостижимо: AutoServer::start всегда поднимает named-сервер.
    let Some(server_events) = server.events() else {
        handler.on_error(ShmError::InvalidConfig(
            "anonymous server is not supported in auto-mode",
        ));
        return;
    };
    let base_handles = [
        server_events.disconnect.raw_handle(),
        server_events.c2s.data.raw_handle(),
        server_events.s2c.space.raw_handle(),
    ];

    let mut connected = false;

    while running.load(Ordering::Acquire) {
        if !connected {
            // Вне подключения писать некуда: оценка места -- ноль.
            gauge.publish_ring(FreeSpace::ZERO);
            match server.wait_for_client(Some(options.poll_timeout)) {
                Ok(_) => {
                    connected = true;
                    gauge.peer_connected(server.peer_pid());
                    handler.on_connect();
                }
                Err(ShmError::Timeout) => {
                    drain_commands(
                        &mut send_queue,
                        &cmd_rx,
                        &options,
                        &running,
                        &handler,
                        &gauge,
                        ChannelKind::ServerToClient,
                    );
                    continue;
                }
                Err(err) => {
                    handler.on_error(err.clone());
                    drain_commands(
                        &mut send_queue,
                        &cmd_rx,
                        &options,
                        &running,
                        &handler,
                        &gauge,
                        ChannelKind::ServerToClient,
                    );
                    continue;
                }
            }
        }

        drain_commands(
            &mut send_queue,
            &cmd_rx,
            &options,
            &running,
            &handler,
            &gauge,
            ChannelKind::ServerToClient,
        );

        if !connected {
            continue;
        }

        process_send_queue(
            server,
            &mut send_queue,
            &handler,
            &stats,
            &gauge,
            ChannelKind::ServerToClient,
        );

        let outcome = process_receive_queue(
            server,
            &handler,
            &stats,
            &mut buffer,
            options.recv_batch,
            ChannelKind::ClientToServer,
        );
        if outcome.fatal {
            notify_disconnect(&handler, &gauge, DisconnectReason::Error);
            server.mark_disconnected();
            connected = false;
            continue;
        }
        if outcome.more_pending {
            // Ещё есть данные — не блокируемся, сразу следующий проход.
            continue;
        }

        let (handles, count) = wait_set(base_handles, server.peer_wait_handle());
        match win::wait_any(&handles[..count], Some(options.poll_timeout)) {
            Ok(Some(0)) => {
                notify_disconnect(&handler, &gauge, DisconnectReason::Graceful);
                server.mark_disconnected();
                connected = false;
            }
            Ok(Some(1)) => {
                // data available, loop will read
            }
            Ok(Some(2)) => {
                handler.on_space_available(ChannelKind::ServerToClient);
            }
            Ok(Some(PEER_WAIT_INDEX)) => {
                drain_after_peer_death(
                    server,
                    &handler,
                    &stats,
                    &mut buffer,
                    options.recv_batch,
                    ChannelKind::ClientToServer,
                );
                notify_disconnect(&handler, &gauge, DisconnectReason::PeerDied);
                server.mark_disconnected();
                connected = false;
            }
            Ok(Some(_)) => {}
            Ok(None) => {}
            Err(err) => {
                handler.on_error(err.clone());
                notify_disconnect(&handler, &gauge, DisconnectReason::Error);
                server.mark_disconnected();
                connected = false;
            }
        }
    }
}

/// Индекс handle процесса пира в наборе ожидания worker-а: после трёх
/// событий канала (DISCONNECT, DATA, SPACE). NT при одновременном сигнале
/// возвращает наименьший индекс, поэтому штатный DISCONNECT и последние
/// данные пира имеют приоритет над «процесс завершился».
const PEER_WAIT_INDEX: usize = 3;

/// Набор ожидания worker-а: события канала + (если пир наблюдается) handle
/// его процесса.
const fn wait_set(base: [isize; 3], peer: Option<isize>) -> ([isize; 4], usize) {
    match peer {
        Some(peer) => ([base[0], base[1], base[2], peer], 4),
        None => ([base[0], base[1], base[2], 0], 3),
    }
}

/// Сообщить об отключении: сначала публикуем статус пира (его видят
/// `is_peer_alive()`), затем callback с причиной.
fn notify_disconnect(
    handler: &Arc<dyn AutoHandler>,
    state: &ChannelState,
    reason: DisconnectReason,
) {
    state.peer_disconnected(reason);
    handler.on_disconnect_reason(reason);
}

/// Пир мёртв -- новых данных не будет, но всё, что он успел записать до
/// смерти, доставляем ДО `on_disconnect_reason(PeerDied)`.
fn drain_after_peer_death<R: ReceiveEndpoint>(
    endpoint: &R,
    handler: &Arc<dyn AutoHandler>,
    stats: &Arc<AutoStats>,
    buffer: &mut Vec<u8>,
    batch: usize,
    direction: ChannelKind,
) {
    loop {
        let outcome = process_receive_queue(endpoint, handler, stats, buffer, batch, direction);
        if outcome.fatal || !outcome.more_pending {
            break;
        }
    }
}

#[derive(Debug)]
pub struct AutoClient {
    cmd_tx: Sender<WorkerCommand>,
    join: Mutex<Option<JoinHandle<()>>>,
    stats: Arc<AutoStats>,
    running: Arc<AtomicBool>,
    gauge: Arc<ChannelState>,
    max_send_queue: usize,
}

impl AutoClient {
    pub fn connect(
        name: &str,
        handler: Arc<dyn AutoHandler>,
        options: AutoOptions,
    ) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let stats = Arc::new(AutoStats::default());
        let running = Arc::new(AtomicBool::new(true));
        let gauge = Arc::new(ChannelState::default());
        let max_send_queue = options.max_send_queue;
        let join_gauge = gauge.clone();
        let join_stats = stats.clone();
        let join_running = running.clone();
        let handler_clone = handler.clone();
        let name_str = name.to_owned();

        // Имя потока -- только в debug (короткий непрозрачный тег), как у
        // AutoServer: в release имя потока не должно светить ни библиотеку,
        // ни имя канала в Process Explorer / Process Hacker.
        #[cfg_attr(not(debug_assertions), allow(unused_mut))]
        let mut builder = thread::Builder::new();
        #[cfg(debug_assertions)]
        {
            builder = builder.name(format!("xsc-{name}"));
        }
        let join = builder
            .spawn(move || {
                client_worker(
                    &name_str,
                    handler_clone,
                    options,
                    rx,
                    join_stats,
                    join_running,
                    join_gauge,
                );
            })
            .map_err(|err| map_spawn_error(err, "spawn client worker"))?;

        Ok(Self {
            cmd_tx: tx,
            join: Mutex::new(Some(join)),
            stats,
            running,
            gauge,
            max_send_queue,
        })
    }

    /// Асинхронная отправка с политикой overwrite-oldest
    /// (см. `AutoServer::send`).
    pub fn send(&self, data: &[u8]) -> Result<()> {
        enqueue(
            &self.cmd_tx,
            &self.running,
            &self.gauge,
            data,
            false,
            self.max_send_queue,
        )
    }

    /// Асинхронная отправка без потерь с backpressure (`Err(QueueFull)`,
    /// когда в очереди `max_send_queue` непереданных сообщений). Полная
    /// семантика -- `AutoServer::try_send`.
    pub fn try_send(&self, data: &[u8]) -> Result<()> {
        enqueue(
            &self.cmd_tx,
            &self.running,
            &self.gauge,
            data,
            true,
            self.max_send_queue,
        )
    }

    /// Консервативная оценка места под новое сообщение в направлении
    /// Client -> Server (см. `AutoServer::free_space`).
    #[must_use]
    pub fn free_space(&self) -> FreeSpace {
        self.gauge.estimate(self.max_send_queue.max(1))
    }

    /// Жив ли процесс пира по данным worker-а: `Some(true)` -- подключён и
    /// наблюдается через удерживаемый handle; `Some(false)` -- последнее
    /// отключение было `PeerDied` (до следующего подключения); `None` -- нет
    /// подключения или пир не наблюдается (старая версия / нет прав).
    /// Смерть пира worker замечает сразу (handle в наборе ожидания), но
    /// статус обновляется после доставки оставшихся в кольце сообщений.
    #[must_use]
    pub fn is_peer_alive(&self) -> Option<bool> {
        self.gauge.peer_alive()
    }

    /// PID наблюдаемого пира (после `PeerDied` -- PID умершего процесса).
    #[must_use]
    pub fn peer_pid(&self) -> Option<u32> {
        self.gauge.peer_pid()
    }

    pub fn stop(&self) {
        let _ = self.cmd_tx.send(WorkerCommand::Shutdown);
    }

    pub fn stats(&self) -> AutoStatsSnapshot {
        self.stats.snapshot()
    }
}

impl Drop for AutoClient {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        let _ = self.cmd_tx.send(WorkerCommand::Shutdown);
        if let Some(handle) = self.join.lock().unwrap().take() {
            join_unless_self(handle);
        }
    }
}

fn client_worker(
    name: &str,
    handler: Arc<dyn AutoHandler>,
    options: AutoOptions,
    cmd_rx: Receiver<WorkerCommand>,
    stats: Arc<AutoStats>,
    running: Arc<AtomicBool>,
    gauge: Arc<ChannelState>,
) {
    let mut send_queue = SendQueue::new();
    let mut buffer = Vec::with_capacity(MAX_MESSAGE_SIZE);

    while running.load(Ordering::Acquire) {
        let mut client = match SharedClient::connect(name, options.connect_timeout) {
            Ok(client) => client,
            Err(err) => {
                handler.on_error(err.clone());
                if !wait_delay(&running, options.reconnect_delay) {
                    break;
                }
                continue;
            }
        };

        gauge.peer_connected(client.peer_pid());
        handler.on_connect();
        // SharedClient всегда использует named events (не anonymous)
        let client_events = client.events();
        let base_handles = [
            client_events.disconnect.raw_handle(),
            client_events.s2c.data.raw_handle(),
            client_events.c2s.space.raw_handle(),
        ];

        loop {
            if !running.load(Ordering::Acquire) {
                break;
            }

            drain_commands(
                &mut send_queue,
                &cmd_rx,
                &options,
                &running,
                &handler,
                &gauge,
                ChannelKind::ClientToServer,
            );
            process_send_queue(
                &client,
                &mut send_queue,
                &handler,
                &stats,
                &gauge,
                ChannelKind::ClientToServer,
            );
            let outcome = process_receive_queue(
                &client,
                &handler,
                &stats,
                &mut buffer,
                options.recv_batch,
                ChannelKind::ServerToClient,
            );
            if outcome.fatal {
                notify_disconnect(&handler, &gauge, DisconnectReason::Error);
                client.mark_disconnected();
                break;
            }
            if outcome.more_pending {
                continue;
            }

            let (handles, count) = wait_set(base_handles, client.peer_wait_handle());
            match win::wait_any(&handles[..count], Some(options.poll_timeout)) {
                Ok(Some(0)) => {
                    notify_disconnect(&handler, &gauge, DisconnectReason::Graceful);
                    client.mark_disconnected();
                    break;
                }
                Ok(Some(1)) => {}
                Ok(Some(2)) => handler.on_space_available(ChannelKind::ClientToServer),
                Ok(Some(PEER_WAIT_INDEX)) => {
                    drain_after_peer_death(
                        &client,
                        &handler,
                        &stats,
                        &mut buffer,
                        options.recv_batch,
                        ChannelKind::ServerToClient,
                    );
                    notify_disconnect(&handler, &gauge, DisconnectReason::PeerDied);
                    client.mark_disconnected();
                    break;
                }
                Ok(Some(_)) => {}
                Ok(None) => {}
                Err(err) => {
                    handler.on_error(err.clone());
                    notify_disconnect(&handler, &gauge, DisconnectReason::Error);
                    client.mark_disconnected();
                    break;
                }
            }
        }

        // Соединение потеряно: писать некуда, оценка места -- ноль.
        gauge.publish_ring(FreeSpace::ZERO);
        if !wait_delay(&running, options.reconnect_delay) {
            break;
        }
    }
}

/// Переносит команды из mpsc-канала в очередь отправки.
///
/// При переполнении (`max_send_queue`) обычным `send` вытесняется САМОЕ
/// СТАРОЕ обычное (не lossless) сообщение, а вызывающему сообщается об этом
/// через `AutoHandler::on_overflow` -- раньше потеря была полностью молчаливой
/// (аудит 2026-07-28), в отличие от симметричного
/// `MultiClientHandler::on_overflow`. Сообщения `try_send` не вытесняются
/// никогда: если вся очередь из них, выбрасывается само новое сообщение
/// `send` (тоже с `on_overflow`). Сообщения `try_send` принимаются всегда --
/// их число уже ограничено `max_send_queue` на входе (`ChannelState::try_acquire`).
fn drain_commands(
    queue: &mut SendQueue,
    rx: &Receiver<WorkerCommand>,
    options: &AutoOptions,
    running: &Arc<AtomicBool>,
    handler: &Arc<dyn AutoHandler>,
    gauge: &ChannelState,
    direction: ChannelKind,
) {
    while let Ok(cmd) = rx.try_recv() {
        match cmd {
            WorkerCommand::Send(msg) => {
                if !msg.lossless && queue.len() >= options.max_send_queue {
                    match queue.iter().position(|m| !m.lossless) {
                        Some(pos) => {
                            if let Some(evicted) = queue.remove(pos) {
                                gauge.release(evicted.frame());
                            }
                            queue.push_back(msg);
                        }
                        None => gauge.release(msg.frame()),
                    }
                    handler.on_overflow(direction, 1);
                } else {
                    queue.push_back(msg);
                }
            }
            WorkerCommand::Shutdown => {
                running.store(false, Ordering::Release);
            }
        }
    }
}

/// Пишет очередь в кольцо, пока пишется.
///
/// Обычные сообщения -- `write_message` (overwrite-oldest), lossless --
/// `try_write_message`. Если lossless-сообщение в голове очереди не влезает,
/// worker оставляет в кольце заявку (`arm_space_waiter`) и ОДИН раз
/// перепроверяет -- после этого либо запись прошла, либо читатель гарантированно
/// увидит заявку и просигналит `SPACE`, который будит `wait_any` worker-а.
/// Порядок FIFO сохраняется: пока голова ждёт места, остальные тоже ждут.
fn process_send_queue<E>(
    endpoint: &E,
    queue: &mut SendQueue,
    handler: &Arc<dyn AutoHandler>,
    stats: &Arc<AutoStats>,
    gauge: &ChannelState,
    direction: ChannelKind,
) where
    E: SendEndpoint,
{
    // `armed` -- заявка поставлена в ЭТОМ проходе; `cleared` -- заявка,
    // возможно оставшаяся с прошлого прохода (читатель её не снял: места
    // было мало или он старой версии), уже снята. Снимаем на первой же
    // успешной записи -- одна запись в заголовок за проход.
    let mut armed = false;
    let mut cleared = false;
    while let Some(msg) = queue.front() {
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
                let frame = msg.frame();
                queue.pop_front();
                // Сначала снимок кольца (уже с этим сообщением), потом
                // списание из pending -- порядок описан у `ChannelState`.
                gauge.publish_ring(endpoint.free_space());
                gauge.release(frame);
                stats.sent_messages.fetch_add(1, Ordering::Relaxed);
                if outcome.overwritten > 0 {
                    stats
                        .send_overflows
                        .fetch_add(outcome.overwritten as u64, Ordering::Relaxed);
                    handler.on_overflow(direction, outcome.overwritten);
                }
            }
            Err(ShmError::QueueFull) if msg.lossless && !armed => {
                endpoint.arm_space_waiter(msg.frame() as u32);
                armed = true;
            }
            Err(ShmError::QueueFull) => break,
            Err(err @ (ShmError::MessageTooSmall | ShmError::MessageTooLarge)) => {
                // Сообщение невалидно навсегда: раньше оно возвращалось в
                // голову очереди и блокировало канал до конца жизни worker-а.
                handler.on_error(err);
                if let Some(bad) = queue.pop_front() {
                    gauge.release(bad.frame());
                }
            }
            Err(err) => {
                handler.on_error(err);
                break;
            }
        }
    }
    gauge.publish_ring(endpoint.free_space());
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
    let mut drained = false;
    for _ in 0..batch.max(1) {
        match endpoint.read(buffer) {
            Ok(len) => {
                stats.received_messages.fetch_add(1, Ordering::Relaxed);
                handler.on_message(direction, &buffer[..len]);
            }
            Err(ShmError::QueueEmpty) => {
                drained = true;
                break;
            }
            Err(ShmError::NotConnected) | Err(ShmError::NotReady) | Err(ShmError::Timeout) => {
                drained = true;
                break;
            }
            Err(ref err @ ShmError::Corrupted) => {
                handler.on_error(err.clone());
                return ReceiveOutcome {
                    fatal: true,
                    more_pending: false,
                };
            }
            Err(err) => {
                handler.on_error(err);
                drained = true;
                break;
            }
        }
    }
    ReceiveOutcome {
        fatal: false,
        more_pending: !drained,
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

        // Даём клиенту время реально подключиться, прежде чем отключать.
        thread::sleep(Duration::from_millis(200));
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
        fn accept(&self, data: &[u8]) -> Result<WriteOutcome> {
            frame_len(data.len())?;
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
            frame_len(data.len())?;
            if self.room.get() == 0 {
                return Err(ShmError::QueueFull);
            }
            self.accept(data)
        }
        fn free_space(&self) -> FreeSpace {
            FreeSpace {
                bytes: self.room.get() * 1000,
                messages: self.room.get() as u32,
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

    fn data_of(queue: &SendQueue) -> Vec<Vec<u8>> {
        queue.iter().map(|m| m.data.clone()).collect()
    }

    /// `try_send` ограничен `max_send_queue` на входе, `send` -- нет; при
    /// переполнении `send` вытесняет только обычные сообщения, а если в
    /// очереди одни lossless -- выбрасывается само новое сообщение `send`.
    #[test]
    fn queue_policy_never_evicts_lossless() {
        let (tx, rx) = mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let gauge = ChannelState::default();
        let counters = Arc::new(Counters::default());
        let handler: Arc<dyn AutoHandler> = counters.clone();
        let options = AutoOptions {
            max_send_queue: 2,
            ..AutoOptions::default()
        };
        let dir = ChannelKind::ClientToServer;

        enqueue(&tx, &running, &gauge, b"L1", true, 2).unwrap();
        enqueue(&tx, &running, &gauge, b"L2", true, 2).unwrap();
        assert_eq!(
            enqueue(&tx, &running, &gauge, b"L3", true, 2),
            Err(ShmError::QueueFull)
        );
        assert_eq!(
            enqueue(&tx, &running, &gauge, b"x", true, 2),
            Err(ShmError::MessageTooSmall),
            "длина lossless-сообщения проверяется синхронно"
        );
        enqueue(&tx, &running, &gauge, b"S1", false, 2).unwrap();

        let mut queue = SendQueue::new();
        drain_commands(&mut queue, &rx, &options, &running, &handler, &gauge, dir);
        assert_eq!(data_of(&queue), [b"L1".to_vec(), b"L2".to_vec()]);
        assert_eq!(counters.overflows.load(Ordering::Relaxed), 1);
        assert_eq!(gauge.pending_msgs.load(Ordering::Acquire), 2);
        assert_eq!(gauge.pending_bytes.load(Ordering::Acquire), 2 * (4 + 2));

        // Смешанная очередь: вытесняется самое старое ОБЫЧНОЕ сообщение.
        let mut queue = SendQueue::new();
        queue.push_back(Outgoing {
            data: b"S0".to_vec(),
            lossless: false,
        });
        queue.push_back(Outgoing {
            data: b"L0".to_vec(),
            lossless: true,
        });
        let gauge = ChannelState::default();
        gauge.acquire(6);
        gauge.acquire(6);
        enqueue(&tx, &running, &gauge, b"S2", false, 2).unwrap();
        drain_commands(&mut queue, &rx, &options, &running, &handler, &gauge, dir);
        assert_eq!(data_of(&queue), [b"L0".to_vec(), b"S2".to_vec()]);
        assert_eq!(gauge.pending_msgs.load(Ordering::Acquire), 2);
    }

    /// Голова-lossless не влезает: worker ставит заявку ровно один раз,
    /// ничего не теряет и не обгоняет (FIFO); освободилось место --
    /// очередь уходит целиком, заявка снимается, учёт pending обнуляется.
    #[test]
    fn blocked_lossless_head_keeps_fifo_and_arms_waiter() {
        let (tx, rx) = mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let gauge = ChannelState::default();
        let counters = Arc::new(Counters::default());
        let handler: Arc<dyn AutoHandler> = counters.clone();
        let stats = Arc::new(AutoStats::default());
        let options = AutoOptions::default();
        let dir = ChannelKind::ClientToServer;
        for i in 0..5u8 {
            enqueue(&tx, &running, &gauge, &[i, i, i], true, 16).unwrap();
        }
        let mut queue = SendQueue::new();
        drain_commands(&mut queue, &rx, &options, &running, &handler, &gauge, dir);

        let ep = MockEndpoint::new(2);
        process_send_queue(&ep, &mut queue, &handler, &stats, &gauge, dir);
        assert_eq!(ep.written.borrow().len(), 2);
        assert_eq!(queue.len(), 3);
        assert_eq!(ep.arms.get(), 1, "заявка -- один раз за проход");
        assert_eq!(ep.armed.get(), 4 + 3);
        assert_eq!(gauge.pending_msgs.load(Ordering::Acquire), 3);
        // Оценка места: кольцо пусто (room=0), а 3 сообщения ещё ждут.
        assert_eq!(gauge.estimate(16), FreeSpace::ZERO);

        ep.room.set(10);
        process_send_queue(&ep, &mut queue, &handler, &stats, &gauge, dir);
        assert!(queue.is_empty());
        assert_eq!(ep.armed.get(), 0, "успешная запись снимает заявку");
        let written = ep.written.borrow();
        for (i, msg) in written.iter().enumerate() {
            assert_eq!(msg, &vec![i as u8; 3], "порядок нарушен");
        }
        assert_eq!(gauge.pending_msgs.load(Ordering::Acquire), 0);
        assert_eq!(gauge.pending_bytes.load(Ordering::Acquire), 0);
        assert_eq!(stats.sent_messages.load(Ordering::Relaxed), 5);
        assert_eq!(counters.overflows.load(Ordering::Relaxed), 0);
    }

    /// Регрессия: невалидное сообщение `send` раньше возвращалось в голову
    /// очереди и навсегда блокировало канал. Теперь -- `on_error` и дальше.
    #[test]
    fn invalid_message_does_not_wedge_queue() {
        let (tx, rx) = mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let gauge = ChannelState::default();
        let counters = Arc::new(Counters::default());
        let handler: Arc<dyn AutoHandler> = counters.clone();
        let stats = Arc::new(AutoStats::default());
        let dir = ChannelKind::ServerToClient;
        enqueue(&tx, &running, &gauge, b"x", false, 16).unwrap();
        enqueue(&tx, &running, &gauge, b"ok", false, 16).unwrap();
        let mut queue = SendQueue::new();
        drain_commands(
            &mut queue,
            &rx,
            &AutoOptions::default(),
            &running,
            &handler,
            &gauge,
            dir,
        );
        let ep = MockEndpoint::new(10);
        process_send_queue(&ep, &mut queue, &handler, &stats, &gauge, dir);
        assert!(queue.is_empty());
        assert_eq!(*ep.written.borrow(), [b"ok".to_vec()]);
        assert_eq!(counters.errors.load(Ordering::Relaxed), 1);
        assert_eq!(gauge.pending_msgs.load(Ordering::Acquire), 0);
    }

    /// Оценка места консервативна: вычитает ещё не записанное и ограничена
    /// свободным местом очереди.
    #[test]
    fn gauge_estimate_is_conservative() {
        let gauge = ChannelState::default();
        assert_eq!(gauge.estimate(8), FreeSpace::ZERO, "до подключения");
        gauge.publish_ring(FreeSpace {
            bytes: 10_000,
            messages: 100,
        });
        assert_eq!(
            gauge.estimate(8),
            FreeSpace {
                bytes: 10_000,
                messages: 8
            }
        );
        gauge.acquire(4 + 996);
        gauge.acquire(4 + 996);
        let est = gauge.estimate(8);
        assert_eq!(est.bytes, 8_000);
        assert_eq!(est.messages, 6);
        assert!(!gauge.try_acquire(10, 2), "очередь 2/2");
        gauge.release(1000);
        assert!(gauge.try_acquire(10, 2));
    }
}
