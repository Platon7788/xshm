//! Мультиклиентный сервер для xShm.
//!
//! Позволяет одному серверу обслуживать до N клиентов одновременно.
//! Клиент САМ захватывает свободный слот (lock-free claim) — без
//! централизованного lobby.
//!
//! # Архитектура (конкурентный захват слотов)
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                      MultiServer                            │
//! ├─────────────────────────────────────────────────────────────┤
//! │  Нет lobby. N независимых сегментов-слотов BaseName_0..N-1.  │
//! │  Клиент пробегает слоты и атомарно захватывает свободный:    │
//! │    CAS reserved[0]: CLAIM_FREE -> token (на сегменте слота)  │
//! │  Победитель CAS делает обычный handshake SharedClient.       │
//! ├─────────────────────────────────────────────────────────────┤
//! │  Slot 0: SharedServer "BaseName_0" ←→ Client A  (claim=tA)  │
//! │  Slot 1: SharedServer "BaseName_1" ←→ Client B  (claim=tB)  │
//! │  Slot 2: (свободен, claim=CLAIM_FREE)                       │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! N клиентов подключаются ПОЛНОСТЬЮ КОНКУРЕНТНО: CAS на разной памяти,
//! без общего состояния, без coalescing событий, без коллизий слотов.
//!
//! # Событийная модель (0.9)
//!
//! Ни сервер, ни клиент не просыпаются в простое (`poll_timeout = None` по
//! умолчанию). Worker сервера (один на каждые `SLOTS_PER_WORKER` слотов)
//! спит в `NtWaitForMultipleObjects` по набору: у свободного слота --
//! `C2S_CONNECT_REQ`, у подключённого -- `S2C_DISCONNECT`, `C2S_DATA` и handle
//! процесса клиента; последним -- безымянное событие `wake` группы
//! (`disconnect_client`, `stop`). Таймаут ожидания -- ровно до ближайшего
//! ДЕДЛАЙНА (протухший захват слота, разовая проверка брошенного
//! рукопожатия), а не тик. Клиент спит по `[S2C_DISCONNECT, S2C_DATA,
//! процесс сервера, wake]`; `send`/`stop` будят его своим событием.

use std::collections::VecDeque;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::client::{Interrupt, SharedClient};
use crate::constants::{
    CLAIM_FREE, EVENT_CONNECT_REQ_SUFFIX, HANDSHAKE_SERVER_READY, MAX_MESSAGE_SIZE,
    RESERVED_CLAIM_INDEX, RESERVED_OWNER_PID_INDEX, SHARED_MAGIC, SHARED_VERSION, SLOT_ID_NO_SLOT,
};
use crate::error::{DisconnectReason, Result, ShmError};
use crate::naming::{Direction, event_name, mapping_name};
use crate::ring::FreeSpace;
use crate::server::SharedServer;
use crate::shared::SharedView;
use crate::wait_delay_or;
use crate::win::{self, EventHandle, Mapping, ProcessWatch};

/// Максимальное количество клиентов по умолчанию
pub const DEFAULT_MAX_CLIENTS: u32 = 20;

/// Жёсткий предел числа слотов. Слоты делятся между worker-потоками по
/// `SLOTS_PER_WORKER` (предел `NtWaitForMultipleObjects` -- 64 handle на
/// одно ожидание), так что 31 слот -- два worker-а.
pub const MAX_MULTI_CLIENTS: u32 = 31;

/// Слотов на один worker-поток: подключённый слот кладёт в набор ожидания до
/// 3 handle (`S2C_DISCONNECT`, `C2S_DATA`, процесс клиента), плюс одно
/// событие `wake` группы: 3 * 21 + 1 = 64 = `MAXIMUM_WAIT_OBJECTS`.
const SLOTS_PER_WORKER: u32 = 21;

/// Таймаут, после которого «зависшая» резервация слота освобождается
/// (клиент захватил claim, но не подключился к слоту).
const RESERVE_TIMEOUT: Duration = Duration::from_secs(10);

/// Гарантированный запас между эффективным клиентским `slot_timeout` и
/// серверным `RESERVE_TIMEOUT`. Клиент обязан сам отвалиться по таймауту и
/// освободить claim РАНЬШЕ, чем сервер сочтёт его протухшим и заберёт claim
/// силой — иначе сервер может отнять слот у ещё легитимно ожидающего
/// клиента (аудит 2026-07-10). `slot_timeout` из `MultiClientOptions`
/// клампится этим запасом в `client_worker`, независимо от того, что задал
/// вызывающий (значение в `MultiClientOptions` сверху не ограничено).
const RESERVE_SAFETY_MARGIN: Duration = Duration::from_secs(2);

/// Разовая (не периодическая) проверка «брошенного рукопожатия» после
/// handshake: клиент, чьё ожидание `S2C_CONNECT` истекло ровно в момент
/// ответа сервера, откатывает заявку и снимает claim, не сигналя
/// `DISCONNECT`. Снимает он его сразу после таймаута, поэтому одной проверки
/// через этот интервал достаточно.
const HANDSHAKE_VERIFY_DELAY: Duration = RESERVE_SAFETY_MARGIN;

/// Ограничение частоты разовой проверки живости по PID для клиента без
/// наблюдаемого процесса (старая версия + процесс не открылся). Это не
/// таймер: проверка делается только при пробуждении worker-а по другой
/// причине и не чаще этого периода (`NtOpenProcess` дороже atomic load).
const LIVENESS_CHECK_INTERVAL: Duration = Duration::from_secs(3);

/// Callback-интерфейс для обработки событий мультиклиентного сервера
pub trait MultiHandler: Send + Sync + 'static {
    /// Вызывается при подключении нового клиента
    fn on_client_connect(&self, client_id: u32);

    /// Вызывается при отключении клиента
    fn on_client_disconnect(&self, client_id: u32);

    /// Отключение клиента с причиной (0.9). По умолчанию вызывает
    /// `on_client_disconnect`; сервер вызывает ТОЛЬКО этот метод.
    /// `PeerDied` -- процесс клиента завершился без штатного отключения
    /// (удерживаемый handle процесса), `Local` -- `disconnect_client`.
    fn on_client_disconnect_reason(&self, client_id: u32, reason: DisconnectReason) {
        let _ = reason;
        self.on_client_disconnect(client_id);
    }

    /// Вызывается при получении сообщения от клиента
    fn on_message(&self, client_id: u32, data: &[u8]);

    /// Вызывается при ошибке (client_id = None для общих ошибок)
    fn on_error(&self, client_id: Option<u32>, err: ShmError) {
        let _ = (client_id, err);
    }
}

/// Callback-интерфейс для MultiClient
pub trait MultiClientHandler: Send + Sync + 'static {
    /// Вызывается при успешном подключении (slot_id — назначенный слот)
    fn on_connect(&self, slot_id: u32);

    /// Вызывается при отключении
    fn on_disconnect(&self);

    /// Отключение с причиной (0.9). По умолчанию вызывает `on_disconnect`;
    /// worker вызывает ТОЛЬКО этот метод. `PeerDied` -- процесс сервера
    /// завершился без штатного отключения.
    fn on_disconnect_reason(&self, reason: DisconnectReason) {
        let _ = reason;
        self.on_disconnect();
    }

    /// Вызывается при получении сообщения от сервера
    fn on_message(&self, data: &[u8]);

    /// Вызывается при переполнении внутренней send-очереди (`max_send_queue`):
    /// `dropped` — сколько старых неотправленных сообщений вытеснено новыми.
    fn on_overflow(&self, _dropped: u32) {}

    /// Вызывается при ошибке
    fn on_error(&self, err: ShmError) {
        let _ = err;
    }
}

/// Опции для MultiServer
#[derive(Clone, Debug)]
pub struct MultiOptions {
    /// Максимальное количество одновременных клиентов
    pub max_clients: u32,
    /// Страховочный таймаут ожидания worker-а. `None` (по умолчанию, 0.9+)
    /// -- только события и дедлайны: worker спит в ядре до подключения,
    /// данных, отключения, смерти клиента, `disconnect_client`/`stop` или
    /// ближайшего дедлайна освобождения слота -- ни одного пробуждения в
    /// простое. `Some(t)` -- дополнительно просыпаться не реже `t` (для
    /// корректности не нужно). До 0.9 -- `Duration` (50 мс, опрос).
    pub poll_timeout: Option<Duration>,
    /// Количество сообщений для обработки за один цикл
    pub recv_batch: usize,
}

impl Default for MultiOptions {
    fn default() -> Self {
        Self {
            max_clients: DEFAULT_MAX_CLIENTS,
            poll_timeout: None,
            recv_batch: 32,
        }
    }
}

/// Опции для MultiClient
#[derive(Clone, Debug)]
pub struct MultiClientOptions {
    /// Таймаут подключения к слоту
    pub slot_timeout: Duration,
    /// Страховочный таймаут ожидания подключённого клиента. `None` (по
    /// умолчанию, 0.9+) -- только события (данные, отключение, смерть
    /// сервера, `send`/`stop`). До 0.9 -- `Duration` (50 мс, опрос).
    pub poll_timeout: Option<Duration>,
    /// Пауза перед новой попыткой захвата слота (0.9): сервер не запущен,
    /// свободных слотов нет, подключение не удалось или разорвано. Это
    /// единственный периодический путь клиента -- только пока он НЕ
    /// подключён (как `AutoOptions::reconnect_delay`); остановка прерывает
    /// паузу сразу. До 0.9 роль паузы играл `poll_timeout` (50 мс).
    pub retry_delay: Duration,
    /// Количество сообщений за один цикл
    pub recv_batch: usize,
    /// Максимум неотправленных сообщений во внутренней очереди перед сбросом
    /// самого старого (overwrite-семантика, как у `AutoOptions.max_send_queue`).
    /// Без этого предела очередь росла бы неограниченно, если пир завис/тормозит.
    pub max_send_queue: usize,
}

impl Default for MultiClientOptions {
    fn default() -> Self {
        Self {
            slot_timeout: Duration::from_secs(5),
            poll_timeout: None,
            retry_delay: Duration::from_millis(250),
            recv_batch: 32,
            max_send_queue: 256,
        }
    }
}

/// Состояние одного клиентского слота
struct ClientSlot {
    id: u32,
    server: SharedServer,
    connected: bool,
    /// Не подключён: чужой claim (token) и момент, когда сервер ВПЕРВЫЕ его
    /// увидел -- дедлайн освобождения = момент + `reserve_timeout`. Token
    /// хранится, чтобы новый захват не унаследовал возраст прежнего.
    claim_seen: Option<(u32, Instant)>,
    /// Подключён: разовый дедлайн проверки брошенного рукопожатия.
    verify_at: Option<Instant>,
    /// Подключён: claim (token) клиента на момент рукопожатия -- при его
    /// штатном уходе сервер снимает claim только CAS-ом с этим значением.
    session_claim: u32,
    /// Подключён клиент, не передавший PID в handshake (старая версия):
    /// наблюдение за процессом-владельцем claim-а (PID из
    /// `RESERVED_OWNER_PID_INDEX`) -- тоже handle в наборе ожидания.
    owner_watch: Option<ProcessWatch>,
    /// Подключён без наблюдаемого процесса: момент последней разовой
    /// проверки живости по PID (ограничение частоты, не таймер).
    last_liveness_check: Option<Instant>,
    /// Handle процессов отключённых клиентов, которые ещё могут лежать в
    /// наборе ожидания спящего worker-а: закрывать ожидаемый handle нельзя,
    /// worker очищает список после пробуждения.
    retired: Vec<ProcessWatch>,
}

impl ClientSlot {
    const fn new(id: u32, server: SharedServer) -> Self {
        Self {
            id,
            server,
            connected: false,
            claim_seen: None,
            verify_at: None,
            session_claim: CLAIM_FREE,
            owner_watch: None,
            last_liveness_check: None,
            retired: Vec::new(),
        }
    }

    /// Handle процесса клиента для набора ожидания: из handshake (0.8+) или
    /// процесс-владелец claim-а (клиент старой версии).
    fn peer_handle(&self) -> Option<isize> {
        self.server
            .peer_wait_handle()
            .or_else(|| self.owner_watch.as_ref().map(ProcessWatch::raw_handle))
    }

    /// PID наблюдаемого процесса клиента.
    fn watched_pid(&self) -> Option<u32> {
        self.server
            .peer_pid()
            .or_else(|| self.owner_watch.as_ref().map(ProcessWatch::pid))
    }

    /// Разорвать подключение слота на стороне сервера: состояния handshake
    /// -> IDLE, handle процессов -> в `retired` (закроет worker).
    fn reset_connection(&mut self) {
        self.connected = false;
        self.claim_seen = None;
        self.verify_at = None;
        self.last_liveness_check = None;
        if let Some(watch) = self.server.take_peer() {
            self.retired.push(watch);
        }
        if let Some(watch) = self.owner_watch.take() {
            self.retired.push(watch);
        }
        self.server.mark_disconnected();
    }

    fn claim(&self) -> u32 {
        self.server.view().control_block().reserved[RESERVED_CLAIM_INDEX].load(Ordering::Acquire)
    }
}

/// Итог прохода обслуживания слотов группы.
#[derive(Debug, Default)]
struct Sweep {
    /// Осиротевшие подключённые слоты: `(slot_id, ожидаемый claim, причина)`.
    orphaned: Vec<(u32, u32, DisconnectReason)>,
    /// Ближайший дедлайн (освобождение протухшего захвата, проверка
    /// рукопожатия) -- до него и ждёт worker.
    next_deadline: Option<Instant>,
}

impl Sweep {
    fn deadline(&mut self, at: Instant) {
        self.next_deadline = Some(self.next_deadline.map_or(at, |d| d.min(at)));
    }
}

/// Мультиклиентный сервер.
///
/// # Конкурентное распределение слотов (без централизованного lobby)
///
/// Сервер создаёт N независимых сегментов-слотов `base_name_0..N-1`. Клиент
/// сам выбирает слот: пробегает слоты и атомарно захватывает первый свободный
/// через `compare_exchange(CLAIM_FREE -> token)` на `reserved[RESERVED_CLAIM_INDEX]`
/// сегмента слота, затем выполняет обычный handshake `SharedClient`. Так N
/// клиентов подключаются ПОЛНОСТЬЮ КОНКУРЕНТНО, захватывая РАЗНЫЕ слоты на
/// РАЗНОЙ памяти — без общего состояния, без coalescing событий, без коллизий.
pub struct MultiServer {
    base_name: String,
    slots: RwLock<Vec<Mutex<ClientSlot>>>,
    max_clients: u32,
    running: Arc<AtomicBool>,
    /// `wake` каждой группы слотов (индекс = номер worker-а): безымянное
    /// автосбросное событие -- `disconnect_client` (набор ожидания меняется),
    /// `stop`/Drop.
    wakes: Vec<EventHandle>,
    worker_handles: Mutex<Vec<JoinHandle<()>>>,
    handler: Arc<dyn MultiHandler>,
    options: MultiOptions,
    /// Через сколько протухает захват слота без подключения.
    reserve_timeout: Duration,
    /// Число пробуждений worker-ов (возвратов из ожидания) -- доказательство
    /// отсутствия опроса в тестах.
    wakeups: AtomicU64,
}

impl MultiServer {
    /// Запуск мультиклиентного сервера
    pub fn start(
        base_name: &str,
        handler: Arc<dyn MultiHandler>,
        options: MultiOptions,
    ) -> Result<Arc<Self>> {
        Self::start_with(base_name, handler, options, RESERVE_TIMEOUT)
    }

    /// `start` с заданным таймаутом резервирования (тесты дедлайна).
    pub(crate) fn start_with(
        base_name: &str,
        handler: Arc<dyn MultiHandler>,
        options: MultiOptions,
        reserve_timeout: Duration,
    ) -> Result<Arc<Self>> {
        if options.max_clients == 0 || options.max_clients > MAX_MULTI_CLIENTS {
            return Err(ShmError::InvalidConfig("max_clients must be in 1..=31"));
        }

        // Создаём N независимых сегментов-слотов. Lobby не нужен — клиенты
        // захватывают слоты сами через атомарный claim (см. doc MultiServer).
        let mut slots = Vec::with_capacity(options.max_clients as usize);
        for slot_id in 0..options.max_clients {
            let server = SharedServer::start(&format!("{base_name}_{slot_id}"))?;
            slots.push(Mutex::new(ClientSlot::new(slot_id, server)));
        }

        let groups = options.max_clients.div_ceil(SLOTS_PER_WORKER);
        let wakes = (0..groups)
            .map(|_| EventHandle::create_unnamed(false))
            .collect::<Result<Vec<_>>>()?;

        let server = Arc::new(Self {
            base_name: base_name.to_owned(),
            slots: RwLock::new(slots),
            max_clients: options.max_clients,
            running: Arc::new(AtomicBool::new(true)),
            wakes,
            worker_handles: Mutex::new(Vec::new()),
            handler,
            options,
            reserve_timeout,
            wakeups: AtomicU64::new(0),
        });

        for group in 0..groups {
            let first = group * SLOTS_PER_WORKER;
            let range = first..(first + SLOTS_PER_WORKER).min(server.max_clients);
            let server_clone = server.clone();
            // Имя потока -- только в debug (короткий непрозрачный тег), чтобы в
            // release ни библиотека, ни имя канала не светились в списке потоков.
            #[cfg_attr(not(debug_assertions), allow(unused_mut))]
            let mut builder = thread::Builder::new();
            #[cfg(debug_assertions)]
            {
                builder = builder.name(format!("xsm{group}-{base_name}"));
            }
            let spawned = crate::thread_hook::spawn(builder, move || {
                server_clone.worker_loop(group as usize, range);
            });
            match spawned {
                Ok(handle) => server.worker_handles.lock().unwrap().push(handle),
                Err(e) => {
                    // Уже запущенные worker-ы держат клон Arc -- остановить их.
                    server.stop();
                    return Err(ShmError::WindowsError {
                        code: e.raw_os_error().unwrap_or(-1) as u32,
                        context: "spawn multi worker",
                    });
                }
            }
        }

        Ok(server)
    }

    /// Отправка сообщения конкретному клиенту
    pub fn send_to(&self, client_id: u32, data: &[u8]) -> Result<()> {
        let slots = self.slots.read().unwrap();
        let slot_mutex = slots
            .get(client_id as usize)
            .ok_or(ShmError::NotConnected)?;
        let slot = slot_mutex.lock().unwrap();

        if !slot.connected {
            return Err(ShmError::NotConnected);
        }

        slot.server.send_to_client(data)?;
        Ok(())
    }

    /// Отправка клиенту **без перезаписи** непрочитанных данных
    /// (`SharedServer::try_send_to_client`): `Err(QueueFull)`, если в кольце
    /// слота нет места; кольцо тогда не меняется.
    pub fn try_send_to(&self, client_id: u32, data: &[u8]) -> Result<()> {
        let slots = self.slots.read().unwrap();
        let slot_mutex = slots
            .get(client_id as usize)
            .ok_or(ShmError::NotConnected)?;
        let slot = slot_mutex.lock().unwrap();

        if !slot.connected {
            return Err(ShmError::NotConnected);
        }

        slot.server.try_send_to_client(data)?;
        Ok(())
    }

    /// Свободное место в кольце Server -> Client слота (точное значение:
    /// запись в слот идёт только под его mutex-ом, так что до следующего
    /// `send_to`/`try_send_to` это нижняя граница). `None` -- слота нет или
    /// клиент не подключён.
    #[must_use]
    pub fn free_space(&self, client_id: u32) -> Option<FreeSpace> {
        let slots = self.slots.read().unwrap();
        let slot = slots.get(client_id as usize)?.lock().unwrap();
        slot.connected.then(|| slot.server.free_space())
    }

    /// Отправка сообщения всем подключённым клиентам
    pub fn broadcast(&self, data: &[u8]) -> Result<u32> {
        let slots = self.slots.read().unwrap();
        let mut sent_count = 0u32;

        for slot_mutex in slots.iter() {
            let slot = slot_mutex.lock().unwrap();
            if slot.connected && slot.server.send_to_client(data).is_ok() {
                sent_count += 1;
            }
        }

        Ok(sent_count)
    }

    /// Принудительное отключение клиента (`on_client_disconnect_reason(Local)`).
    pub fn disconnect_client(&self, client_id: u32) -> Result<()> {
        let slots = self.slots.read().unwrap();
        let slot_mutex = slots
            .get(client_id as usize)
            .ok_or(ShmError::NotConnected)?;
        let mut slot = slot_mutex.lock().unwrap();

        if slot.connected {
            // Порядок: состояния handshake -> IDLE, потом `DISCONNECT` --
            // клиент, проснувшись, видит уже отключённый слот (так же
            // `wait_for_space` отличает настоящее отключение от устаревшего
            // сигнала). Ревизия 2: claim НЕ освобождаем -- его снимет сам
            // клиент, получив `DISCONNECT` (CAS token -> 0). Раньше слот
            // освобождался до того, как клиент проснулся: следующий клиент
            // успевал его захватить, и его `complete_handshake` сбрасывал
            // `DISCONNECT` прежнего -- два клиента на одном SPSC-кольце.
            // Мёртвого клиента освобождает дедлайн `reserve_timeout` (слот
            // не подключён, claim занят -- обычный протухший захват).
            slot.reset_connection();
            if let Some(events) = slot.server.events() {
                let _ = events.disconnect.set();
            }
            drop(slot);
            drop(slots);
            // Набор ожидания worker-а изменился (слот снова ждёт CONNECT_REQ).
            self.wake_slot(client_id);
            self.handler
                .on_client_disconnect_reason(client_id, DisconnectReason::Local);
        }

        Ok(())
    }

    /// Получение списка подключённых клиентов
    pub fn connected_clients(&self) -> Vec<u32> {
        let slots = self.slots.read().unwrap();
        slots
            .iter()
            .filter_map(|slot_mutex| {
                let slot = slot_mutex.lock().unwrap();
                if slot.connected { Some(slot.id) } else { None }
            })
            .collect()
    }

    /// Количество подключённых клиентов
    pub fn client_count(&self) -> u32 {
        let slots = self.slots.read().unwrap();
        slots
            .iter()
            .filter(|slot_mutex| slot_mutex.lock().unwrap().connected)
            .count() as u32
    }

    /// Проверка подключения конкретного клиента
    pub fn is_client_connected(&self, client_id: u32) -> bool {
        let slots = self.slots.read().unwrap();
        slots
            .get(client_id as usize)
            .is_some_and(|slot_mutex| slot_mutex.lock().unwrap().connected)
    }

    /// Жив ли процесс клиента: `Some(true/false)` -- клиент наблюдается через
    /// удерживаемый handle процесса; `None` -- не подключён или наблюдения
    /// нет (клиент старой версии, процесс не открылся).
    #[must_use]
    pub fn is_client_alive(&self, client_id: u32) -> Option<bool> {
        let slots = self.slots.read().unwrap();
        let slot = slots.get(client_id as usize)?.lock().unwrap();
        if !slot.connected {
            return None;
        }
        slot.server
            .is_peer_alive()
            .or_else(|| slot.owner_watch.as_ref().map(|watch| !watch.has_exited()))
    }

    /// Остановка сервера.
    ///
    /// Синхронно дожидается выхода worker-потоков перед возвратом — после
    /// return ни один callback (`on_message`/`on_client_connect`/`on_error`)
    /// больше не будет вызван -- вызывающий может сразу после возврата
    /// освободить состояние, на которое ссылается handler. Worker-ы спят в
    /// ядре без таймаута -- их будит событие `wake`, поэтому `stop()`
    /// возвращается сразу. Идемпотентна: повторный вызов — no-op.
    ///
    /// Ревизия 2: подключённым клиентам `DISCONNECT` сигналится сразу здесь
    /// (состояние `IDLE` -> `DISCONNECT`, как `disconnect_client`, без
    /// колбэков), а не когда отпустят последний `Arc` сервера (Drop слотов):
    /// раньше клиент оставался «подключённым» к остановленному серверу, пока
    /// кто-то держал `Arc<MultiServer>`.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
        for wake in &self.wakes {
            let _ = wake.set();
        }
        // Self-join исключён: если последний `Arc` отпустил сам worker (Drop
        // на его потоке), свой handle не джойним -- поток уже выходит.
        let handles: Vec<_> = self.worker_handles.lock().unwrap().drain(..).collect();
        for handle in handles {
            if handle.thread().id() != thread::current().id() {
                let _ = handle.join();
            }
        }
        // Worker-ы остановлены (кроме, возможно, вызывающего -- он выйдет по
        // `running`): отключаем подключённые слоты под их mutex-ами.
        let slots = self.slots.read().unwrap();
        for slot_mutex in slots.iter() {
            let mut slot = slot_mutex.lock().unwrap();
            if slot.connected {
                slot.reset_connection();
                if let Some(events) = slot.server.events() {
                    let _ = events.disconnect.set();
                }
            }
        }
    }

    /// Базовое имя канала
    pub fn base_name(&self) -> &str {
        &self.base_name
    }

    /// Получение имени канала для конкретного слота
    pub fn channel_name(&self, slot_id: u32) -> Option<String> {
        if slot_id < self.max_clients {
            Some(format!("{}_{}", self.base_name, slot_id))
        } else {
            None
        }
    }

    /// Число пробуждений worker-ов (тесты: ноль в простое).
    #[cfg(test)]
    pub(crate) fn wakeups(&self) -> u64 {
        self.wakeups.load(Ordering::Acquire)
    }

    /// Разбудить worker группы, обслуживающей слот.
    fn wake_slot(&self, slot_id: u32) {
        if let Some(wake) = self.wakes.get((slot_id / SLOTS_PER_WORKER) as usize) {
            let _ = wake.set();
        }
    }

    /// Обслуживание слотов группы (вызывается после каждого пробуждения, до
    /// сборки нового набора ожидания). Никаких собственных пробуждений не
    /// планирует, кроме ДЕДЛАЙНОВ в `Sweep::next_deadline`.
    ///
    /// - **не подключён, claim занят** -- захват без подключения. Дедлайн
    ///   `first_seen + reserve_timeout`; наступил -- CAS(claim -> FREE).
    ///   «Первое наблюдение» случается при любом пробуждении worker-а, в том
    ///   числе от «толчка» клиента, не нашедшего свободного слота
    ///   (`nudge_stale_claims`), так что протухший захват не ждёт тика.
    /// - **подключён, claim снят** -- «осиротевший» слот (брошенное
    ///   рукопожатие: сервер завершил handshake ровно когда клиент отвалился
    ///   по таймауту, снял claim и ушёл без `DISCONNECT`). Ловится на
    ///   ближайшем проходе, гарантированно -- на разовом дедлайне
    ///   `HANDSHAKE_VERIFY_DELAY` после handshake.
    /// - **подключён, процесс не наблюдается** (клиент старой версии, handle
    ///   не открылся) -- разовая проверка живости по PID владельца claim-а,
    ///   только при пробуждении по другой причине и не чаще
    ///   `LIVENESS_CHECK_INTERVAL`. Периодического тика ради неё нет:
    ///   смерть такого клиента замечается при следующем событии сервера.
    ///
    /// Отключение осиротевших (`handle_orphaned_slot_disconnect`) --
    /// вызывающим, ВНЕ блокировок; он перепроверяет claim через
    /// CAS(ожидаемый -> FREE) перед мутацией.
    ///
    /// Инвариант обеспечен принудительно (см. `client_worker`): клиентский
    /// `slot_timeout` всегда клампится ниже `RESERVE_TIMEOUT`, иначе сервер
    /// мог бы отнять слот у легитимно подключающегося клиента.
    fn reclaim_stale_claims(&self, range: Range<u32>, now: Instant) -> Sweep {
        let mut sweep = Sweep::default();
        let slots = self.slots.read().unwrap();
        for slot_id in range {
            let Some(slot_mutex) = slots.get(slot_id as usize) else {
                continue;
            };
            let mut slot = slot_mutex.lock().unwrap();
            // Worker проснулся -- handle отключённых клиентов больше не в
            // ожидании, их можно закрыть.
            slot.retired.clear();
            let claim = slot.claim();
            if slot.connected {
                if claim == CLAIM_FREE {
                    sweep
                        .orphaned
                        .push((slot.id, CLAIM_FREE, DisconnectReason::Graceful));
                    continue;
                }
                if let Some(at) = slot.verify_at {
                    if now >= at {
                        slot.verify_at = None;
                    } else {
                        sweep.deadline(at);
                    }
                }
                if slot.peer_handle().is_some() {
                    continue; // смерть процесса придёт событием
                }
                let due = slot.last_liveness_check.is_none_or(|last| {
                    now.saturating_duration_since(last) >= LIVENESS_CHECK_INTERVAL
                });
                if due {
                    slot.last_liveness_check = Some(now);
                    let owner_pid = slot.server.view().control_block().reserved
                        [RESERVED_OWNER_PID_INDEX]
                        .load(Ordering::Acquire);
                    if !win::is_process_alive(owner_pid) {
                        sweep
                            .orphaned
                            .push((slot.id, claim, DisconnectReason::PeerDied));
                    }
                }
                continue;
            }
            // Не подключён: протухший захват -- по дедлайну.
            if claim == CLAIM_FREE {
                slot.claim_seen = None;
                continue;
            }
            let seen_at = match slot.claim_seen {
                Some((token, at)) if token == claim => at,
                _ => {
                    slot.claim_seen = Some((claim, now));
                    now
                }
            };
            let due = seen_at + self.reserve_timeout;
            if now >= due {
                // CAS: если за это время claim сменился, это уже другой захват.
                let _ = slot.server.view().control_block().reserved[RESERVED_CLAIM_INDEX]
                    .compare_exchange(claim, CLAIM_FREE, Ordering::AcqRel, Ordering::Acquire);
                slot.claim_seen = None;
            } else {
                sweep.deadline(due);
            }
        }
        sweep
    }

    /// Набор ожидания группы: у подключённого слота -- `S2C_DISCONNECT`,
    /// `C2S_DATA`, процесс клиента (если наблюдается); у свободного --
    /// `C2S_CONNECT_REQ`. При одновременном сигнале NT отдаёт наименьший
    /// индекс; на `DISCONNECT` и смерть процесса кольцо всё равно дочитывается
    /// до колбэка, поэтому порядок внутри слота не теряет данных.
    fn collect_wait_set(
        &self,
        range: Range<u32>,
        handles: &mut Vec<isize>,
        sources: &mut Vec<EventSource>,
    ) {
        let slots = self.slots.read().unwrap();
        for slot_id in range {
            let Some(slot_mutex) = slots.get(slot_id as usize) else {
                continue;
            };
            let slot = slot_mutex.lock().unwrap();
            // Слоты всегда named (MultiServer::start поднимает
            // SharedServer::start), но на anonymous-слоте событий нет --
            // молча пропускаем вместо паники в worker-потоке.
            let Some(events) = slot.server.events() else {
                continue;
            };
            if slot.connected {
                handles.push(events.disconnect.raw_handle());
                sources.push(EventSource::SlotDisconnect(slot.id));
                handles.push(events.c2s.data.raw_handle());
                sources.push(EventSource::SlotData);
                if let Some(peer) = slot.peer_handle() {
                    handles.push(peer);
                    sources.push(EventSource::SlotPeer(slot.id));
                }
            } else {
                handles.push(events.connect_req.raw_handle());
                sources.push(EventSource::SlotConnect(slot.id));
            }
        }
    }

    /// Worker loop группы слотов `range`: спит до события или дедлайна.
    fn worker_loop(&self, group: usize, range: Range<u32>) {
        let Some(wake) = self.wakes.get(group) else {
            return;
        };
        let mut buffer = Vec::with_capacity(MAX_MESSAGE_SIZE);
        let mut handles: Vec<isize> = Vec::with_capacity(64);
        let mut sources: Vec<EventSource> = Vec::with_capacity(64);
        // В каком-то кольце осталось больше `recv_batch` сообщений: следующее
        // ожидание -- нулевое (сперва события, затем добор), без сна.
        let mut backlog = false;

        while self.running.load(Ordering::Acquire) {
            let sweep = self.reclaim_stale_claims(range.clone(), Instant::now());
            for (slot_id, expected_claim, reason) in sweep.orphaned {
                // Клиент мог уйти штатно (DISCONNECT, затем снял claim), а
                // worker проснулся по другой причине раньше, чем разобрал его
                // DISCONNECT: всё, что клиент успел записать, -- до колбэка.
                self.drain_slot(slot_id, &mut buffer);
                self.handle_orphaned_slot_disconnect(slot_id, expected_claim, reason);
            }

            handles.clear();
            sources.clear();
            self.collect_wait_set(range.clone(), &mut handles, &mut sources);
            handles.push(wake.raw_handle());
            sources.push(EventSource::Wake);

            let timeout = if backlog {
                Some(Duration::ZERO)
            } else {
                let until_deadline = sweep
                    .next_deadline
                    .map(|at| at.saturating_duration_since(Instant::now()));
                match (self.options.poll_timeout, until_deadline) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                }
            };

            let result = win::wait_any(&handles, timeout);
            self.wakeups.fetch_add(1, Ordering::AcqRel);
            match result {
                Ok(Some(index)) => {
                    if let Some(&source) = sources.get(index)
                        && self.handle_event(source, range.clone(), &mut buffer)
                    {
                        backlog = true;
                    }
                }
                Ok(None) => {
                    // Дедлайн (его обработает проход в начале цикла) или
                    // добор данных сверх `recv_batch`.
                    if backlog {
                        backlog = self.poll_group_slots(range.clone(), &mut buffer);
                    }
                }
                Err(err) => {
                    self.handler.on_error(None, err);
                }
            }
        }
    }

    /// Обработка события. `true` -- в кольцах группы остались сообщения.
    fn handle_event(&self, source: EventSource, range: Range<u32>, buffer: &mut Vec<u8>) -> bool {
        match source {
            EventSource::SlotConnect(slot_id) => {
                self.handle_slot_connect(slot_id);
                false
            }
            // Данные одного слота -- добор по всей группе: без этого
            // занятой слот с меньшим индексом мог бы «затенять» остальных
            // (NT отдаёт наименьший сигнальный индекс).
            EventSource::SlotData => self.poll_group_slots(range, buffer),
            EventSource::SlotDisconnect(slot_id) => {
                self.handle_slot_disconnect(slot_id, buffer);
                false
            }
            EventSource::SlotPeer(slot_id) => {
                self.handle_slot_peer_died(slot_id, buffer);
                false
            }
            // Набор ожидания изменился или остановка -- верх цикла разберётся.
            EventSource::Wake => false,
        }
    }

    /// `C2S_CONNECT_REQ` на свободном слоте.
    fn handle_slot_connect(&self, slot_id: u32) {
        let slots = self.slots.read().unwrap();
        let Some(slot_mutex) = slots.get(slot_id as usize) else {
            return;
        };
        let mut slot = slot_mutex.lock().unwrap();
        if slot.connected {
            return; // Уже подключён
        }

        // Handshake -- общий с `SharedServer` (`complete_handshake`): PID
        // клиента из `reserved[3]` -> удерживаемый handle процесса,
        // сброс устаревшего `DISCONNECT`, `S2C_CONNECT`.
        match slot.server.accept_pending() {
            Ok(()) => {
                slot.connected = true;
                // Claim клиент ставит до `CLIENT_HELLO` (Acquire-чтение
                // заявки в `accept_pending` делает его видимым).
                slot.session_claim = slot.claim();
                slot.claim_seen = None;
                slot.last_liveness_check = None;
                slot.verify_at = Some(Instant::now() + HANDSHAKE_VERIFY_DELAY);
                if slot.server.peer_pid().is_none() {
                    // Клиент старой версии не передал PID в handshake, но
                    // PID владельца claim-а он пишет всегда -- наблюдаем его.
                    let owner_pid = slot.server.view().control_block().reserved
                        [RESERVED_OWNER_PID_INDEX]
                        .load(Ordering::Acquire);
                    slot.owner_watch = ProcessWatch::open_peer(owner_pid);
                }
                let id = slot.id;
                drop(slot);
                drop(slots);
                self.handler.on_client_connect(id);
            }
            Err(_) => {
                // `CONNECT_REQ` без `CLIENT_HELLO`: клиент откатил заявку по
                // таймауту (claim он снял сам) или это «толчок» клиента, не
                // нашедшего свободного слота (`nudge_stale_claims`). Claim не
                // трогаем: живой захват скоро подключится, протухший снимет
                // дедлайн `reserve_timeout`, который назначит ближайший проход.
            }
        }
    }

    /// Дочитать кольцо слота целиком (пир ушёл -- новых данных не будет).
    fn drain_slot(&self, slot_id: u32, buffer: &mut Vec<u8>) {
        while self.receive_from_slot(slot_id, buffer) {}
    }

    /// `S2C_DISCONNECT` подключённого слота: клиент ушёл штатно. Всё, что он
    /// успел записать (прощальное сообщение), доставляется ДО колбэка.
    fn handle_slot_disconnect(&self, slot_id: u32, buffer: &mut Vec<u8>) {
        {
            let slots = self.slots.read().unwrap();
            let Some(slot_mutex) = slots.get(slot_id as usize) else {
                return;
            };
            let slot = slot_mutex.lock().unwrap();
            if !slot.connected {
                // Слот уже отключён локально (`disconnect_client`), а worker
                // спал на старом наборе и поглотил сигнал, адресованный
                // КЛИЕНТУ (событие одно на обе стороны). Возвращаем его.
                if let Some(events) = slot.server.events() {
                    let _ = events.disconnect.set();
                }
                return;
            }
        }

        self.drain_slot(slot_id, buffer);

        let was_connected = {
            let slots = self.slots.read().unwrap();
            let Some(slot_mutex) = slots.get(slot_id as usize) else {
                return;
            };
            let mut slot = slot_mutex.lock().unwrap();
            let was = slot.connected;
            if was {
                let session_claim = slot.session_claim;
                slot.reset_connection();
                // Ревизия 2: CAS claim сессии -> FREE, а не store. Клиент 0.9
                // снимает claim сам сразу после `DISCONNECT` (R13), и новый
                // клиент мог уже захватить слот, пока мы дочитывали кольцо --
                // безусловный store отнял бы у него захват. Клиент старой
                // версии, не снявший claim, освобождается этим CAS.
                let _ = slot.server.view().control_block().reserved[RESERVED_CLAIM_INDEX]
                    .compare_exchange(
                        session_claim,
                        CLAIM_FREE,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
            }
            was
        };

        if was_connected {
            self.handler
                .on_client_disconnect_reason(slot_id, DisconnectReason::Graceful);
        }
    }

    /// Процесс клиента завершился (handle сигнален): дочитать кольцо, затем
    /// отключить слот с `PeerDied`. Claim снимается, только если его держит
    /// именно умерший процесс (PID владельца совпадает) -- новый захват не
    /// затирается.
    fn handle_slot_peer_died(&self, slot_id: u32, buffer: &mut Vec<u8>) {
        self.drain_slot(slot_id, buffer);

        let was_connected = {
            let slots = self.slots.read().unwrap();
            let Some(slot_mutex) = slots.get(slot_id as usize) else {
                return;
            };
            let mut slot = slot_mutex.lock().unwrap();
            if !slot.connected {
                return;
            }
            let control = slot.server.view().control_block();
            let claim = control.reserved[RESERVED_CLAIM_INDEX].load(Ordering::Acquire);
            let owner = control.reserved[RESERVED_OWNER_PID_INDEX].load(Ordering::Acquire);
            if claim != CLAIM_FREE && slot.watched_pid() == Some(owner) {
                let _ = control.reserved[RESERVED_CLAIM_INDEX].compare_exchange(
                    claim,
                    CLAIM_FREE,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
            // Handle процесса обязан уйти из набора ожидания в любом случае:
            // он сигнален навсегда и иначе будил бы worker без конца.
            slot.reset_connection();
            true
        };

        if was_connected {
            self.handler
                .on_client_disconnect_reason(slot_id, DisconnectReason::PeerDied);
        }
    }

    /// Отключение осиротевшего слота, обнаруженного `reclaim_stale_claims`:
    /// либо брошенное рукопожатие (claim был `CLAIM_FREE`), либо
    /// подтверждённо мёртвый процесс-владелец (claim — его последний
    /// известный token). В обоих случаях `expected_claim` — это ЗНАЧЕНИЕ
    /// claim, увиденное в момент детекции.
    ///
    /// Между детекцией и этим вызовом могло пройти время: если claim успел
    /// измениться (новый клиент захватил освободившийся слот), безусловная
    /// мутация затёрла бы легитимное состояние (аудит 2026-07-10, находка
    /// "unconditional store race"). Поэтому `claim == expected_claim`
    /// проверяется и одновременно фиксируется ОДНИМ атомарным
    /// `compare_exchange(expected_claim, CLAIM_FREE)`: если он проваливается
    /// — слот уже не тот, что мы считали осиротевшим, пропускаем без единой
    /// мутации остального состояния.
    fn handle_orphaned_slot_disconnect(
        &self,
        slot_id: u32,
        expected_claim: u32,
        reason: DisconnectReason,
    ) {
        let was_connected = {
            let slots = self.slots.read().unwrap();
            let Some(slot_mutex) = slots.get(slot_id as usize) else {
                return;
            };
            let mut slot = slot_mutex.lock().unwrap();

            if !slot.connected {
                return; // уже обработан (например явным disconnect-событием)
            }

            let claim_field = &slot.server.view().control_block().reserved[RESERVED_CLAIM_INDEX];
            if claim_field
                .compare_exchange(
                    expected_claim,
                    CLAIM_FREE,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                return; // claim изменился — состояние слота уже не то, что при детекции
            }

            // claim атомарно переведён в FREE выше (или уже был FREE и остался
            // им) -- освобождать его повторно не нужно.
            slot.reset_connection();
            true
        };

        if was_connected {
            self.handler.on_client_disconnect_reason(slot_id, reason);
        }
    }

    /// Получение сообщений от слота (не больше `recv_batch` за вызов).
    /// `true` -- пачка выбрана целиком, в кольце могут остаться сообщения.
    fn receive_from_slot(&self, slot_id: u32, buffer: &mut Vec<u8>) -> bool {
        // Собираем все сообщения под lock-ом
        let mut messages: Vec<Vec<u8>> = Vec::new();
        let mut error: Option<ShmError> = None;
        let batch = self.options.recv_batch.max(1);

        {
            let slots = self.slots.read().unwrap();
            let Some(slot_mutex) = slots.get(slot_id as usize) else {
                return false;
            };
            let slot = slot_mutex.lock().unwrap();
            if !slot.connected {
                return false;
            }
            for _ in 0..batch {
                match slot.server.receive_from_client(buffer) {
                    Ok(len) => messages.push(buffer[..len].to_vec()),
                    Err(ShmError::QueueEmpty) => break,
                    Err(err) => {
                        error = Some(err);
                        break;
                    }
                }
            }
        }

        let more = error.is_none() && messages.len() == batch;

        // Отдаём handler-у без lock-а
        for data in &messages {
            self.handler.on_message(slot_id, data);
        }

        if let Some(err) = error {
            self.handler.on_error(Some(slot_id), err);
        }
        more
    }

    /// Пачка сообщений с каждого подключённого слота группы. `true` -- где-то
    /// остались сообщения.
    fn poll_group_slots(&self, range: Range<u32>, buffer: &mut Vec<u8>) -> bool {
        let slot_ids: Vec<u32> = {
            let slots = self.slots.read().unwrap();
            range
                .filter(|&id| {
                    slots
                        .get(id as usize)
                        .is_some_and(|slot| slot.lock().unwrap().connected)
                })
                .collect()
        };

        let mut more = false;
        for slot_id in slot_ids {
            more |= self.receive_from_slot(slot_id, buffer);
        }
        more
    }
}

impl std::fmt::Debug for MultiServer {
    /// Ручная реализация: `handler` -- `Arc<dyn MultiHandler>`, у трейта нет
    /// `Debug`. Печатаем то, что реально полезно в логах: имя и заполненность.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MultiServer")
            .field("base_name", &self.base_name)
            .field("max_clients", &self.max_clients)
            .field("connected", &self.client_count())
            .field("running", &self.running.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl Drop for MultiServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Источник события для worker loop
#[derive(Clone, Copy, Debug)]
enum EventSource {
    SlotConnect(u32),
    SlotData,
    SlotDisconnect(u32),
    SlotPeer(u32),
    Wake,
}

// ============================================================================
// MultiClient — клиент с автоматическим назначением слота
// ============================================================================

#[derive(Debug)]
enum ClientCommand {
    Send(Vec<u8>),
    Shutdown,
}

/// Мультиклиент — подключается к базовому имени, получает слот автоматически
#[derive(Debug)]
pub struct MultiClient {
    cmd_tx: Sender<ClientCommand>,
    /// Будит worker: новая команда (`send`/`stop`/Drop). Безымянное,
    /// автосброс.
    wake: Arc<EventHandle>,
    join: Mutex<Option<JoinHandle<()>>>,
    running: Arc<AtomicBool>,
    slot_id: Arc<AtomicU32>,
    /// Число пробуждений подключённого worker-а (тесты: ноль в простое).
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "читается только тестами (доказательство отсутствия опроса)"
        )
    )]
    wakeups: Arc<AtomicU64>,
}

impl MultiClient {
    /// Подключение к мультисерверу
    ///
    /// Клиент автоматически:
    /// 1. Пробегает слоты base_name_0.. и атомарно захватывает свободный (CAS)
    /// 2. Выполняет обычный handshake с захваченным слотом
    /// 3. При потере связи — повторяет захват свободного слота (через
    ///    `retry_delay`)
    pub fn connect(
        base_name: &str,
        handler: Arc<dyn MultiClientHandler>,
        options: MultiClientOptions,
    ) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let wake = Arc::new(EventHandle::create_unnamed(false)?);
        let running = Arc::new(AtomicBool::new(true));
        let slot_id = Arc::new(AtomicU32::new(SLOT_ID_NO_SLOT));
        let wakeups = Arc::new(AtomicU64::new(0));

        let shared = ClientShared {
            wake: wake.clone(),
            running: running.clone(),
            slot_id: slot_id.clone(),
            wakeups: wakeups.clone(),
        };
        let name = base_name.to_owned();

        #[cfg_attr(not(debug_assertions), allow(unused_mut))]
        let mut builder = thread::Builder::new();
        #[cfg(debug_assertions)]
        {
            builder = builder.name(format!("xsmc-{base_name}"));
        }
        let handle = crate::thread_hook::spawn(builder, move || {
            client_worker(&name, &handler, &options, &rx, &shared);
        })
        .map_err(|e| ShmError::WindowsError {
            code: e.raw_os_error().unwrap_or(-1) as u32,
            context: "spawn multi client worker",
        })?;

        Ok(Self {
            cmd_tx: tx,
            wake,
            join: Mutex::new(Some(handle)),
            running,
            slot_id,
            wakeups,
        })
    }

    /// Отправка сообщения серверу
    pub fn send(&self, data: &[u8]) -> Result<()> {
        if !self.running.load(Ordering::Acquire) {
            return Err(ShmError::NotReady);
        }
        self.cmd_tx
            .send(ClientCommand::Send(data.to_vec()))
            .map_err(|_| ShmError::NotReady)?;
        // Команда уже в канале: worker, проснувшись, её увидит.
        let _ = self.wake.set();
        Ok(())
    }

    /// Получить назначенный slot_id (SLOT_ID_NO_SLOT если не подключён)
    pub fn slot_id(&self) -> u32 {
        self.slot_id.load(Ordering::Acquire)
    }

    /// Проверка подключения
    pub fn is_connected(&self) -> bool {
        self.slot_id.load(Ordering::Acquire) != SLOT_ID_NO_SLOT
    }

    /// Остановка клиента (асинхронно): принятое до вызова дописывается в
    /// кольцо, worker выходит. Прерывает и паузу между попытками захвата.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
        let _ = self.cmd_tx.send(ClientCommand::Shutdown);
        let _ = self.wake.set();
    }

    /// Число пробуждений подключённого worker-а.
    #[cfg(test)]
    pub(crate) fn wakeups(&self) -> u64 {
        self.wakeups.load(Ordering::Acquire)
    }
}

impl Drop for MultiClient {
    fn drop(&mut self) {
        self.stop();
        if let Some(handle) = self.join.lock().unwrap().take() {
            // Drop из собственного колбэка (worker-поток) -- не self-join:
            // поток уже видит `running == false` и выйдет сам.
            if handle.thread().id() != thread::current().id() {
                let _ = handle.join();
            }
        }
    }
}

/// Состояние, общее для `MultiClient` и его worker-а.
struct ClientShared {
    wake: Arc<EventHandle>,
    running: Arc<AtomicBool>,
    slot_id: Arc<AtomicU32>,
    wakeups: Arc<AtomicU64>,
}

/// Клампит клиентский `slot_timeout` ниже `RESERVE_TIMEOUT` с запасом
/// `RESERVE_SAFETY_MARGIN` — иначе при достаточно большом вызывающим-заданном
/// `slot_timeout` (сверху он не ограничен) сервер мог бы счесть
/// клиента протухшим и отдать слот другому раньше, чем клиент сам отвалится
/// по своему таймауту.
fn clamp_slot_timeout(requested: Duration) -> Duration {
    requested.min(RESERVE_TIMEOUT.saturating_sub(RESERVE_SAFETY_MARGIN))
}

/// Кладёт сообщение во внутреннюю send-очередь MultiClient, вытесняя самое
/// старое при переполнении (`max_send_queue`) вместо неограниченного роста.
/// Возвращает `true`, если пришлось вытеснить старое сообщение (переполнение).
fn push_with_cap(queue: &mut VecDeque<Vec<u8>>, data: Vec<u8>, max_send_queue: usize) -> bool {
    let overflowed = if queue.len() >= max_send_queue {
        queue.pop_front();
        true
    } else {
        false
    };
    queue.push_back(data);
    overflowed
}

/// Команды из канала -> очередь отправки; `Shutdown` гасит `running`.
fn drain_commands(
    cmd_rx: &Receiver<ClientCommand>,
    queue: &mut VecDeque<Vec<u8>>,
    options: &MultiClientOptions,
    running: &AtomicBool,
    handler: &Arc<dyn MultiClientHandler>,
) {
    while let Ok(cmd) = cmd_rx.try_recv() {
        match cmd {
            ClientCommand::Send(data) => {
                if push_with_cap(queue, data, options.max_send_queue) {
                    handler.on_overflow(1);
                }
            }
            ClientCommand::Shutdown => running.store(false, Ordering::Release),
        }
    }
}

/// Очередь -> кольцо. `send_to_server` пишет с перезаписью старейшего и
/// места не ждёт; сообщение, которое записать нельзя вовсе (длина вне
/// пределов), выбрасывается с `on_error` -- иначе оно навсегда застряло бы
/// в голове очереди.
fn flush_queue(
    client: &SharedClient,
    queue: &mut VecDeque<Vec<u8>>,
    handler: &Arc<dyn MultiClientHandler>,
) {
    while let Some(data) = queue.pop_front() {
        if let Err(err) = client.send_to_server(&data) {
            handler.on_error(err);
        }
    }
}

/// Прочитать всё, что есть в кольце сервера.
fn receive_all(client: &SharedClient, buffer: &mut Vec<u8>, handler: &Arc<dyn MultiClientHandler>) {
    loop {
        match client.receive_from_server(buffer) {
            Ok(len) => handler.on_message(&buffer[..len]),
            Err(ShmError::QueueEmpty) => break,
            Err(err) => {
                handler.on_error(err);
                break;
            }
        }
    }
}

/// Worker для MultiClient
fn client_worker(
    base_name: &str,
    handler: &Arc<dyn MultiClientHandler>,
    options: &MultiClientOptions,
    cmd_rx: &Receiver<ClientCommand>,
    shared: &ClientShared,
) {
    let mut buffer = Vec::with_capacity(MAX_MESSAGE_SIZE);
    let running = &*shared.running;
    let wake = &*shared.wake;
    let effective_slot_timeout = clamp_slot_timeout(options.slot_timeout);

    while running.load(Ordering::Acquire) {
        // Шаг 1: атомарно захватываем свободный слот (без централизованного lobby).
        let (slot_id, slot_name, token) = match claim_free_slot(base_name) {
            Ok(v) => v,
            Err(err) => {
                if err == ShmError::NoFreeSlot {
                    // Возможно, слот держит захват упавшего клиента: толкаем
                    // сервер, чтобы он назначил дедлайн его освобождения.
                    nudge_stale_claims(base_name);
                }
                handler.on_error(err);
                if !wait_delay_or(running, wake, options.retry_delay) {
                    break;
                }
                continue;
            }
        };

        // Шаг 2: подключаемся к захваченному слоту обычным handshake.
        // Ревизия 2: ожидание `S2C_CONNECT` прерывается `wake` -- `stop`/Drop
        // не ждут `slot_timeout` (`stop` гасит `running` сам).
        let mut should_stop = || !running.load(Ordering::Acquire);
        let connected = SharedClient::connect_interruptible(
            &slot_name,
            effective_slot_timeout,
            Interrupt {
                wake: wake.raw_handle(),
                should_stop: &mut should_stop,
            },
        );
        let mut client = match connected {
            Ok(c) => c,
            Err(_) if !running.load(Ordering::Acquire) => {
                release_claim(&slot_name, token);
                break;
            }
            Err(err) => {
                // Не подключились — освобождаем захваченный слот (best-effort;
                // иначе сервер вернёт его в оборот по RESERVE_TIMEOUT).
                release_claim(&slot_name, token);
                handler.on_error(err);
                if !wait_delay_or(running, wake, options.retry_delay) {
                    break;
                }
                continue;
            }
        };

        shared.slot_id.store(slot_id, Ordering::Release);
        handler.on_connect(slot_id);

        // Шаг 3: работаем по событиям. Набор: [DISCONNECT, DATA, процесс
        // сервера (0.8+), wake] -- меньший индекс важнее.
        let client_events = client.events();
        let peer = client.peer_wait_handle();
        let mut handles = [
            client_events.disconnect.raw_handle(),
            client_events.s2c.data.raw_handle(),
            0,
            0,
        ];
        let (count, peer_index) = match peer {
            Some(peer) => {
                handles[2] = peer;
                handles[3] = wake.raw_handle();
                (4, Some(2))
            }
            None => {
                handles[2] = wake.raw_handle();
                (3, None)
            }
        };
        let mut send_queue: VecDeque<Vec<u8>> = VecDeque::new();

        let reason = loop {
            drain_commands(cmd_rx, &mut send_queue, options, running, handler);
            flush_queue(&client, &mut send_queue, handler);
            if !running.load(Ordering::Acquire) {
                // Остановка: принятое до `stop()` уже в кольце -- сервер
                // дочитает его до DISCONNECT (Drop клиента ниже).
                break None;
            }
            receive_all(&client, &mut buffer, handler);

            let result = win::wait_any(&handles[..count], options.poll_timeout);
            shared.wakeups.fetch_add(1, Ordering::AcqRel);
            match result {
                Ok(Some(0)) => {
                    // Штатное отключение сервера: его последние сообщения
                    // доставляем ДО колбэка.
                    receive_all(&client, &mut buffer, handler);
                    break Some(DisconnectReason::Graceful);
                }
                Ok(Some(i)) if Some(i) == peer_index => {
                    receive_all(&client, &mut buffer, handler);
                    break Some(DisconnectReason::PeerDied);
                }
                // DATA, wake или страховочный таймаут -- следующий проход.
                // Ревизия 2: сначала сверить сессию. `generation` сменился --
                // слот уже у другого клиента (сервер старой версии освободил
                // claim до нашего пробуждения, и `complete_handshake`
                // нового клиента сбросил наш `DISCONNECT`). Сигнал `DATA` был
                // для него: возвращаем, и уходим без `DISCONNECT`.
                Ok(Some(index)) => {
                    if !client.is_session_current() {
                        if index == 1 {
                            let _ = client.events().s2c.data.set();
                        }
                        break Some(DisconnectReason::Graceful);
                    }
                }
                Ok(None) => {
                    if !client.is_session_current() {
                        break Some(DisconnectReason::Graceful);
                    }
                }
                Err(err) => {
                    handler.on_error(err);
                    break Some(DisconnectReason::Error);
                }
            }
        };

        // slot_id сбрасывается ЗДЕСЬ, а не только на ветке disconnect: выход по
        // `running == false` (stop()/Drop) её минует, и `is_connected()` навсегда
        // оставался бы `true` уже после остановки клиента (аудит 2026-07-28).
        shared.slot_id.store(SLOT_ID_NO_SLOT, Ordering::Release);
        if let Some(reason) = reason {
            // Сервер ушёл сам: `DISCONNECT` в ответ не сигналим -- поздний
            // сигнал мог бы достаться уже следующему клиенту этого слота.
            client.mark_disconnected();
            handler.on_disconnect_reason(reason);
        }
        drop(client);
        // Слот освободился у нас — снимаем claim (best-effort), чтобы он сразу
        // вернулся в оборот. CAS token->FREE сработает, только если claim ещё наш
        // (если сервер уже отнял слот по таймауту/force-disconnect — это no-op).
        release_claim(&slot_name, token);

        if !wait_delay_or(running, wake, options.retry_delay) {
            break;
        }
    }
}

/// Процесс-локальный счётчик попыток захвата (гарантирует, что в пределах
/// одного процесса токены не повторяются, пока живёт процесс).
static CLAIM_TOKEN_COUNTER: AtomicU32 = AtomicU32::new(1);

/// Генерирует токен захвата, устойчивый к ABA между процессами.
///
/// Раньше токен строился как `pid<<8 ^ n` — как только процесс-локальный
/// счётчик `n` превышал 255, XOR начинал портить биты pid, и токен мог
/// совпасть с токеном другого процесса. Из-за этого устаревший (уже
/// неактуальный) вызов `release_claim` с таким совпавшим токеном мог
/// CAS-ом освободить ЖИВОЙ claim чужого процесса (аудит 2026-07-10).
///
/// `RandomState` (std, без внешних зависимостей) на каждый вызов `new()`
/// сидируется свежей ОС-энтропией — смешивая её с pid и процесс-локальным
/// счётчиком, получаем токен, практически уникальный и внутри процесса,
/// и между процессами (в отличие от детерминированной XOR-схемы).
fn next_claim_token() -> u32 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    let n = CLAIM_TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u32(pid);
    hasher.write_u32(n);
    let token = hasher.finish() as u32;
    if token == CLAIM_FREE { 1 } else { token }
}

/// Пытается атомарно захватить конкретный слот через `compare_exchange`.
/// `Ok(true)` — захвачено нами; `Ok(false)` — слот занят/невалиден;
/// `Err(_)` — слота с таким именем не существует (сегмент не открылся).
fn try_claim_slot(slot_name: &str, token: u32) -> Result<bool> {
    // Сервер создаёт секцию под именем mapping_name(slot_name) — открываем так же.
    let mapping = Mapping::open(&mapping_name(slot_name))?; // Err => слота нет
    // SAFETY: `Mapping::open` проверил размер отображения и держит его живым на
    // всё время жизни `mapping` (а значит и `view`).
    let view = unsafe { SharedView::new(mapping.as_ptr()) };
    let control = view.control_block();
    if control.magic != SHARED_MAGIC || control.version != SHARED_VERSION {
        return Ok(false); // чужой/повреждённый сегмент — пропускаем
    }
    let claimed = control.reserved[RESERVED_CLAIM_INDEX]
        .compare_exchange(CLAIM_FREE, token, Ordering::AcqRel, Ordering::Acquire)
        .is_ok();
    if claimed {
        // Публикуем свой PID для liveness-проверки сервером (см.
        // RESERVED_OWNER_PID_INDEX) -- пишем СРАЗУ после успешного захвата,
        // Release гарантирует, что сервер, увидевший claim (Acquire), увидит
        // и корректный PID, а не мусор/значение от предыдущего владельца.
        control.reserved[RESERVED_OWNER_PID_INDEX].store(std::process::id(), Ordering::Release);
    }
    Ok(claimed)
    // mapping размапится здесь; захваченный claim остаётся в shared memory.
}

/// Снять собственный claim со слота (CAS token -> FREE), если он всё ещё наш.
fn release_claim(slot_name: &str, token: u32) {
    if let Ok(mapping) = Mapping::open(&mapping_name(slot_name)) {
        // SAFETY: `Mapping::open` проверил размер отображения и держит его живым на
        // всё время жизни `mapping` (а значит и `view`).
        let view = unsafe { SharedView::new(mapping.as_ptr()) };
        let _ = view.control_block().reserved[RESERVED_CLAIM_INDEX].compare_exchange(
            token,
            CLAIM_FREE,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

/// «Толчок» сервера (0.9): клиент, не нашедший свободного слота, взводит
/// `C2S_CONNECT_REQ` каждого слота, чей claim занят, а сервер его ещё не
/// принял (`server_state != SERVER_READY`). Сервер просыпается, видит
/// `CONNECT_REQ` без `CLIENT_HELLO` (не рукопожатие -- claim не трогает) и
/// назначает дедлайн освобождения протухшего захвата. Так упавший между
/// захватом и подключением клиент не держит слот, пока сервер спит, -- без
/// какого-либо тика на сервере.
fn nudge_stale_claims(base_name: &str) {
    for slot_id in 0..MAX_MULTI_CLIENTS {
        let slot_name = format!("{base_name}_{slot_id}");
        let Ok(mapping) = Mapping::open(&mapping_name(&slot_name)) else {
            break; // слотов больше нет
        };
        // SAFETY: `Mapping::open` проверил размер отображения и держит его живым на
        // всё время жизни `mapping` (а значит и `view`).
        let view = unsafe { SharedView::new(mapping.as_ptr()) };
        let control = view.control_block();
        if control.magic != SHARED_MAGIC || control.version != SHARED_VERSION {
            continue;
        }
        let claimed = control.reserved[RESERVED_CLAIM_INDEX].load(Ordering::Acquire) != CLAIM_FREE;
        let accepted = control.server_state.load(Ordering::Acquire) == HANDSHAKE_SERVER_READY;
        if claimed
            && !accepted
            && let Ok(event) = EventHandle::open(&event_name(
                &slot_name,
                Direction::ClientToServer,
                EVENT_CONNECT_REQ_SUFFIX,
            ))
        {
            let _ = event.set();
        }
    }
}

/// Пробегает слоты `base_name_0..` и атомарно захватывает первый свободный.
/// Конкурентные клиенты захватывают РАЗНЫЕ слоты (CAS на разной памяти).
fn claim_free_slot(base_name: &str) -> Result<(u32, String, u32)> {
    let token = next_claim_token();
    let mut saw_slot = false;
    for slot_id in 0..MAX_MULTI_CLIENTS {
        let slot_name = format!("{base_name}_{slot_id}");
        match try_claim_slot(&slot_name, token) {
            Ok(true) => return Ok((slot_id, slot_name, token)),
            Ok(false) => {
                saw_slot = true;
                continue;
            }
            Err(_) => break, // слотов больше нет
        }
    }
    if saw_slot {
        Err(ShmError::NoFreeSlot) // сервер есть, но все слоты заняты
    } else {
        Err(ShmError::NotConnected) // сервер не запущен (нет ни одного слота)
    }
}

#[cfg(test)]
mod tests;
