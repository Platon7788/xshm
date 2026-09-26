//! Dispatch-сервер — единое лобби с динамическими каналами на каждого клиента.
//!
//! # Архитектура
//!
//! ```text
//! DispatchServer("Global\NxT") ← единственное лобби, принимает всех клиентов
//!     ↓
//! Клиент захватывает мьютекс лобби "Global\NxT_lock" (0.9: клиенты лобби
//! строго по одному) → подключается к лобби → RegistrationRequest {pid, revision, name}
//!     ↓
//! Сервер создаёт AutoServer("Global\3f9a0c1d2e4b5a67") → RegistrationResponse
//!     ↓
//! Клиент отключается от лобби, отпускает мьютекс → подключается к
//! "Global\3f9a0c1d2e4b5a67" через AutoClient
//!     ↓
//! Обмен 1:1 на выделенном канале
//! ```
//!
//! Имя канала -- 16 hex-символов из ОС-энтропии, **без** базового имени
//! лобби, но в том же пространстве имён (0.9): лобби `Global\X` -> канал
//! `Global\<hex>`, `Local\X` -> `Local\<hex>`, NT-путь `\Dir\X` ->
//! `\Dir\<hex>`, имя без префикса -> `<hex>` (неявный `Local\`). До 0.9
//! канал всегда был `<hex>` и из другой сессии не открывался.

pub mod protocol;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::auto::{AutoClient, AutoHandler, AutoOptions, AutoServer, ChannelKind};
use crate::client::SharedClient;
use crate::constants::MAX_MESSAGE_SIZE;
use crate::error::{DisconnectReason, Result, ShmError};
use crate::naming::mapping_name;
use crate::ring::FreeSpace;
use crate::server::SharedServer;
use crate::wait_delay_or;
use crate::win::{self, EventHandle, Mapping, NamedMutex};

pub use protocol::{RegistrationRequest, RegistrationResponse};

// ─── Public types ────────────────────────────────────────────────────────────

/// Данные регистрации клиента, полученные во время handshake в лобби.
#[derive(Debug, Clone)]
pub struct ClientRegistration {
    pub pid: u32,
    pub revision: u16,
    pub name: String,
}

/// Callback-интерфейс для событий DispatchServer.
pub trait DispatchHandler: Send + Sync + 'static {
    /// Вызывается, когда клиент зарегистрировался и подключился к своему выделенному каналу.
    fn on_client_connect(&self, client_id: u32, info: &ClientRegistration);

    /// Вызывается при отключении клиента от выделенного канала.
    fn on_client_disconnect(&self, client_id: u32);

    /// Отключение клиента с причиной (0.8+). По умолчанию вызывает
    /// `on_client_disconnect`; сервер вызывает ТОЛЬКО этот метод.
    /// `PeerDied` -- процесс клиента завершился без штатного отключения
    /// (замечено по удерживаемому handle процесса, обычно за единицы мс).
    fn on_client_disconnect_reason(&self, client_id: u32, reason: DisconnectReason) {
        let _ = reason;
        self.on_client_disconnect(client_id);
    }

    /// Вызывается при получении сообщения от клиента по выделенному каналу.
    /// Гарантия порядка: для каждого клиента -- строго после
    /// `on_client_connect` и до `on_client_disconnect_reason`.
    fn on_message(&self, client_id: u32, data: &[u8]);

    /// Клиент освободил место в канале к нему (прочитал): `try_send_to`,
    /// получивший `QueueFull`, стоит повторить. Событие, а не опрос; по
    /// умолчанию ничего не делает.
    fn on_space_available(&self, client_id: u32) {
        let _ = client_id;
    }

    /// Вызывается при ошибке (client_id = None для общих ошибок).
    fn on_error(&self, client_id: Option<u32>, err: ShmError) {
        let _ = (client_id, err);
    }
}

/// Callback-интерфейс для событий DispatchClient.
pub trait DispatchClientHandler: Send + Sync + 'static {
    /// Вызывается при успешном подключении к выделенному каналу.
    fn on_connect(&self, client_id: u32, channel_name: &str);

    /// Вызывается при отключении от выделенного канала.
    fn on_disconnect(&self);

    /// Отключение с причиной (0.8+). По умолчанию вызывает `on_disconnect`;
    /// клиент вызывает ТОЛЬКО этот метод. `PeerDied` -- процесс сервера
    /// (viewer-а) умер без штатного отключения.
    fn on_disconnect_reason(&self, reason: DisconnectReason) {
        let _ = reason;
        self.on_disconnect();
    }

    /// Вызывается при получении сообщения от сервера.
    fn on_message(&self, data: &[u8]);

    /// Сервер освободил место в канале (прочитал): `try_send`, получивший
    /// `QueueFull`, стоит повторить. К моменту вызова worker уже дописал из
    /// своей очереди в кольцо всё, что влезло. Событие, а не опрос -- ждущий
    /// отправитель спит до него; по умолчанию ничего не делает.
    fn on_space_available(&self) {}

    /// Вызывается при ошибке.
    fn on_error(&self, err: ShmError) {
        let _ = err;
    }
}

/// Настройки DispatchServer.
#[derive(Clone, Debug)]
pub struct DispatchOptions {
    /// Таймаут чтения данных регистрации из лобби после handshake.
    pub lobby_timeout: Duration,
    /// Таймаут подключения клиента к выделенному каналу после регистрации.
    pub channel_connect_timeout: Duration,
    /// Страховочный таймаут ожидания потоков сервера (лобби, каналы).
    /// `None` (по умолчанию, 0.9+) -- только события, ни одного пробуждения
    /// в простое (см. `AutoOptions::poll_timeout`).
    pub poll_timeout: Option<Duration>,
    /// Количество сообщений за один цикл на каждом клиентском канале.
    pub recv_batch: usize,
}

impl Default for DispatchOptions {
    fn default() -> Self {
        Self {
            lobby_timeout: Duration::from_secs(5),
            channel_connect_timeout: Duration::from_secs(30),
            poll_timeout: None,
            recv_batch: 32,
        }
    }
}

/// Настройки DispatchClient.
#[derive(Clone, Debug)]
pub struct DispatchClientOptions {
    /// Таймаут подключения к лобби.
    pub lobby_timeout: Duration,
    /// Таймаут чтения ответа регистрации из лобби.
    pub response_timeout: Duration,
    /// Таймаут подключения к выделенному каналу.
    pub channel_timeout: Duration,
    /// Страховочный таймаут ожидания worker-а канала. `None` (по умолчанию,
    /// 0.9+) -- только события (см. `AutoOptions::poll_timeout`).
    pub poll_timeout: Option<Duration>,
    /// Количество сообщений за один цикл.
    pub recv_batch: usize,
    /// Максимум сообщений в очереди перед сбросом самого старого.
    pub max_send_queue: usize,
}

impl Default for DispatchClientOptions {
    fn default() -> Self {
        Self {
            lobby_timeout: Duration::from_secs(5),
            response_timeout: Duration::from_secs(5),
            channel_timeout: Duration::from_secs(10),
            poll_timeout: None,
            recv_batch: 32,
            max_send_queue: 256,
        }
    }
}

// ─── DispatchServer ──────────────────────────────────────────────────────────

/// Отступ перед пересозданием лобби после серьёзной ошибки (только путь
/// ошибки; прерывается остановкой сервера).
const LOBBY_RETRY_DELAY: Duration = Duration::from_millis(250);

/// Префикс пространства имён лобби, который наследует имя выделенного
/// канала: `Global\`, `Local\`, каталог NT-пути (`\...\`) или пусто (имя
/// без префикса -- неявный `Local\`, как у самого лобби).
fn channel_namespace_prefix(lobby: &str) -> &str {
    if lobby.starts_with("Global\\") {
        "Global\\"
    } else if lobby.starts_with("Local\\") {
        "Local\\"
    } else if lobby.starts_with('\\') {
        // `rfind` находит хотя бы ведущий `\`.
        lobby.rfind('\\').map_or("\\", |i| &lobby[..=i])
    } else {
        ""
    }
}

/// Имя мьютекса лобби (0.9): `<лобби>_lock` в пространстве имён лобби (те же
/// правила, что у секции и маяка).
fn lobby_lock_name(lobby: &str) -> String {
    if crate::naming::has_explicit_namespace(lobby) {
        format!("{lobby}_lock")
    } else {
        format!("Local\\{lobby}_lock")
    }
}

thread_local! {
    /// Сервер, в чьём потоке мы сейчас (поток лобби или колбэк обработчика
    /// в worker-е канала): адрес его карты клиентов, 0 -- ничей. По нему
    /// `DispatchServer::stop` узнаёт вызов из собственного потока.
    static SERVER_THREAD: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Метка «этот поток сейчас работает на сервер `owner`» на время области;
/// прежнее значение восстанавливается (колбэки могут вкладываться).
struct ServerThreadScope(usize);

impl ServerThreadScope {
    fn enter(owner: &ClientMap) -> Self {
        let id = Arc::as_ptr(owner) as usize;
        Self(SERVER_THREAD.with(|cell| cell.replace(id)))
    }
}

impl Drop for ServerThreadScope {
    fn drop(&mut self) {
        SERVER_THREAD.with(|cell| cell.set(self.0));
    }
}

/// Активный клиент на выделенном канале.
struct DispatchedClient {
    server: AutoServer,
    info: ClientRegistration,
    channel_name: String,
    /// Устанавливается в true, когда отключение уже обработано (предотвращает
    /// двойное уведомление). Общий с `AutoProxyHandler`: после отключения
    /// прокси больше не пересылает сообщения этого клиента.
    disconnected: Arc<AtomicBool>,
}

/// Общая карта клиентов, доступная и серверу, и proxy-обработчикам.
type ClientMap = Arc<RwLock<HashMap<u32, DispatchedClient>>>;

/// Центральный dispatch-сервер — одно лобби, динамические каналы на клиента.
///
/// Все потоки сервера (лобби, ожидание подключения к каналу, worker-ы
/// каналов) спят в ядре до события: подключение, данные, место, команда,
/// остановка. В простое -- ни одного пробуждения.
pub struct DispatchServer {
    base_name: String,
    clients: ClientMap,
    running: Arc<AtomicBool>,
    /// «Остановись» (безымянное событие с ручным сбросом): будит поток лобби
    /// и потоки ожидания подключения к каналу.
    stop_event: Arc<EventHandle>,
    next_client_id: Arc<AtomicU32>,
    worker_handle: Mutex<Option<JoinHandle<()>>>,
    /// Потоки, ожидающие подключения клиента к выделенному каналу (см.
    /// `handle_lobby_client`) -- обязаны быть заджойнены в `stop()` ДО
    /// возврата, иначе канал, так и не дождавшийся клиента, пережил бы
    /// сервер.
    pending_connects: Mutex<Vec<JoinHandle<()>>>,
    handler: Arc<dyn DispatchHandler>,
    options: DispatchOptions,
    /// Пространство имён лобби, которое наследуют имена каналов.
    channel_prefix: String,
    /// Возвраты потока лобби из ожидания в ядре (тесты: ноль в простое).
    lobby_wakeups: std::sync::atomic::AtomicU64,
}

impl DispatchServer {
    /// Запускает dispatch-сервер с лобби на заданном базовом имени.
    ///
    /// Лобби создаётся **синхронно**: после `Ok` клиенты уже могут
    /// регистрироваться (сервер может взводить маяк), а занятое имя (другой
    /// сервер с тем же лобби) -- ошибка здесь, а не молчаливые повторы в
    /// фоне.
    ///
    /// # Errors
    ///
    /// Имя лобби занято, ошибка ОС, поток не создан; NT-путь лобби слишком
    /// длинный, чтобы имя канала (каталог + 16 hex) уложилось в 64 байта
    /// ответа регистрации (`InvalidConfig`).
    pub fn start(
        name: &str,
        handler: Arc<dyn DispatchHandler>,
        options: DispatchOptions,
    ) -> Result<Arc<Self>> {
        let channel_prefix = channel_namespace_prefix(name).to_owned();
        if channel_prefix.len() + 16 > protocol::MAX_CHANNEL_NAME_LEN {
            return Err(ShmError::InvalidConfig(
                "lobby namespace prefix is too long for channel names (64 bytes)",
            ));
        }
        let lobby = SharedServer::start(name)?;
        let stop_event = Arc::new(EventHandle::create_unnamed(true)?);
        let running = Arc::new(AtomicBool::new(true));

        let server = Arc::new(Self {
            base_name: name.to_owned(),
            clients: Arc::new(RwLock::new(HashMap::new())),
            running,
            stop_event,
            next_client_id: Arc::new(AtomicU32::new(1)),
            worker_handle: Mutex::new(None),
            pending_connects: Mutex::new(Vec::new()),
            handler,
            options,
            channel_prefix,
            lobby_wakeups: std::sync::atomic::AtomicU64::new(0),
        });

        let server_clone = server.clone();
        let name_owned = name.to_owned();
        // Имя потока только в debug (короткий непрозрачный тег `xsd-{name}`,
        // чтобы локальные трейсы совпадали с сегментом), анонимно в release,
        // чтобы Process Explorer / Process Hacker не показывал
        // "xshm-dispatch-…" как заметный маркер в хост-процессе.
        #[cfg_attr(not(debug_assertions), allow(unused_mut))]
        let mut builder = thread::Builder::new();
        #[cfg(debug_assertions)]
        {
            builder = builder.name(format!("xsd-{name}"));
        }
        let handle = crate::thread_hook::spawn(builder, move || {
            server_clone.worker_loop(&name_owned, lobby);
        })
        .map_err(|e| ShmError::WindowsError {
            code: e.raw_os_error().unwrap_or(-1) as u32,
            context: "spawn dispatch worker",
        })?;

        *server.worker_handle.lock().unwrap() = Some(handle);

        Ok(server)
    }

    /// Отправляет сообщение конкретному клиенту.
    pub fn send_to(&self, client_id: u32, data: &[u8]) -> Result<()> {
        let clients = self.clients.read().unwrap();
        let client = clients.get(&client_id).ok_or(ShmError::NotConnected)?;
        client.server.send(data)
    }

    /// Отправка клиенту без потерь (backpressure): `Err(QueueFull)`, когда в
    /// очереди канала `max_send_queue` непереданных сообщений; принятое
    /// сообщение пишется в кольцо без перезаписи. Семантика --
    /// `AutoServer::try_send`; место освободилось --
    /// `DispatchHandler::on_space_available`.
    pub fn try_send_to(&self, client_id: u32, data: &[u8]) -> Result<()> {
        let clients = self.clients.read().unwrap();
        let client = clients.get(&client_id).ok_or(ShmError::NotConnected)?;
        client.server.try_send(data)
    }

    /// Жив ли процесс клиента (`AutoServer::is_peer_alive`); `None` -- клиента
    /// нет или он не наблюдается (клиент старой версии, нет прав).
    #[must_use]
    pub fn is_client_alive(&self, client_id: u32) -> Option<bool> {
        self.clients
            .read()
            .unwrap()
            .get(&client_id)
            .and_then(|c| c.server.is_peer_alive())
    }

    /// Пробуждения потока лобби и worker-ов всех каналов (тесты: ноль в простое).
    #[cfg(test)]
    pub(crate) fn wakeups(&self) -> u64 {
        self.lobby_wakeups.load(Ordering::Relaxed)
            + self
                .clients
                .read()
                .unwrap()
                .values()
                .map(|c| c.server.wakeups())
                .sum::<u64>()
    }

    /// Консервативная оценка места под новое сообщение клиенту
    /// (`AutoServer::free_space`); `None` -- клиента нет.
    #[must_use]
    pub fn free_space(&self, client_id: u32) -> Option<FreeSpace> {
        self.clients
            .read()
            .unwrap()
            .get(&client_id)
            .map(|c| c.server.free_space())
    }

    /// Рассылает сообщение всем подключённым клиентам.
    pub fn broadcast(&self, data: &[u8]) -> Result<u32> {
        let clients = self.clients.read().unwrap();
        let mut sent = 0u32;
        for client in clients.values() {
            if client.server.send(data).is_ok() {
                sent += 1;
            }
        }
        Ok(sent)
    }

    /// Отключает конкретного клиента и уничтожает его канал.
    ///
    /// Сообщения, принятые `send_to`/`try_send_to` до этого вызова, worker
    /// канала дописывает в кольцо перед закрытием (сколько влезет), а
    /// клиент дочитывает их до своего `on_disconnect` -- так доходит,
    /// например, прощальное сообщение с причиной отключения.
    ///
    /// Можно звать из колбэков `DispatchHandler` (в том числе из
    /// `on_client_connect` этого же клиента): дроп канала из его
    /// собственного worker-а не ждёт сам себя.
    pub fn disconnect_client(&self, client_id: u32) -> Result<()> {
        let removed = self.clients.write().unwrap().remove(&client_id);
        if let Some(client) = removed {
            // Помечаем как отключённого, чтобы AutoProxyHandler не уведомил повторно
            client.disconnected.store(true, Ordering::Release);
            client.server.stop();
            drop(client);
            self.handler
                .on_client_disconnect_reason(client_id, DisconnectReason::Local);
            Ok(())
        } else {
            Err(ShmError::NotConnected)
        }
    }

    /// Возвращает список ID подключённых клиентов.
    pub fn connected_clients(&self) -> Vec<u32> {
        self.clients.read().unwrap().keys().copied().collect()
    }

    /// Возвращает количество подключённых клиентов.
    pub fn client_count(&self) -> u32 {
        self.clients.read().unwrap().len() as u32
    }

    /// Проверяет, подключён ли конкретный клиент.
    pub fn is_client_connected(&self, client_id: u32) -> bool {
        self.clients.read().unwrap().contains_key(&client_id)
    }

    /// Возвращает данные регистрации клиента.
    pub fn client_info(&self, client_id: u32) -> Option<ClientRegistration> {
        self.clients
            .read()
            .unwrap()
            .get(&client_id)
            .map(|c| c.info.clone())
    }

    /// Возвращает имя канала клиента. Названо `channel_name` (не `client_channel`)
    /// для единообразия с `MultiServer::channel_name` (0.6.0, аудит API).
    pub fn channel_name(&self, client_id: u32) -> Option<String> {
        self.clients
            .read()
            .unwrap()
            .get(&client_id)
            .map(|c| c.channel_name.clone())
    }

    /// Останавливает dispatch-сервер и все клиентские каналы.
    ///
    /// Синхронно дожидается выхода lobby worker-потока И всех "pending
    /// connect" потоков (см. `handle_lobby_client`) перед возвратом — после
    /// return ни один callback (`on_client_connect`/`on_message`/`on_error`/…)
    /// больше не будет вызван -- вызывающий может сразу после возврата
    /// освободить состояние, которым владеет handler. Идемпотентна (повторный
    /// вызов — no-op, обе очереди handle-ов уже опустошены).
    ///
    /// **Из колбэка `DispatchHandler`** (или другого потока самого сервера)
    /// `stop()` не блокируется (0.9, ревизия 2): синхронный join был бы
    /// взаимной блокировкой -- поток лобби при остановке join-ит worker-ы
    /// каналов, а один из них и есть вызывающий. Тогда остановка
    /// ОТЛОЖЕННАЯ: сервер помечен остановленным, потоки разбужены, каналы
    /// закроет поток лобби, как только колбэк вернётся; гарантия «после
    /// возврата колбэков больше не будет» в этом случае НЕ действует --
    /// дождаться полной остановки можно повторным `stop()`/Drop из чужого
    /// потока.
    pub fn stop(&self) {
        self.shutdown();
    }

    fn shutdown(&self) {
        self.running.store(false, Ordering::Release);
        // Будит лобби и потоки ожидания подключения: они спят в ядре без
        // таймаута и иначе не заметили бы остановку.
        let _ = self.stop_event.set();
        // Вызов из собственного потока (колбэк в worker-е канала или поток
        // лобби): join-ить нельзя -- поток лобби join-ит worker-ы каналов, в
        // одном из которых мы сейчас. Остановка отложенная: хвост
        // `worker_loop` закроет каналы, join -- следующий stop()/Drop из
        // чужого потока.
        let own = Arc::as_ptr(&self.clients) as usize;
        if SERVER_THREAD.with(std::cell::Cell::get) == own {
            return;
        }
        // Джойнится потоком-владельцем Self. Тот же фикс, что и для
        // MultiServer::stop() (аудит 2026-07-10): worker держит собственный
        // клон Arc<Self>, поэтому расчёт только на Drop гонял бы точно так же.
        if let Some(handle) = self.worker_handle.lock().unwrap().take()
            && handle.thread().id() != thread::current().id()
        {
            let _ = handle.join();
        }
        // Lobby worker уже остановлен -> новых pending-connect потоков не
        // появится, можно безопасно забрать и заджойнить все существующие.
        // Каждый из них проснётся по `stop_event` и остановит свой так и не
        // зарегистрированный канал.
        let pending: Vec<_> = self.pending_connects.lock().unwrap().drain(..).collect();
        for handle in pending {
            let _ = handle.join();
        }
    }

    /// Базовое имя dispatch-сервера.
    pub fn base_name(&self) -> &str {
        &self.base_name
    }

    /// Генерирует уникальное И непредсказуемое имя выделенного канала.
    ///
    /// Раньше имя строилось детерминированно из `time_nanos XOR counter`
    /// (с SplitMix64-подобным domainMixing) и заявлялось в докстринге как
    /// "cryptographic-quality" -- заявление было НЕВЕРНЫМ: оба входа
    /// наблюдаемы враждебным локальным процессом (`client_id`/`counter`
    /// приходит клиенту открытым текстом в `RegistrationResponse`; момент
    /// регистрации оценивается по факту с точностью до миллисекунд) --
    /// перебор ~10^6 кандидатов по узкому временному окну тривиален.
    /// Squatting-риск: чужой процесс заранее вычисляет будущее имя и создаёт
    /// секцию с этим именем ПЕРВЫМ (`NtCreateSection`), либо срывая канал
    /// легитимному серверу, либо подставляя себя как "сервер" ничего не
    /// подозревающему клиенту. Актуально даже при NULL DACL — это риск не
    /// про чтение чужих данных (то и так открыто), а про то, кто первым
    /// займёт имя (аудит 2026-07-10).
    ///
    /// Фикс: `RandomState` (std, без внешних зависимостей) сидируется свежей
    /// ОС-энтропией на КАЖДЫЙ вызов — атакующему больше не из чего вычислить
    /// имя заранее, в отличие от детерминированной time/counter-схемы.
    fn generate_channel_name(&self) -> String {
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher, Hasher};

        let counter = self.next_client_id.load(Ordering::Relaxed) as u64;
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(counter);
        // 0.9: в пространстве имён лобби (см. `channel_namespace_prefix`).
        format!("{}{:016x}", self.channel_prefix, hasher.finish())
    }

    /// Главный worker loop — принимает клиентов через лобби.
    ///
    /// Сам lobby-handshake (single-client протокол) неизбежно последователен,
    /// но обрабатывается быстро (только чтение регистрации + ответ). Ожидание
    /// подключения клиента к его выделенному каналу (до `channel_connect_timeout`,
    /// по умолчанию 30с) вынесено в отдельный поток (см. `handle_lobby_client`),
    /// поэтому один медленный клиент больше не блокирует регистрацию остальных.
    fn worker_loop(&self, base_name: &str, lobby: SharedServer) {
        // Поток лобби зовёт `on_error`/`on_client_disconnect_reason`:
        // `stop()` оттуда не должен join-ить сам себя.
        let _scope = ServerThreadScope::enter(&self.clients);
        let mut buffer = Vec::with_capacity(MAX_MESSAGE_SIZE);
        let mut first = Some(lobby);

        while self.running.load(Ordering::Acquire) {
            // Лобби создано в `start`; пересоздаётся только после серьёзной
            // ошибки (с отступом, прерываемым остановкой).
            let mut lobby_server = match first.take() {
                Some(lobby) => lobby,
                None => match SharedServer::start(base_name) {
                    Ok(s) => s,
                    Err(err) => {
                        self.handler.on_error(None, err);
                        if !wait_delay_or(&self.running, &self.stop_event, LOBBY_RETRY_DELAY) {
                            break;
                        }
                        continue;
                    }
                },
            };

            // Внутренний цикл: последовательный приём клиентов через лобби.
            // Ждём CONNECT_REQ или остановку -- без опроса по таймеру.
            while self.running.load(Ordering::Acquire) {
                let accepted = lobby_server
                    .wait_for_client_or(self.stop_event.raw_handle(), self.options.poll_timeout);
                self.lobby_wakeups.fetch_add(1, Ordering::Relaxed);
                match accepted {
                    Ok(true) => {
                        // Клиент подключился — обрабатываем регистрацию
                        self.handle_lobby_client(&mut lobby_server, &mut buffer);

                        // Сбрасываем лобби для следующего клиента
                        // (`client_state` -- CAS: HELLO следующего клиента,
                        // вошедшего по мьютексу лобби, не затирается).
                        lobby_server.mark_disconnected();
                    }
                    Ok(false) => continue,
                    Err(ShmError::AlreadyConnected) => {
                        lobby_server.mark_disconnected();
                    }
                    // `CONNECT_REQ` без `CLIENT_HELLO` (клиент откатил заявку
                    // по таймауту): рукопожатия не было, лобби цело --
                    // пересоздавать нечего (пересоздание к тому же упёрлось
                    // бы в занятое имя, пока клиенты держат секцию).
                    Err(ShmError::HandshakeFailed) => continue,
                    Err(err) => {
                        self.handler.on_error(None, err);
                        break; // Пересоздаём лобби при серьёзной ошибке
                    }
                }
            }
        }

        // Остановка: закрываем все клиентские каналы. Карту опустошаем под
        // логом, а каналы останавливаем и дропаем (join их worker-ов) уже без
        // него: worker канала в этот момент может сам ждать лог (прокси в
        // `on_connect`/`on_disconnect_reason`) -- join под логом был бы
        // взаимной блокировкой. `on_client_disconnect_reason` -- строго после
        // join: колбэки этого клиента (включая `on_client_connect`) к этому
        // моменту завершены.
        let drained: Vec<(u32, DispatchedClient)> = self.clients.write().unwrap().drain().collect();
        for (id, client) in drained {
            client.disconnected.store(true, Ordering::Release);
            client.server.stop();
            drop(client);
            self.handler
                .on_client_disconnect_reason(id, DisconnectReason::Local);
        }
    }

    /// Обрабатывает одного клиента в лобби: читает регистрацию, создаёт канал, отвечает.
    ///
    /// Возвращается СРАЗУ после отправки ответа клиенту (не дожидаясь его
    /// подключения к выделенному каналу) — вызывающий код (`worker_loop`)
    /// тут же сбрасывает лобби и готов принимать следующего клиента.
    /// Регистрация в `self.clients` и `on_client_connect` происходят в
    /// worker-е канала при фактическом подключении (`AutoProxyHandler::
    /// on_connect`) -- ДО первого `on_message` этого клиента; отдельный поток
    /// лишь убирает канал, если клиент не подключился до
    /// `channel_connect_timeout` или сервер остановлен.
    fn handle_lobby_client(&self, lobby: &mut SharedServer, buffer: &mut Vec<u8>) {
        let Some(events) = lobby.events() else {
            self.handler.on_error(None, ShmError::NotReady);
            return;
        };

        // Ожидаем данные регистрации через событие c2s.data (по событию, без
        // опроса; остановка сервера будит сразу).
        let start = std::time::Instant::now();
        let request = loop {
            let remaining = self.options.lobby_timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                self.handler.on_error(None, ShmError::Timeout);
                return;
            }

            if !self.running.load(Ordering::Acquire) {
                return;
            }

            // Сначала пробуем прочитать — данные могли уже оказаться в буфере
            match lobby.receive_from_client(buffer) {
                Ok(len) => match protocol::decode_request(&buffer[..len]) {
                    Ok(req) => break req,
                    Err(err) => {
                        self.handler.on_error(None, err);
                        return;
                    }
                },
                Err(ShmError::QueueEmpty) => {
                    // Блокируемся на событии — просыпаемся, когда клиент
                    // запишет данные, по остановке или по смерти клиента
                    // (0.9: не ждём `lobby_timeout` за упавшим клиентом).
                    let mut handles = [
                        events.c2s.data.raw_handle(),
                        self.stop_event.raw_handle(),
                        0,
                    ];
                    let count = match lobby.peer_wait_handle() {
                        Some(peer) => {
                            handles[2] = peer;
                            3
                        }
                        None => 2,
                    };
                    let woke = win::wait_any(&handles[..count], Some(remaining));
                    self.lobby_wakeups.fetch_add(1, Ordering::Relaxed);
                    if woke == Ok(Some(2)) {
                        // Клиент умер посреди регистрации: канал ему не нужен
                        // (даже если запрос успел лечь в кольцо).
                        self.handler.on_error(None, ShmError::PeerDied);
                        return;
                    }
                    continue;
                }
                Err(err) => {
                    self.handler.on_error(None, err);
                    return;
                }
            }
        };

        let client_id = self.next_client_id.fetch_add(1, Ordering::Relaxed);
        let channel_name = self.generate_channel_name();

        let info = ClientRegistration {
            pid: request.pid,
            revision: request.revision,
            name: request.name,
        };

        // «Клиент зарегистрирован» -- сигнал потоку ожидания (ручной сброс).
        let registered = match EventHandle::create_unnamed(true) {
            Ok(ev) => Arc::new(ev),
            Err(err) => {
                self.handler.on_error(None, err);
                self.reject(lobby);
                return;
            }
        };
        // AutoServer канала до регистрации: забирает либо прокси при
        // подключении клиента, либо поток ожидания по таймауту/остановке.
        let slot: Arc<Mutex<Option<AutoServer>>> = Arc::new(Mutex::new(None));

        let proxy_handler = Arc::new(AutoProxyHandler {
            client_id,
            info,
            channel_name: channel_name.clone(),
            handler: self.handler.clone(),
            clients: Arc::clone(&self.clients),
            running: Arc::clone(&self.running),
            slot: Arc::clone(&slot),
            registered_event: Arc::clone(&registered),
            registered: AtomicBool::new(false),
            disconnected: Arc::new(AtomicBool::new(false)),
        });

        let auto_options = AutoOptions {
            connect_timeout: self.options.channel_connect_timeout,
            poll_timeout: self.options.poll_timeout,
            recv_batch: self.options.recv_batch,
            ..AutoOptions::default()
        };

        // Слот держим, пока AutoServer не положен в него: иначе прокси, чей
        // worker стартует сразу, мог бы не найти канал при очень быстром
        // подключении клиента (клиент подключится только после ответа
        // лобби, но порядок гарантируем явно).
        let mut slot_guard = slot.lock().unwrap();
        match AutoServer::start(&channel_name, proxy_handler, auto_options) {
            Ok(s) => *slot_guard = Some(s),
            Err(err) => {
                drop(slot_guard);
                self.handler.on_error(None, err);
                self.reject(lobby);
                return;
            }
        }
        drop(slot_guard);

        // Отправляем клиенту назначенный канал через лобби
        let response = protocol::encode_response(&RegistrationResponse {
            status: protocol::STATUS_OK,
            client_id,
            channel_name,
        });

        if let Err(err) = lobby.send_to_client(&response) {
            self.handler.on_error(None, err);
            let taken = slot.lock().unwrap().take();
            if let Some(server) = taken {
                server.stop();
            }
            return;
        }

        // Сигналим о доступности данных в лобби, чтобы клиент мог их прочитать
        if let Some(events) = lobby.events() {
            let _ = events.s2c.data.set();
        }

        // Поток ожидания: клиент не подключился к каналу до
        // `channel_connect_timeout` (по умолчанию 30с) или сервер
        // остановлен -- канал убрать. Всё по событиям (регистрация /
        // остановка), таймаут -- только дедлайн. Отдельный поток, чтобы лобби
        // было готово к следующему клиенту сразу.
        let stop_event = Arc::clone(&self.stop_event);
        let channel_connect_timeout = self.options.channel_connect_timeout;
        let join_handle = crate::thread_hook::spawn(thread::Builder::new(), move || {
            let _ = win::wait_any(
                &[registered.raw_handle(), stop_event.raw_handle()],
                Some(channel_connect_timeout),
            );
            // Зарегистрированный канал прокси уже забрал из слота; остался --
            // клиент не пришёл: остановить (join worker-а -- из этого потока,
            // не из самого worker-а).
            let taken = slot.lock().unwrap().take();
            if let Some(server) = taken {
                server.stop();
                drop(server);
            }
        })
        .expect("failed to spawn thread");

        // Регистрируем handle для join'а в stop(); заодно вычищаем уже
        // завершившиеся записи, чтобы вектор не рос неограниченно на
        // долгоживущем сервере с большим потоком регистраций.
        let mut pending = self.pending_connects.lock().unwrap();
        pending.retain(|h| !h.is_finished());
        pending.push(join_handle);
    }

    /// Отказ в регистрации: ответ `STATUS_REJECTED` в лобби.
    fn reject(&self, lobby: &mut SharedServer) {
        let reject = protocol::encode_response(&RegistrationResponse {
            status: protocol::STATUS_REJECTED,
            client_id: 0,
            channel_name: String::new(),
        });
        let _ = lobby.send_to_client(&reject);
        if let Some(events) = lobby.events() {
            let _ = events.s2c.data.set();
        }
    }
}

impl std::fmt::Debug for DispatchServer {
    /// Ручная реализация: `handler` -- `Arc<dyn DispatchHandler>` без `Debug`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DispatchServer")
            .field("base_name", &self.base_name)
            .field("clients", &self.client_count())
            .field("running", &self.running.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl Drop for DispatchServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ─── AutoProxyHandler — bridges AutoServer events to DispatchHandler ─────────

struct AutoProxyHandler {
    client_id: u32,
    info: ClientRegistration,
    channel_name: String,
    handler: Arc<dyn DispatchHandler>,
    clients: ClientMap,
    /// `running` сервера: после остановки новые клиенты не регистрируются.
    running: Arc<AtomicBool>,
    /// AutoServer канала до регистрации (см. `handle_lobby_client`).
    slot: Arc<Mutex<Option<AutoServer>>>,
    /// Сигнал потоку ожидания: клиент зарегистрирован.
    registered_event: Arc<EventHandle>,
    /// Клиент зарегистрирован и объявлен (`on_client_connect` вызван).
    registered: AtomicBool,
    /// Отключение обработано (общий с `DispatchedClient`).
    disconnected: Arc<AtomicBool>,
}

impl AutoProxyHandler {
    /// Клиент объявлен и ещё не отключён: только тогда его события уходят
    /// в `DispatchHandler`.
    fn is_active(&self) -> bool {
        self.registered.load(Ordering::Acquire) && !self.disconnected.load(Ordering::Acquire)
    }
}

impl AutoHandler for AutoProxyHandler {
    fn on_connect(&self) {
        let _scope = ServerThreadScope::enter(&self.clients);
        // Регистрация -- здесь, в worker-е канала, ДО обработки его первого
        // сообщения: раньше `on_client_connect` звал отдельный поток уже
        // после того, как worker мог доставить первые сообщения, и они
        // уходили в `on_message` клиента, о котором обработчик ещё не знал
        // (и терялись).
        {
            let mut clients = self.clients.write().unwrap();
            // Сервер останавливается: не регистрировать -- поток ожидания
            // проснётся по остановке и уберёт канал сам.
            if !self.running.load(Ordering::Acquire) {
                return;
            }
            // Нет в слоте: поток ожидания уже убрал канал по таймауту, или
            // это повторное подключение к тому же каналу -- не объявлять.
            let Some(server) = self.slot.lock().unwrap().take() else {
                return;
            };
            clients.insert(
                self.client_id,
                DispatchedClient {
                    server,
                    info: self.info.clone(),
                    channel_name: self.channel_name.clone(),
                    disconnected: Arc::clone(&self.disconnected),
                },
            );
        }
        self.registered.store(true, Ordering::Release);
        let _ = self.registered_event.set();
        self.handler.on_client_connect(self.client_id, &self.info);
    }

    fn on_disconnect_reason(&self, reason: DisconnectReason) {
        let _scope = ServerThreadScope::enter(&self.clients);
        // Проверяем, не обработано ли уже (например, через disconnect_client())
        //
        // ВАЖНО: `clients.remove(...)` результат обязательно привязывается к
        // переменной (`removed`), а не дропается тут же внутри блока с
        // write-логом. `on_disconnect` вызывается СИНХРОННО из worker-потока
        // САМОГО AutoServer'а этого клиента; удаляемый `DispatchedClient`
        // содержит этот же `AutoServer` (поле `server`), а его `Drop`
        // синхронно джойнит свой `worker_handle` -- т.е. текущий поток. Дропни
        // мы его прямо здесь (тем более всё ещё под логом) -- self-join
        // deadlock, лог остаётся захваченным навсегда (аудит 2026-07-10).
        let removed = {
            let mut clients = self.clients.write().unwrap();
            if let Some(client) = clients.get(&self.client_id) {
                if client
                    .disconnected
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    clients.remove(&self.client_id)
                } else {
                    None
                }
            } else {
                None
            }
        }; // write-лог освобождён здесь; `removed` (если Some) ещё жив,
        // AutoServer внутри него ещё НЕ дропнут.

        if let Some(dispatched_client) = removed {
            // Фактический Drop (и его синхронный join) переносим на ОТДЕЛЬНЫЙ
            // поток -- он не является worker-потоком этого AutoServer, поэтому
            // join там безопасен и не self-join'ится.
            crate::thread_hook::spawn(thread::Builder::new(), move || drop(dispatched_client))
                .expect("failed to spawn thread");
            self.handler
                .on_client_disconnect_reason(self.client_id, reason);
        }
    }

    fn on_message(&self, _direction: ChannelKind, payload: &[u8]) {
        let _scope = ServerThreadScope::enter(&self.clients);
        if self.is_active() {
            self.handler.on_message(self.client_id, payload);
        }
    }

    fn on_space_available(&self, _direction: ChannelKind) {
        let _scope = ServerThreadScope::enter(&self.clients);
        if self.is_active() {
            self.handler.on_space_available(self.client_id);
        }
    }

    fn on_error(&self, err: ShmError) {
        let _scope = ServerThreadScope::enter(&self.clients);
        if self.is_active() {
            self.handler.on_error(Some(self.client_id), err);
        }
    }
}

// ─── DispatchClient ──────────────────────────────────────────────────────────

/// Клиент, подключающийся к DispatchServer, регистрирующийся и общающийся
/// по динамически назначенному каналу.
///
/// Жизненный цикл: подключение один раз → регистрация → получение канала →
/// обмен данными → остановка. НЕ переподключается автоматически — при
/// отключении нужно создать нового клиента.
pub struct DispatchClient {
    /// Разделяется с `DispatchClientProxy`: при разрыве канала прокси сам
    /// забирает отсюда `AutoClient` и роняет его, останавливая бесконечные
    /// попытки переподключения (см. `DispatchClientProxy::on_disconnect`).
    auto_client: Arc<Mutex<Option<AutoClient>>>,
    running: Arc<AtomicBool>,
    client_id: u32,
    channel_name: String,
    /// Причина разрыва канала (заполняет прокси; `None` -- канал жив или
    /// остановлен локально через `stop()`).
    last_reason: Arc<Mutex<Option<DisconnectReason>>>,
    /// PID сервера из рукопожатия канала (0 -- ещё нет / сервер без PID);
    /// публикует worker канала до `on_connect`, при разрыве не стирается.
    server_pid: Arc<AtomicU32>,
}

impl DispatchClient {
    /// Подключается к dispatch-серверу, регистрируется и начинает обмен данными.
    ///
    /// Это блокирующий вызов — синхронно выполняет handshake в лобби, затем
    /// поднимает выделенный канал через AutoClient.
    pub fn connect(
        name: &str,
        registration: ClientRegistration,
        handler: Arc<dyn DispatchClientHandler>,
        options: DispatchClientOptions,
    ) -> Result<Self> {
        // Фаза 1: подключение к лобби и регистрация (блокирующая)
        let mut buffer = Vec::with_capacity(MAX_MESSAGE_SIZE);
        let (assigned_id, assigned_channel) =
            lobby_register(name, &registration, &options, &mut buffer)?;

        // Фаза 2: подключение к выделенному каналу через AutoClient
        let running = Arc::new(AtomicBool::new(true));
        let slot: Arc<Mutex<Option<AutoClient>>> = Arc::new(Mutex::new(None));
        let last_reason = Arc::new(Mutex::new(None));

        let client_handler = Arc::new(DispatchClientProxy {
            handler: handler.clone(),
            running: Arc::clone(&running),
            slot: Arc::clone(&slot),
            client_id: assigned_id,
            channel_name: assigned_channel.clone(),
            last_reason: Arc::clone(&last_reason),
        });

        let auto_options = AutoOptions {
            connect_timeout: options.channel_timeout,
            poll_timeout: options.poll_timeout,
            max_send_queue: options.max_send_queue,
            recv_batch: options.recv_batch,
            ..AutoOptions::default()
        };

        // Ревизия 2: выделенный канал не переподключается -- неудача первого
        // подключения завершает клиента (`on_disconnect_reason(Error)`), а
        // не крутит повторы к каналу, которого уже нет.
        let server_pid = Arc::new(AtomicU32::new(0));
        let auto_client = AutoClient::connect_dedicated(
            &assigned_channel,
            client_handler,
            auto_options,
            Arc::clone(&server_pid),
        )?;
        *slot.lock().unwrap() = Some(auto_client);

        // `on_connect` больше НЕ вызывается здесь: возврат `AutoClient::connect`
        // означает лишь "worker-поток запущен", а не "канал поднят". Уведомление
        // приходит из `DispatchClientProxy::on_connect`, когда handshake на
        // выделенном канале реально завершён (аудит 2026-07-28) -- симметрично
        // серверу, который ждёт фактического подключения перед
        // `on_client_connect`.

        Ok(Self {
            auto_client: slot,
            running,
            client_id: assigned_id,
            channel_name: assigned_channel,
            last_reason,
            server_pid,
        })
    }

    /// Жив ли процесс сервера (viewer-а): `Some(true)` -- канал поднят и
    /// сервер наблюдается через удерживаемый handle процесса; `Some(false)` --
    /// канал разорван из-за смерти сервера; `None` -- неизвестно (канал ещё
    /// не поднят, сервер старой версии, нет прав, остановлен локально).
    #[must_use]
    pub fn is_peer_alive(&self) -> Option<bool> {
        if let Some(client) = self.auto_client.lock().unwrap().as_ref() {
            return client.is_peer_alive();
        }
        (self.disconnect_reason() == Some(DisconnectReason::PeerDied)).then_some(false)
    }

    /// PID процесса сервера из рукопожатия выделенного канала (сервер 0.8+
    /// опубликовал его и процесс открылся). Известен с `on_connect` и (0.9,
    /// ревизия 2) остаётся и после разрыва канала -- отказанному клиенту
    /// есть чей выход ждать, даже если сервер отключил его сразу после
    /// рукопожатия. `None` -- канал ещё не поднимался или PID нет.
    #[must_use]
    pub fn server_pid(&self) -> Option<u32> {
        Some(self.server_pid.load(Ordering::Acquire)).filter(|&pid| pid != 0)
    }

    /// Почему канал разорван со стороны сервера (`Graceful`/`PeerDied`/`Error`);
    /// `None` -- канал жив или остановлен локально.
    #[must_use]
    pub fn disconnect_reason(&self) -> Option<DisconnectReason> {
        *self.last_reason.lock().unwrap()
    }

    /// Отправляет сообщение серверу по выделенному каналу.
    pub fn send(&self, data: &[u8]) -> Result<()> {
        if !self.running.load(Ordering::Acquire) {
            return Err(ShmError::NotReady);
        }
        let guard = self.auto_client.lock().unwrap();
        match guard.as_ref() {
            Some(client) => client.send(data),
            None => Err(ShmError::NotConnected),
        }
    }

    /// Отправка серверу **без потерь** (backpressure).
    ///
    /// `Ok(())` -- сообщение принято и будет записано в кольцо без
    /// перезаписи непрочитанного; `Err(QueueFull)` -- в очереди уже
    /// `DispatchClientOptions::max_send_queue` непереданных сообщений (сервер
    /// не успевает читать), ничего не принято; `MessageTooSmall`/
    /// `MessageTooLarge` -- синхронно; `NotReady`/`NotConnected` -- канал
    /// остановлен или разорван. Семантика -- `AutoServer::try_send`.
    pub fn try_send(&self, data: &[u8]) -> Result<()> {
        if !self.running.load(Ordering::Acquire) {
            return Err(ShmError::NotReady);
        }
        let guard = self.auto_client.lock().unwrap();
        match guard.as_ref() {
            Some(client) => client.try_send(data),
            None => Err(ShmError::NotConnected),
        }
    }

    /// Консервативная оценка места под новое сообщение серверу
    /// (`AutoClient::free_space`); после разрыва канала -- `FreeSpace::ZERO`.
    #[must_use]
    pub fn free_space(&self) -> FreeSpace {
        self.auto_client
            .lock()
            .unwrap()
            .as_ref()
            .map_or(FreeSpace::ZERO, AutoClient::free_space)
    }

    /// Есть ли сейчас лобби `name` (сервер работает): разовая проверка без
    /// подключения. Клиенту, проснувшемуся по взведённому `Beacon`, она
    /// отличает «сервер упал, не сбросив маяк» (лобби нет -- сбросить маяк и
    /// ждать нового взвода) от временной ошибки регистрации.
    #[must_use]
    pub fn lobby_exists(name: &str) -> bool {
        Mapping::open(&mapping_name(name)).is_ok()
    }

    /// Пробуждения worker-а канала (тесты: ноль в простое).
    #[cfg(test)]
    pub(crate) fn wakeups(&self) -> u64 {
        self.auto_client
            .lock()
            .unwrap()
            .as_ref()
            .map_or(0, AutoClient::wakeups)
    }

    /// Возвращает назначенный ID клиента.
    #[must_use]
    pub const fn client_id(&self) -> u32 {
        self.client_id
    }

    /// Возвращает имя назначенного канала.
    #[must_use]
    pub fn channel_name(&self) -> &str {
        &self.channel_name
    }

    /// Проверяет, жив ли выделенный канал.
    ///
    /// Становится `false` не только после `stop()`, но и при разрыве канала со
    /// стороны сервера -- прокси гасит `running` в `on_disconnect`.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.running.load(Ordering::Acquire) && self.auto_client.lock().unwrap().is_some()
    }

    /// Останавливает клиента и отключается.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
        Self::shutdown_channel(&self.auto_client);
    }

    /// Забирает `AutoClient` из общего слота и роняет его УЖЕ ПОСЛЕ
    /// освобождения mutex-а.
    ///
    /// Порядок принципиален: `Drop for AutoClient` синхронно джойнит свой
    /// worker-поток, а тот в этот момент может выполнять
    /// `DispatchClientProxy::on_disconnect`, которому нужен тот же mutex --
    /// дроп под захваченным lock-ом дал бы взаимную блокировку.
    fn shutdown_channel(slot: &Arc<Mutex<Option<AutoClient>>>) {
        let taken = slot.lock().unwrap().take();
        if let Some(client) = taken {
            client.stop();
            drop(client);
        }
    }
}

impl std::fmt::Debug for DispatchClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DispatchClient")
            .field("client_id", &self.client_id)
            .field("channel_name", &self.channel_name)
            .field("connected", &self.is_connected())
            .finish_non_exhaustive()
    }
}

impl Drop for DispatchClient {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        Self::shutdown_channel(&self.auto_client);
    }
}

// ─── Регистрация в лобби (блокирующая) ──────────────────────────────────────

/// Выполняет регистрацию в лобби: подключение, отправка запроса, чтение ответа.
///
/// 0.9: всё рукопожатие с лобби -- под именованным мьютексом `<лобби>_lock`
/// (`lobby_lock_name`). Лобби -- одноклиентский SPSC-канал: без мьютекса два
/// клиента, пришедшие одновременно (например, разбуженные одним взводом
/// маяка), проходили рукопожатие оба, писали в одно кольцо и читали чужой
/// ответ. Мьютекс держится до отключения от лобби (Drop `SharedClient`) и
/// освобождается тем же потоком; упавший владелец оставляет мьютекс
/// «брошенным» -- следующий клиент получает его сразу (`STATUS_ABANDONED`).
fn lobby_register(
    base_name: &str,
    registration: &ClientRegistration,
    options: &DispatchClientOptions,
    buffer: &mut Vec<u8>,
) -> Result<(u32, String)> {
    let lock = NamedMutex::open_or_create(&lobby_lock_name(base_name))?;
    // Порядок объявлений важен: `client` дропается раньше `_turn` --
    // мьютекс отпускается только после отключения от лобби.
    let Some(_turn) = lock.lock(Some(options.lobby_timeout))? else {
        return Err(ShmError::Timeout);
    };
    let client = SharedClient::connect(base_name, options.lobby_timeout)?;

    // Отправляем запрос на регистрацию
    let request = protocol::encode_request(&RegistrationRequest {
        pid: registration.pid,
        revision: registration.revision,
        name: registration.name.clone(),
    });
    client.send_to_server(&request)?;

    // Сигналим серверу о наличии данных через событие
    let _ = client.events().c2s.data.set();

    // Ожидаем ответ через событие s2c.data (по событию, без опроса)
    let events = client.events();
    let start = std::time::Instant::now();
    loop {
        let remaining = options.response_timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return Err(ShmError::Timeout);
        }

        // Сначала пробуем прочитать — данные могли уже оказаться в буфере
        match client.receive_from_server(buffer) {
            Ok(len) => {
                let response = protocol::decode_response(&buffer[..len])?;
                if response.status != protocol::STATUS_OK {
                    return Err(ShmError::HandshakeFailed);
                }
                // Дропаем client — отключаемся от лобби
                drop(client);
                return Ok((response.client_id, response.channel_name));
            }
            Err(ShmError::QueueEmpty) => {
                // Блокируемся на событии — просыпаемся, когда сервер запишет
                // ответ (или истечёт дедлайн).
                let _ = events.s2c.data.wait(Some(remaining));
                continue;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Proxy-обработчик, пересылающий события AutoClient в DispatchClientHandler.
///
/// Дополнительно приводит поведение к задокументированному контракту
/// `DispatchClient` (не переподключается автоматически): при разрыве канала
/// гасит `running` и роняет `AutoClient` (аудит 2026-07-28). Сам `AutoClient`
/// выделенного канала создаётся без переподключения
/// (`AutoClient::connect_dedicated`, ревизия 2): неудачное ПЕРВОЕ
/// подключение тоже приходит сюда как `on_disconnect_reason(Error)` -- раньше
/// оно давало вечные повторы раз в 250 мс, `is_connected() == true` и ни
/// одного `on_disconnect`.
struct DispatchClientProxy {
    handler: Arc<dyn DispatchClientHandler>,
    running: Arc<AtomicBool>,
    slot: Arc<Mutex<Option<AutoClient>>>,
    client_id: u32,
    channel_name: String,
    last_reason: Arc<Mutex<Option<DisconnectReason>>>,
}

impl AutoHandler for DispatchClientProxy {
    fn on_connect(&self) {
        self.handler.on_connect(self.client_id, &self.channel_name);
    }

    fn on_disconnect_reason(&self, reason: DisconnectReason) {
        *self.last_reason.lock().unwrap() = Some(reason);
        self.running.store(false, Ordering::Release);
        // Вызывается СИНХРОННО из worker-потока самого AutoClient. Забираем
        // его из слота и роняем: `Drop for AutoClient` распознаёт self-join
        // (`join_unless_self`) и просто открепляет поток, поэтому deadlock-а
        // нет, а бесконечный reconnect прекращается.
        let taken = self.slot.lock().unwrap().take();
        drop(taken);
        self.handler.on_disconnect_reason(reason);
    }

    fn on_message(&self, _direction: ChannelKind, payload: &[u8]) {
        self.handler.on_message(payload);
    }

    fn on_space_available(&self, _direction: ChannelKind) {
        self.handler.on_space_available();
    }

    fn on_error(&self, err: ShmError) {
        self.handler.on_error(err);
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    struct TestServerHandler {
        connects: AtomicU32,
        disconnects: AtomicU32,
        messages: AtomicU32,
        last_pid: AtomicU32,
    }

    impl TestServerHandler {
        fn new() -> Self {
            Self {
                connects: AtomicU32::new(0),
                disconnects: AtomicU32::new(0),
                messages: AtomicU32::new(0),
                last_pid: AtomicU32::new(0),
            }
        }
    }

    impl DispatchHandler for TestServerHandler {
        fn on_client_connect(&self, _client_id: u32, info: &ClientRegistration) {
            self.connects.fetch_add(1, Ordering::Relaxed);
            self.last_pid.store(info.pid, Ordering::Relaxed);
        }
        fn on_client_disconnect(&self, _client_id: u32) {
            self.disconnects.fetch_add(1, Ordering::Relaxed);
        }
        fn on_message(&self, _client_id: u32, _data: &[u8]) {
            self.messages.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct TestClientHandler {
        connected: AtomicBool,
        messages: AtomicU32,
    }

    impl TestClientHandler {
        fn new() -> Self {
            Self {
                connected: AtomicBool::new(false),
                messages: AtomicU32::new(0),
            }
        }
    }

    impl DispatchClientHandler for TestClientHandler {
        fn on_connect(&self, _client_id: u32, _channel_name: &str) {
            self.connected.store(true, Ordering::Relaxed);
        }
        fn on_disconnect(&self) {
            self.connected.store(false, Ordering::Relaxed);
        }
        fn on_message(&self, _data: &[u8]) {
            self.messages.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Регрессия (аудит 2026-07-10, тот же класс, что и в multi/): stop()
    /// обязан синхронно дождаться выхода lobby worker-потока, иначе после
    /// возврата из `stop()` callbacks могут продолжать дёргаться на уже
    /// освобождённом вызывающим состоянии.
    #[test]
    fn stop_synchronously_joins_worker() {
        let handler = Arc::new(TestServerHandler::new());
        let name = format!("TEST_DISPATCH_STOP_JOIN_{}", std::process::id());
        let server =
            DispatchServer::start(&name, handler, DispatchOptions::default()).expect("start");

        server.stop();

        assert!(
            server.worker_handle.lock().unwrap().is_none(),
            "stop() должен забрать и заджойнить worker_handle синхронно"
        );

        // Повторный stop() — идемпотентен, не паникует и не виснет.
        server.stop();
    }

    #[test]
    fn dispatch_server_start_stop() {
        let handler = Arc::new(TestServerHandler::new());
        let name = format!("TEST_DISPATCH_{}", std::process::id());
        let server = DispatchServer::start(&name, handler, DispatchOptions::default());
        assert!(server.is_ok());
        let server = server.unwrap();
        assert_eq!(server.client_count(), 0);
        server.stop();
    }

    #[test]
    fn dispatch_roundtrip() {
        let name = format!("TEST_DISPATCH_RT_{}", std::process::id());

        let server_handler = Arc::new(TestServerHandler::new());
        let server =
            DispatchServer::start(&name, server_handler.clone(), DispatchOptions::default())
                .expect("server start");

        thread::sleep(Duration::from_millis(100));

        let client_handler = Arc::new(TestClientHandler::new());
        let registration = ClientRegistration {
            pid: 12345,
            revision: 1,
            name: "test.exe".into(),
        };

        let client = DispatchClient::connect(
            &name,
            registration,
            client_handler.clone(),
            DispatchClientOptions::default(),
        )
        .expect("client connect");

        // Ждём, пока сервер зарегистрирует клиента
        let start = std::time::Instant::now();
        while server_handler.connects.load(Ordering::Relaxed) == 0
            && start.elapsed() < Duration::from_secs(5)
        {
            thread::sleep(Duration::from_millis(50));
        }

        assert!(client_handler.connected.load(Ordering::Relaxed));
        assert_eq!(server_handler.connects.load(Ordering::Relaxed), 1);
        assert_eq!(server_handler.last_pid.load(Ordering::Relaxed), 12345);
        assert!(client.client_id() > 0);
        assert_eq!(server.client_count(), 1);

        // Отправляем сообщение от клиента серверу
        client.send(b"hello").expect("client send");
        thread::sleep(Duration::from_millis(200));
        assert!(server_handler.messages.load(Ordering::Relaxed) >= 1);

        // Отправляем сообщение от сервера клиенту
        let clients = server.connected_clients();
        assert_eq!(clients.len(), 1);
        server.send_to(clients[0], b"world").expect("server send");
        thread::sleep(Duration::from_millis(200));
        assert!(client_handler.messages.load(Ordering::Relaxed) >= 1);

        // Очистка
        client.stop();
        thread::sleep(Duration::from_millis(200));
        server.stop();
    }

    #[test]
    fn dispatch_disconnect_no_double_notify() {
        let name = format!("TEST_DISPATCH_DC_{}", std::process::id());

        let server_handler = Arc::new(TestServerHandler::new());
        let server =
            DispatchServer::start(&name, server_handler.clone(), DispatchOptions::default())
                .expect("server start");

        thread::sleep(Duration::from_millis(100));

        let client_handler = Arc::new(TestClientHandler::new());
        let registration = ClientRegistration {
            pid: 99999,
            revision: 1,
            name: "dc_test.exe".into(),
        };

        let client = DispatchClient::connect(
            &name,
            registration,
            client_handler,
            DispatchClientOptions::default(),
        )
        .expect("client connect");

        // Ждём подключения
        let start = std::time::Instant::now();
        while server.client_count() == 0 && start.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(server.client_count(), 1);

        // Отключение со стороны сервера
        let clients = server.connected_clients();
        server
            .disconnect_client(clients[0])
            .expect("disconnect_client");

        thread::sleep(Duration::from_millis(300));

        // Должно быть ровно 1 отключение (не двойное)
        assert_eq!(server_handler.disconnects.load(Ordering::Relaxed), 1);

        client.stop();
        server.stop();
    }

    #[test]
    fn dispatch_multiple_clients() {
        let name = format!("TEST_DISPATCH_MC_{}", std::process::id());

        let server_handler = Arc::new(TestServerHandler::new());
        let server =
            DispatchServer::start(&name, server_handler.clone(), DispatchOptions::default())
                .expect("server start");

        thread::sleep(Duration::from_millis(100));

        // Подключаем 3 клиентов последовательно
        let mut clients = Vec::new();
        for i in 0..3u32 {
            let handler = Arc::new(TestClientHandler::new());
            let reg = ClientRegistration {
                pid: 1000 + i,
                revision: 1,
                name: format!("client_{i}.exe"),
            };
            let client = DispatchClient::connect(
                &name,
                reg,
                handler.clone(),
                DispatchClientOptions::default(),
            )
            .expect("client connect");
            clients.push((client, handler));

            // Ждём, пока сервер зарегистрирует этого клиента
            let expected = i + 1;
            let start = std::time::Instant::now();
            while server.client_count() < expected && start.elapsed() < Duration::from_secs(5) {
                thread::sleep(Duration::from_millis(50));
            }
        }

        assert_eq!(server.client_count(), 3);
        assert_eq!(server_handler.connects.load(Ordering::Relaxed), 3);

        // Все клиенты могут отправлять сообщения
        for (client, _) in &clients {
            client.send(b"ping").expect("send");
        }
        thread::sleep(Duration::from_millis(300));
        assert!(server_handler.messages.load(Ordering::Relaxed) >= 3);

        // Рассылка
        let sent = server.broadcast(b"pong").expect("broadcast");
        assert_eq!(sent, 3);
        thread::sleep(Duration::from_millis(300));
        for (_, handler) in &clients {
            assert!(handler.messages.load(Ordering::Relaxed) >= 1);
        }

        // Очистка
        for (client, _) in &clients {
            client.stop();
        }
        thread::sleep(Duration::from_millis(200));
        server.stop();
    }

    /// Регрессия на self-join deadlock (аудит 2026-07-10): когда клиент
    /// отключается, `AutoProxyHandler::on_disconnect` (вызывается ИЗ
    /// собственного worker-потока AutoServer этого канала) раньше делал
    /// `clients.remove(&id)` без привязки результата к переменной -- временное
    /// значение `DispatchedClient` (содержащее `AutoServer`) дропалось тут же,
    /// ВСЁ ЕЩЁ под write-логом `clients`. `Drop for AutoServer` синхронно
    /// джойнит свой `worker_handle` -- а это и есть текущий поток, self-join
    /// навсегда, лог остаётся захваченным. `DispatchServer::stop()` (которая
    /// теперь синхронно джойнит lobby worker) потом виснет НАВСЕГДА на том же
    /// логе в shutdown-секции `worker_loop`. Раньше это было незаметно, т.к.
    /// stop() не джойнил и никто не ждал зависший поток.
    #[test]
    fn stop_does_not_deadlock_after_client_disconnects() {
        let name = format!("TEST_DISPATCH_NODEADLOCK_{}", std::process::id());

        let server_handler = Arc::new(TestServerHandler::new());
        let server = DispatchServer::start(&name, server_handler, DispatchOptions::default())
            .expect("server start");

        thread::sleep(Duration::from_millis(100));

        let client_handler = Arc::new(TestClientHandler::new());
        let registration = ClientRegistration {
            pid: 55555,
            revision: 1,
            name: "deadlock_test.exe".into(),
        };
        let client = DispatchClient::connect(
            &name,
            registration,
            client_handler,
            DispatchClientOptions::default(),
        )
        .expect("client connect");

        let start = std::time::Instant::now();
        while server.client_count() == 0 && start.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(server.client_count(), 1);

        // Клиент отключается сам (не через disconnect_client) -- именно этот
        // путь триггерит AutoProxyHandler::on_disconnect ИЗНУТРИ AutoServer'а.
        client.stop();
        thread::sleep(Duration::from_millis(300));

        // Если это виснет (таймаут теста) -- деадлок вернулся.
        server.stop();
    }

    /// Регрессия на concurrency-фикс лобби (аудит 2026-07-10): раньше один
    /// клиент, зарегистрировавшийся в лобби, но так и не подключившийся к
    /// выделенному каналу, блокировал ВСЕХ последующих на весь
    /// `channel_connect_timeout` (единственный worker-поток лобби был занят
    /// Condvar-ожиданием этого клиента). Теперь ожидание вынесено в отдельный
    /// поток, и лобби свободно для следующего клиента сразу после ответа.
    #[test]
    fn stalled_client_does_not_block_subsequent_registrations() {
        let name = format!("TEST_DISPATCH_CONCURRENT_{}", std::process::id());

        let server_handler = Arc::new(TestServerHandler::new());
        let server = DispatchServer::start(
            &name,
            server_handler,
            DispatchOptions {
                // Заметно длиннее, чем должна занять регистрация клиента B --
                // если бы лобби всё ещё было последовательным, тест бы либо
                // завис на это время, либо B зарегистрировался бы только
                // спустя ~5с.
                channel_connect_timeout: Duration::from_secs(5),
                ..Default::default()
            },
        )
        .expect("server start");

        thread::sleep(Duration::from_millis(100));

        // Клиент A: только lobby-регистрация, БЕЗ подключения к выделенному
        // каналу -- симулирует зависшего/медленного клиента.
        let mut buffer = Vec::with_capacity(MAX_MESSAGE_SIZE);
        let reg_a = ClientRegistration {
            pid: 111,
            revision: 1,
            name: "stalled.exe".into(),
        };
        let (_id_a, _channel_a) = lobby_register(
            &name,
            &reg_a,
            &DispatchClientOptions::default(),
            &mut buffer,
        )
        .expect("client A lobby_register");
        // Намеренно НЕ вызываем AutoClient::connect для client A -- канал
        // остаётся неподключённым до истечения channel_connect_timeout.

        // Клиент B: полноценное подключение сразу после A.
        let start = std::time::Instant::now();
        let client_handler_b = Arc::new(TestClientHandler::new());
        let reg_b = ClientRegistration {
            pid: 222,
            revision: 1,
            name: "prompt.exe".into(),
        };
        let client_b = DispatchClient::connect(
            &name,
            reg_b,
            client_handler_b,
            DispatchClientOptions::default(),
        )
        .expect("client B connect");
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(2),
            "клиент B зарегистрировался за {elapsed:?} -- лобби всё ещё \
             сериализуется через зависшего клиента A (channel_connect_timeout=5с)"
        );

        client_b.stop();
        server.stop();
    }

    /// Регрессия (аудит 2026-07-10): имена каналов не должны предсказуемо
    /// выводиться из известных клиенту входов (`client_id`/примерное время
    /// регистрации) -- иначе враждебный локальный процесс мог бы вычислить
    /// будущее имя заранее и захватить его первым (squatting). Прямая
    /// непредсказуемость не тестируется юнит-тестом (нужен внешний
    /// наблюдатель), но проверяем необходимое условие: практическая
    /// уникальность на большом числе вызовов и корректный формат.
    #[test]
    fn channel_names_are_practically_unique() {
        use std::collections::HashSet;

        let name = format!("TEST_DISPATCH_CHNAME_{}", std::process::id());
        let handler = Arc::new(TestServerHandler::new());
        let server = DispatchServer::start(&name, handler, DispatchOptions::default())
            .expect("server start");

        let names: HashSet<String> = (0..1000).map(|_| server.generate_channel_name()).collect();

        assert_eq!(
            names.len(),
            1000,
            "1000 сгенерированных имён каналов должны быть различны"
        );
        for n in &names {
            assert_eq!(n.len(), 16, "имя канала должно быть 16 hex-символов: {n}");
            assert!(
                n.chars().all(|c| c.is_ascii_hexdigit()),
                "имя канала должно состоять только из hex-символов: {n}"
            );
        }

        server.stop();
    }
    /// Регрессия (аудит 2026-07-28): после `disconnect_client()` со стороны
    /// сервера нижележащий `AutoClient` продолжал бесконечно переподключаться
    /// к уже снесённому каналу, а `DispatchClient::is_connected()` оставался
    /// `true` -- вопреки задокументированному "не переподключается
    /// автоматически".
    #[test]
    fn client_is_connected_false_after_server_disconnect() {
        let name = format!("TEST_DISPATCH_CFLAG_{}", std::process::id());

        let server_handler = Arc::new(TestServerHandler::new());
        let server = DispatchServer::start(&name, server_handler, DispatchOptions::default())
            .expect("server start");

        thread::sleep(Duration::from_millis(100));

        let client_handler = Arc::new(TestClientHandler::new());
        let client = DispatchClient::connect(
            &name,
            ClientRegistration {
                pid: std::process::id(),
                revision: 1,
                name: "cflag.exe".into(),
            },
            client_handler,
            DispatchClientOptions::default(),
        )
        .expect("client connect");

        let start = std::time::Instant::now();
        while server.client_count() == 0 && start.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(server.client_count(), 1);
        assert!(client.is_connected(), "канал должен быть поднят");

        let ids = server.connected_clients();
        server.disconnect_client(ids[0]).expect("disconnect_client");

        let start = std::time::Instant::now();
        while client.is_connected() && start.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !client.is_connected(),
            "после разрыва со стороны сервера is_connected() обязан стать false"
        );

        server.stop();
    }

    fn wait_for(what: &str, limit: Duration, mut cond: impl FnMut() -> bool) {
        let start = std::time::Instant::now();
        while !cond() {
            assert!(start.elapsed() < limit, "не дождались: {what}");
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// 0.9: имя канала наследует пространство имён лобби.
    #[test]
    fn channel_namespace_prefix_follows_lobby() {
        assert_eq!(channel_namespace_prefix("Global\\NxT"), "Global\\");
        assert_eq!(channel_namespace_prefix("Local\\NxT"), "Local\\");
        assert_eq!(channel_namespace_prefix("NxT"), "");
        assert_eq!(
            channel_namespace_prefix("\\Sessions\\1\\BaseNamedObjects\\NxT"),
            "\\Sessions\\1\\BaseNamedObjects\\"
        );
        assert_eq!(lobby_lock_name("NxT"), "Local\\NxT_lock");
        assert_eq!(lobby_lock_name("Global\\NxT"), "Global\\NxT_lock");
        // Каталог NT-пути длиннее 48 байт не оставил бы места под 16 hex.
        let long = format!("\\{}\\NxT", "D".repeat(60));
        match DispatchServer::start(
            &long,
            Arc::new(TestServerHandler::new()),
            DispatchOptions::default(),
        ) {
            Err(ShmError::InvalidConfig(_)) => {}
            other => panic!("ожидался InvalidConfig, получено {other:?}"),
        }
    }

    /// 0.9: лобби `Local\X` -> канал `Local\<16 hex>`, клиент подключается
    /// по имени из ответа.
    #[test]
    fn channel_inherits_lobby_namespace_end_to_end() {
        let name = format!("Local\\TEST_DISPATCH_NS_{}", std::process::id());
        let server = DispatchServer::start(
            &name,
            Arc::new(TestServerHandler::new()),
            DispatchOptions::default(),
        )
        .unwrap();
        let handler = Arc::new(TestClientHandler::new());
        let client = DispatchClient::connect(
            &name,
            ClientRegistration {
                pid: std::process::id(),
                revision: 1,
                name: "ns".into(),
            },
            handler.clone(),
            DispatchClientOptions::default(),
        )
        .unwrap();
        let channel = client.channel_name().to_owned();
        assert!(
            channel.starts_with("Local\\"),
            "канал вне namespace лобби: {channel}"
        );
        assert_eq!(channel.len(), "Local\\".len() + 16);
        wait_for("канал поднят", Duration::from_secs(5), || {
            handler.connected.load(Ordering::Relaxed)
        });
        assert_eq!(server.channel_name(client.client_id()), Some(channel));
        client.stop();
        server.stop();
    }

    /// Сервер, запоминающий регистрацию и первое сообщение каждого клиента.
    #[derive(Default)]
    struct RoutingHandler {
        names: Mutex<HashMap<u32, String>>,
        first_message: Mutex<HashMap<u32, Vec<u8>>>,
    }

    impl DispatchHandler for RoutingHandler {
        fn on_client_connect(&self, client_id: u32, info: &ClientRegistration) {
            self.names
                .lock()
                .unwrap()
                .insert(client_id, info.name.clone());
        }
        fn on_client_disconnect(&self, _client_id: u32) {}
        fn on_message(&self, client_id: u32, data: &[u8]) {
            self.first_message
                .lock()
                .unwrap()
                .entry(client_id)
                .or_insert_with(|| data.to_vec());
        }
    }

    /// 0.9: 16 клиентов регистрируются одновременно -- мьютекс лобби
    /// пропускает их по одному: все подключаются, id и каналы различны,
    /// ответы не перепутаны (сообщение клиента приходит с его же id, и его
    /// имя совпадает с зарегистрированным).
    #[test]
    fn concurrent_registrations_are_serialized() {
        const N: usize = 16;
        let name = format!("TEST_DISPATCH_16_{}", std::process::id());
        let handler = Arc::new(RoutingHandler::default());
        let server =
            DispatchServer::start(&name, handler.clone(), DispatchOptions::default()).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(N));
        let threads: Vec<_> = (0..N)
            .map(|i| {
                let (name, barrier) = (name.clone(), barrier.clone());
                thread::spawn(move || {
                    barrier.wait();
                    let me = format!("client-{i}");
                    let client = DispatchClient::connect(
                        &name,
                        ClientRegistration {
                            pid: std::process::id(),
                            revision: 1,
                            name: me.clone(),
                        },
                        Arc::new(TestClientHandler::new()),
                        DispatchClientOptions::default(),
                    )
                    .unwrap_or_else(|e| panic!("{me}: регистрация не удалась: {e:?}"));
                    (me, client)
                })
            })
            .collect();
        let clients: Vec<(String, DispatchClient)> =
            threads.into_iter().map(|t| t.join().unwrap()).collect();

        let mut ids: Vec<u32> = clients.iter().map(|(_, c)| c.client_id()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), N, "id клиентов повторяются");
        let mut channels: Vec<&str> = clients.iter().map(|(_, c)| c.channel_name()).collect();
        channels.sort_unstable();
        channels.dedup();
        assert_eq!(channels.len(), N, "каналы повторяются");

        wait_for(
            "все каналы подняты",
            Duration::from_secs(10),
            || server.client_count() as usize == N,
        );
        for (me, client) in &clients {
            client.send(me.as_bytes()).unwrap();
        }
        wait_for(
            "сообщения от всех",
            Duration::from_secs(10),
            || handler.first_message.lock().unwrap().len() == N,
        );
        let names = handler.names.lock().unwrap();
        let messages = handler.first_message.lock().unwrap();
        for (me, client) in &clients {
            let id = client.client_id();
            assert_eq!(names.get(&id), Some(me), "ответ регистрации перепутан");
            assert_eq!(messages.get(&id).map(Vec::as_slice), Some(me.as_bytes()));
            assert_eq!(
                server.channel_name(id).as_deref(),
                Some(client.channel_name())
            );
        }
        drop(names);
        drop(messages);
        drop(clients);
        server.stop();
    }

    /// 0.9: подключённые клиент и сервер Dispatch (лобби + канал с обеих
    /// сторон) в простое не просыпаются ни разу.
    #[test]
    fn idle_dispatch_does_not_wake_up() {
        let name = format!("TEST_DISPATCH_IDLE_{}", std::process::id());
        let server_h = Arc::new(TestServerHandler::new());
        let server =
            DispatchServer::start(&name, server_h.clone(), DispatchOptions::default()).unwrap();
        let client_h = Arc::new(TestClientHandler::new());
        let client = DispatchClient::connect(
            &name,
            ClientRegistration {
                pid: std::process::id(),
                revision: 1,
                name: "idle".into(),
            },
            client_h.clone(),
            DispatchClientOptions::default(),
        )
        .unwrap();
        wait_for("канал поднят", Duration::from_secs(5), || {
            client_h.connected.load(Ordering::Relaxed) && server.client_count() == 1
        });
        client.send(b"ping").unwrap();
        wait_for("доставка", Duration::from_secs(2), || {
            server_h.messages.load(Ordering::Relaxed) == 1
        });
        thread::sleep(Duration::from_millis(200));

        let (s0, c0) = (server.wakeups(), client.wakeups());
        thread::sleep(Duration::from_secs(1));
        assert_eq!(
            server.wakeups() - s0,
            0,
            "лобби/канал сервера просыпались в простое"
        );
        assert_eq!(
            client.wakeups() - c0,
            0,
            "канал клиента просыпался в простое"
        );
        client.stop();
        server.stop();
    }

    /// Регрессия (аудит prof-shm 2026-09-25): лобби создавалось в фоне, и
    /// второй сервер с тем же именем получал `Ok` и молча повторял попытки.
    /// Теперь занятое имя -- ошибка `start`.
    #[test]
    fn second_server_on_same_lobby_fails_synchronously() {
        let name = format!("TEST_DISPATCH_BUSY_{}", std::process::id());
        let first = DispatchServer::start(
            &name,
            Arc::new(TestServerHandler::new()),
            DispatchOptions::default(),
        )
        .expect("первый сервер");
        let second = DispatchServer::start(
            &name,
            Arc::new(TestServerHandler::new()),
            DispatchOptions::default(),
        );
        assert!(second.is_err(), "имя лобби занято -- ошибка сразу");
        assert!(DispatchClient::lobby_exists(&name));
        first.stop();
        drop(first);
        assert!(
            !DispatchClient::lobby_exists(&name),
            "после остановки лобби нет"
        );
        // Имя освободилось -- снова можно.
        let again = DispatchServer::start(
            &name,
            Arc::new(TestServerHandler::new()),
            DispatchOptions::default(),
        )
        .expect("имя освободилось");
        again.stop();
    }

    /// Обработчик, проверяющий порядок колбэков: сообщение клиента, о
    /// котором ещё не было `on_client_connect`, -- ошибка.
    #[derive(Default)]
    struct OrderCheck {
        known: Mutex<std::collections::HashSet<u32>>,
        orphans: AtomicU32,
        messages: AtomicU32,
    }

    impl DispatchHandler for OrderCheck {
        fn on_client_connect(&self, client_id: u32, _info: &ClientRegistration) {
            // Медленный обработчик подключения: при старом порядке (объявление
            // из отдельного потока) worker канала успевал доставить первые
            // сообщения раньше.
            thread::sleep(Duration::from_millis(20));
            self.known.lock().unwrap().insert(client_id);
        }
        fn on_client_disconnect(&self, _client_id: u32) {}
        fn on_message(&self, client_id: u32, _data: &[u8]) {
            if !self.known.lock().unwrap().contains(&client_id) {
                self.orphans.fetch_add(1, Ordering::AcqRel);
            }
            self.messages.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Клиент, который пишет сразу в `on_connect` (канал поднят).
    struct EagerClient {
        slot: Mutex<Option<Arc<DispatchClient>>>,
        up: AtomicBool,
    }

    impl DispatchClientHandler for EagerClient {
        fn on_connect(&self, _client_id: u32, _channel_name: &str) {
            self.up.store(true, Ordering::Release);
        }
        fn on_disconnect(&self) {}
        fn on_message(&self, _data: &[u8]) {}
    }

    /// Регрессия (аудит prof-shm 2026-09-25): `on_message` мог прийти ДО
    /// `on_client_connect` -- первые сообщения клиента (у prof-shm это
    /// заголовок потока и определения) терялись. Теперь клиент объявляется
    /// в worker-е канала до его первого сообщения.
    #[test]
    fn messages_never_precede_client_connect() {
        let name = format!("TEST_DISPATCH_ORDER_{}", std::process::id());
        let handler = Arc::new(OrderCheck::default());
        let server = DispatchServer::start(&name, handler.clone(), DispatchOptions::default())
            .expect("server start");
        const CLIENTS: u32 = 8;
        const BURST: u32 = 20;
        for i in 0..CLIENTS {
            let client_handler = Arc::new(EagerClient {
                slot: Mutex::new(None),
                up: AtomicBool::new(false),
            });
            let client = Arc::new(
                DispatchClient::connect(
                    &name,
                    ClientRegistration {
                        pid: std::process::id(),
                        revision: 1,
                        name: format!("eager_{i}"),
                    },
                    client_handler.clone(),
                    DispatchClientOptions::default(),
                )
                .expect("client connect"),
            );
            *client_handler.slot.lock().unwrap() = Some(Arc::clone(&client));
            wait_for("канал клиента", Duration::from_secs(5), || {
                client_handler.up.load(Ordering::Acquire)
            });
            // Сразу после подъёма канала -- пачка сообщений.
            for _ in 0..BURST {
                client.try_send(b"early").expect("try_send");
            }
            wait_for(
                "сообщения клиента",
                Duration::from_secs(5),
                || handler.messages.load(Ordering::Acquire) >= (i + 1) * BURST,
            );
            client.stop();
            client_handler.slot.lock().unwrap().take();
        }
        assert_eq!(
            handler.orphans.load(Ordering::Acquire),
            0,
            "сообщение до on_client_connect"
        );
        assert_eq!(
            handler.messages.load(Ordering::Acquire),
            CLIENTS * BURST,
            "потери"
        );
        server.stop();
    }

    /// Остановка будит потоки сервера событием: `stop()` не ждёт ни
    /// `channel_connect_timeout` зависшего клиента, ни таймаута опроса.
    #[test]
    fn stop_is_prompt_with_stalled_registration() {
        let name = format!("TEST_DISPATCH_PROMPT_{}", std::process::id());
        let server = DispatchServer::start(
            &name,
            Arc::new(TestServerHandler::new()),
            DispatchOptions {
                channel_connect_timeout: Duration::from_secs(30),
                ..DispatchOptions::default()
            },
        )
        .expect("server start");
        let mut buffer = Vec::with_capacity(MAX_MESSAGE_SIZE);
        // Регистрация без подключения к каналу -- поток ожидания висит на
        // `channel_connect_timeout`.
        let _ = lobby_register(
            &name,
            &ClientRegistration {
                pid: 1,
                revision: 1,
                name: "stalled".into(),
            },
            &DispatchClientOptions::default(),
            &mut buffer,
        )
        .expect("lobby_register");
        let started = std::time::Instant::now();
        server.stop();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "stop() ждал {:?}",
            started.elapsed()
        );
    }

    /// Клиент, копирующий сообщения и отмечающий момент отключения.
    #[derive(Default)]
    struct FarewellClient {
        log: Mutex<Vec<String>>,
    }

    impl DispatchClientHandler for FarewellClient {
        fn on_connect(&self, _client_id: u32, _channel_name: &str) {}
        fn on_disconnect(&self) {
            self.log.lock().unwrap().push("disconnect".into());
        }
        fn on_message(&self, data: &[u8]) {
            self.log
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(data).into_owned());
        }
    }

    /// Прощальное сообщение сервера (`try_send_to` + `disconnect_client`)
    /// доходит до клиента ДО его `on_disconnect`: worker канала дописывает
    /// очередь при остановке, клиент дочитывает кольцо при отключении.
    #[test]
    fn farewell_message_is_delivered_before_disconnect() {
        let name = format!("TEST_DISPATCH_BYE_{}", std::process::id());
        let server = DispatchServer::start(
            &name,
            Arc::new(TestServerHandler::new()),
            DispatchOptions::default(),
        )
        .expect("server start");
        for round in 0..5 {
            let client_handler = Arc::new(FarewellClient::default());
            let client = DispatchClient::connect(
                &name,
                ClientRegistration {
                    pid: std::process::id(),
                    revision: 1,
                    name: "farewell".into(),
                },
                client_handler.clone(),
                DispatchClientOptions::default(),
            )
            .expect("client connect");
            wait_for("регистрация", Duration::from_secs(5), || {
                server.is_client_connected(client.client_id())
            });
            server
                .try_send_to(client.client_id(), b"bye: closed")
                .expect("try_send_to");
            server
                .disconnect_client(client.client_id())
                .expect("disconnect_client");
            wait_for(
                "отключение клиента",
                Duration::from_secs(5),
                || {
                    client_handler
                        .log
                        .lock()
                        .unwrap()
                        .last()
                        .map(String::as_str)
                        == Some("disconnect")
                },
            );
            let log = client_handler.log.lock().unwrap().clone();
            assert_eq!(log, ["bye: closed", "disconnect"], "раунд {round}");
            client.stop();
        }
        server.stop();
    }

    /// Отправитель без потерь ждёт места по событию `on_space_available`
    /// (без сна-циклов) и доставляет всё по порядку.
    #[test]
    fn space_event_drives_lossless_sender() {
        struct Slow {
            got: AtomicU32,
            bad: AtomicBool,
        }
        impl DispatchHandler for Slow {
            fn on_client_connect(&self, _id: u32, _info: &ClientRegistration) {}
            fn on_client_disconnect(&self, _id: u32) {}
            fn on_message(&self, _id: u32, data: &[u8]) {
                let expected = self.got.load(Ordering::Acquire);
                if u32::from_le_bytes(data[..4].try_into().unwrap()) != expected {
                    self.bad.store(true, Ordering::Release);
                }
                self.got.fetch_add(1, Ordering::AcqRel);
                if expected.is_multiple_of(8) {
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }
        #[derive(Default)]
        struct Waker {
            epoch: Mutex<u64>,
            cv: std::sync::Condvar,
        }
        impl DispatchClientHandler for Waker {
            fn on_connect(&self, _id: u32, _channel: &str) {}
            fn on_disconnect(&self) {}
            fn on_message(&self, _data: &[u8]) {}
            fn on_space_available(&self) {
                *self.epoch.lock().unwrap() += 1;
                self.cv.notify_all();
            }
        }

        const TOTAL: u32 = 3_000;
        let name = format!("TEST_DISPATCH_SPACE_{}", std::process::id());
        let slow = Arc::new(Slow {
            got: AtomicU32::new(0),
            bad: AtomicBool::new(false),
        });
        let server = DispatchServer::start(&name, slow.clone(), DispatchOptions::default())
            .expect("server start");
        let waker = Arc::new(Waker::default());
        let client = DispatchClient::connect(
            &name,
            ClientRegistration {
                pid: std::process::id(),
                revision: 1,
                name: "space".into(),
            },
            waker.clone(),
            DispatchClientOptions {
                max_send_queue: 8,
                ..DispatchClientOptions::default()
            },
        )
        .expect("client connect");
        wait_for("регистрация", Duration::from_secs(5), || {
            server.client_count() == 1
        });
        let mut fulls = 0u32;
        let payload_len = 30_000;
        for seq in 0..TOTAL {
            let mut payload = vec![0u8; payload_len];
            payload[..4].copy_from_slice(&seq.to_le_bytes());
            loop {
                let seen = *waker.epoch.lock().unwrap();
                match client.try_send(&payload) {
                    Ok(()) => break,
                    Err(ShmError::QueueFull) => {
                        fulls += 1;
                        let guard = waker.epoch.lock().unwrap();
                        let (guard, timeout) = waker
                            .cv
                            .wait_timeout_while(guard, Duration::from_secs(10), |e| *e == seen)
                            .unwrap();
                        drop(guard);
                        assert!(!timeout.timed_out(), "событие места не пришло за 10 с");
                    }
                    Err(err) => panic!("try_send: {err:?}"),
                }
            }
        }
        wait_for("доставка", Duration::from_secs(30), || {
            slow.got.load(Ordering::Acquire) >= TOTAL
        });
        assert_eq!(slow.got.load(Ordering::Acquire), TOTAL);
        assert!(!slow.bad.load(Ordering::Acquire), "порядок нарушен");
        assert!(
            fulls > 0,
            "очередь ни разу не заполнилась -- тест ничего не проверил"
        );
        client.stop();
        server.stop();
    }

    /// Сервер, отказывающий каждому клиенту прямо в `on_client_connect`:
    /// прощальное сообщение и `disconnect_client` (как `refuse` prof-shm).
    #[derive(Default)]
    struct Refuser {
        server: std::sync::OnceLock<std::sync::Weak<DispatchServer>>,
        refused: AtomicU32,
    }

    impl DispatchHandler for Refuser {
        fn on_client_connect(&self, client_id: u32, _info: &ClientRegistration) {
            let server = self
                .server
                .get()
                .and_then(std::sync::Weak::upgrade)
                .unwrap();
            server.try_send_to(client_id, b"bye: refused").unwrap();
            server.disconnect_client(client_id).unwrap();
            self.refused.fetch_add(1, Ordering::AcqRel);
        }
        fn on_client_disconnect(&self, _client_id: u32) {}
        fn on_message(&self, _client_id: u32, _data: &[u8]) {}
    }

    /// Регрессия (ревизия 2, дефект 1): сервер отключает клиента сразу после
    /// рукопожатия канала (из `on_client_connect`). Раньше клиент видел
    /// `server_state == IDLE` после `S2C_CONNECT`, откатывался с
    /// `HandshakeFailed` и переподключался к исчезнувшему каналу вечно:
    /// прощание терялось, `on_disconnect` не приходил, `is_connected()`
    /// оставался `true`. Теперь рукопожатие засчитывается по смене
    /// `generation`, и клиент дочитывает кольцо до `DISCONNECT`.
    #[test]
    fn refusal_in_on_client_connect_delivers_farewell_and_disconnect() {
        let name = format!("TEST_DISPATCH_REFUSE_{}", std::process::id());
        let handler = Arc::new(Refuser::default());
        let server =
            DispatchServer::start(&name, handler.clone(), DispatchOptions::default()).unwrap();
        handler.server.set(Arc::downgrade(&server)).unwrap();
        for round in 0..10 {
            let client_handler = Arc::new(FarewellClient::default());
            let client = DispatchClient::connect(
                &name,
                ClientRegistration {
                    pid: std::process::id(),
                    revision: 1,
                    name: "refused".into(),
                },
                client_handler.clone(),
                DispatchClientOptions::default(),
            )
            .expect("регистрация в лобби");
            wait_for(
                "отключение клиента",
                Duration::from_secs(5),
                || {
                    client_handler
                        .log
                        .lock()
                        .unwrap()
                        .last()
                        .map(String::as_str)
                        == Some("disconnect")
                },
            );
            let log = client_handler.log.lock().unwrap().clone();
            assert_eq!(log, ["bye: refused", "disconnect"], "раунд {round}");
            wait_for("is_connected() == false", Duration::from_secs(2), || {
                !client.is_connected()
            });
            assert_eq!(client.disconnect_reason(), Some(DisconnectReason::Graceful));
            assert_eq!(
                client.server_pid(),
                Some(std::process::id()),
                "PID сервера переживает разрыв"
            );
            client.stop();
        }
        assert_eq!(handler.refused.load(Ordering::Acquire), 10);
        server.stop();
    }

    /// Регрессия (ревизия 2, дефект 1): выделенный канал не поднялся --
    /// клиент завершается (`on_error` + `on_disconnect_reason(Error)`), а не
    /// повторяет подключение раз в 250 мс вечно.
    #[test]
    fn dedicated_channel_failure_ends_the_client() {
        #[derive(Default)]
        struct Probe {
            errors: AtomicU32,
            reasons: Mutex<Vec<DisconnectReason>>,
        }
        impl AutoHandler for Probe {
            fn on_disconnect_reason(&self, reason: DisconnectReason) {
                self.reasons.lock().unwrap().push(reason);
            }
            fn on_error(&self, _err: ShmError) {
                self.errors.fetch_add(1, Ordering::AcqRel);
            }
        }
        let probe = Arc::new(Probe::default());
        let pid = Arc::new(AtomicU32::new(0));
        let client = AutoClient::connect_dedicated(
            &format!("TEST_DISPATCH_NOCHANNEL_{}", std::process::id()),
            probe.clone(),
            AutoOptions {
                reconnect_delay: Duration::from_millis(20),
                ..AutoOptions::default()
            },
            pid,
        )
        .unwrap();
        wait_for("разрыв", Duration::from_secs(2), || {
            !probe.reasons.lock().unwrap().is_empty()
        });
        // Несколько `reconnect_delay`: повторов быть не должно.
        thread::sleep(Duration::from_millis(200));
        assert_eq!(probe.errors.load(Ordering::Acquire), 1, "повторные попытки");
        assert_eq!(*probe.reasons.lock().unwrap(), [DisconnectReason::Error]);
        drop(client);
    }

    /// Ревизия 2, дефект 6: `stop()` из колбэка обработчика не блокируется
    /// (раньше: поток лобби join-ил worker канала, ждущий в `stop()`, --
    /// взаимная блокировка). Остановка отложенная; `stop()` из чужого потока
    /// потом дожидается её полностью.
    #[test]
    fn stop_from_handler_callback_does_not_deadlock() {
        #[derive(Default)]
        struct Stopper {
            server: std::sync::OnceLock<std::sync::Weak<DispatchServer>>,
            returned: AtomicBool,
        }
        impl DispatchHandler for Stopper {
            fn on_client_connect(&self, _id: u32, _info: &ClientRegistration) {}
            fn on_client_disconnect(&self, _id: u32) {}
            fn on_message(&self, _id: u32, _data: &[u8]) {
                if let Some(server) = self.server.get().and_then(std::sync::Weak::upgrade) {
                    server.stop();
                    self.returned.store(true, Ordering::Release);
                }
            }
        }
        let name = format!("TEST_DISPATCH_STOPCB_{}", std::process::id());
        let handler = Arc::new(Stopper::default());
        let server =
            DispatchServer::start(&name, handler.clone(), DispatchOptions::default()).unwrap();
        handler.server.set(Arc::downgrade(&server)).unwrap();
        let client_handler = Arc::new(FarewellClient::default());
        let client = DispatchClient::connect(
            &name,
            ClientRegistration {
                pid: std::process::id(),
                revision: 1,
                name: "stopper".into(),
            },
            client_handler.clone(),
            DispatchClientOptions::default(),
        )
        .unwrap();
        wait_for("регистрация", Duration::from_secs(5), || {
            server.client_count() == 1
        });
        client.send(b"stop please").unwrap();
        wait_for(
            "stop() из колбэка вернулся",
            Duration::from_secs(5),
            || handler.returned.load(Ordering::Acquire),
        );
        wait_for(
            "клиент отключён",
            Duration::from_secs(5),
            || {
                client_handler
                    .log
                    .lock()
                    .unwrap()
                    .last()
                    .map(String::as_str)
                    == Some("disconnect")
            },
        );
        let (tx, rx) = std::sync::mpsc::channel();
        let joiner = {
            let server = Arc::clone(&server);
            thread::spawn(move || {
                server.stop();
                let _ = tx.send(());
            })
        };
        rx.recv_timeout(Duration::from_secs(5))
            .expect("stop() из чужого потока завис");
        joiner.join().unwrap();
        assert!(server.worker_handle.lock().unwrap().is_none());
        client.stop();
    }
}
