//! Dispatch-сервер — единое лобби с динамическими каналами на каждого клиента.
//!
//! # Архитектура
//!
//! ```text
//! DispatchServer("NxT")        ← единственное лобби, принимает всех клиентов
//!     ↓
//! Клиент подключается к лобби → отправляет RegistrationRequest {pid, revision, name}
//!     ↓
//! Сервер создаёт AutoServer("NxT_a7f3b2c1") → отправляет RegistrationResponse
//!     ↓
//! Клиент отключается от лобби → подключается к "NxT_a7f3b2c1" через AutoClient
//!     ↓
//! Обмен 1:1 на выделенном канале
//! ```

pub mod ffi;
pub mod protocol;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::auto::{AutoClient, AutoHandler, AutoOptions, AutoServer, ChannelKind};
use crate::client::SharedClient;
use crate::constants::MAX_MESSAGE_SIZE;
use crate::error::{Result, ShmError};
use crate::server::SharedServer;
use crate::wait_delay;

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

    /// Вызывается при получении сообщения от клиента по выделенному каналу.
    fn on_message(&self, client_id: u32, data: &[u8]);

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

    /// Вызывается при получении сообщения от сервера.
    fn on_message(&self, data: &[u8]);

    /// Вызывается при ошибке.
    fn on_error(&self, err: ShmError) {
        let _ = err;
    }
}

/// Настройки DispatchServer.
#[derive(Clone)]
pub struct DispatchOptions {
    /// Таймаут чтения данных регистрации из лобби после handshake.
    pub lobby_timeout: Duration,
    /// Таймаут подключения клиента к выделенному каналу после регистрации.
    pub channel_connect_timeout: Duration,
    /// Интервал опроса в worker loop.
    pub poll_timeout: Duration,
    /// Количество сообщений за один цикл на каждом клиентском канале.
    pub recv_batch: usize,
}

impl Default for DispatchOptions {
    fn default() -> Self {
        Self {
            lobby_timeout: Duration::from_secs(5),
            channel_connect_timeout: Duration::from_secs(30),
            poll_timeout: Duration::from_millis(50),
            recv_batch: 32,
        }
    }
}

/// Настройки DispatchClient.
#[derive(Clone)]
pub struct DispatchClientOptions {
    /// Таймаут подключения к лобби.
    pub lobby_timeout: Duration,
    /// Таймаут чтения ответа регистрации из лобби.
    pub response_timeout: Duration,
    /// Таймаут подключения к выделенному каналу.
    pub channel_timeout: Duration,
    /// Интервал опроса в worker loop на выделенном канале.
    pub poll_timeout: Duration,
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
            poll_timeout: Duration::from_millis(50),
            recv_batch: 32,
            max_send_queue: 256,
        }
    }
}

// ─── DispatchServer ──────────────────────────────────────────────────────────

/// Активный клиент на выделенном канале.
struct DispatchedClient {
    server: AutoServer,
    info: ClientRegistration,
    channel_name: String,
    /// Устанавливается в true, когда отключение уже обработано (предотвращает двойное уведомление).
    disconnected: AtomicBool,
}

/// Общая карта клиентов, доступная и серверу, и proxy-обработчикам.
type ClientMap = Arc<RwLock<HashMap<u32, DispatchedClient>>>;

/// Центральный dispatch-сервер — одно лобби, динамические каналы на клиента.
pub struct DispatchServer {
    base_name: String,
    clients: ClientMap,
    running: Arc<AtomicBool>,
    next_client_id: Arc<AtomicU32>,
    worker_handle: Mutex<Option<JoinHandle<()>>>,
    /// Потоки, ожидающие подключения клиента к выделенному каналу (см.
    /// `handle_lobby_client`) -- обязаны быть заджойнены в `stop()` ДО
    /// возврата, иначе `on_client_connect` мог бы выстрелить уже после того,
    /// как C-вызывающий код счёл сервер остановленным и освободил user_data.
    pending_connects: Mutex<Vec<JoinHandle<()>>>,
    handler: Arc<dyn DispatchHandler>,
    options: DispatchOptions,
}

impl DispatchServer {
    /// Запускает dispatch-сервер с лобби на заданном базовом имени.
    pub fn start(
        name: &str,
        handler: Arc<dyn DispatchHandler>,
        options: DispatchOptions,
    ) -> Result<Arc<Self>> {
        let running = Arc::new(AtomicBool::new(true));

        let server = Arc::new(Self {
            base_name: name.to_owned(),
            clients: Arc::new(RwLock::new(HashMap::new())),
            running,
            next_client_id: Arc::new(AtomicU32::new(1)),
            worker_handle: Mutex::new(None),
            pending_connects: Mutex::new(Vec::new()),
            handler,
            options,
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
        let handle = builder
            .spawn(move || server_clone.worker_loop(&name_owned))
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
    pub fn disconnect_client(&self, client_id: u32) -> Result<()> {
        let removed = self.clients.write().unwrap().remove(&client_id);
        if let Some(client) = removed {
            // Помечаем как отключённого, чтобы AutoProxyHandler не уведомил повторно
            client.disconnected.store(true, Ordering::Release);
            client.server.stop();
            self.handler.on_client_disconnect(client_id);
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
    /// больше не будет вызван. Критично для FFI: C-вызывающий код может
    /// освободить `user_data` сразу после возврата. Идемпотентна (повторный
    /// вызов — no-op, обе очереди handle-ов уже опустошены).
    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
        // Джойнится потоком-владельцем Self (не самим worker'ом — stop()
        // не вызывается изнутри worker_loop), поэтому не self-join. Тот же
        // фикс, что и для MultiServer::stop() (аудит 2026-07-10): worker
        // держит собственный клон Arc<Self>, поэтому расчёт только на Drop
        // гонял бы точно так же.
        if let Some(handle) = self.worker_handle.lock().unwrap().take() {
            let _ = handle.join();
        }
        // Lobby worker уже остановлен -> новых pending-connect потоков не
        // появится, можно безопасно забрать и заджойнить все существующие.
        // Каждый из них проверяет `running` и завершится быстро (не будет
        // ждать полный channel_connect_timeout), т.к. running уже false.
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
        format!("{:016x}", hasher.finish())
    }

    /// Главный worker loop — принимает клиентов через лобби.
    ///
    /// Сам lobby-handshake (single-client протокол) неизбежно последователен,
    /// но обрабатывается быстро (только чтение регистрации + ответ). Ожидание
    /// подключения клиента к его выделенному каналу (до `channel_connect_timeout`,
    /// по умолчанию 30с) вынесено в отдельный поток (см. `handle_lobby_client`),
    /// поэтому один медленный клиент больше не блокирует регистрацию остальных.
    fn worker_loop(&self, base_name: &str) {
        let mut buffer = Vec::with_capacity(MAX_MESSAGE_SIZE);

        while self.running.load(Ordering::Acquire) {
            // Создаём лобби (или пересоздаём при ошибке)
            let mut lobby_server = match SharedServer::start(base_name) {
                Ok(s) => s,
                Err(err) => {
                    self.handler.on_error(None, err);
                    if !wait_delay(&self.running, self.options.poll_timeout) {
                        break;
                    }
                    continue;
                }
            };

            // Внутренний цикл: последовательный приём клиентов через лобби
            while self.running.load(Ordering::Acquire) {
                match lobby_server.wait_for_client(Some(self.options.poll_timeout)) {
                    Ok(()) => {
                        // Клиент подключился — обрабатываем регистрацию
                        self.handle_lobby_client(&mut lobby_server, &mut buffer);

                        // Сбрасываем лобби для следующего клиента
                        lobby_server.mark_disconnected();
                    }
                    Err(ShmError::Timeout) => continue,
                    Err(ShmError::AlreadyConnected) => {
                        lobby_server.mark_disconnected();
                    }
                    Err(err) => {
                        self.handler.on_error(None, err);
                        break; // Пересоздаём лобби при серьёзной ошибке
                    }
                }
            }
        }

        // Остановка: закрываем все клиентские каналы
        let mut clients = self.clients.write().unwrap();
        for (id, client) in clients.drain() {
            client.disconnected.store(true, Ordering::Release);
            client.server.stop();
            self.handler.on_client_disconnect(id);
        }
    }

    /// Обрабатывает одного клиента в лобби: читает регистрацию, создаёт канал, отвечает.
    ///
    /// Возвращается СРАЗУ после отправки ответа клиенту (не дожидаясь его
    /// подключения к выделенному каналу) — вызывающий код (`worker_loop`)
    /// тут же сбрасывает лобби и готов принимать следующего клиента.
    /// Ожидание фактического подключения к каналу (до `channel_connect_timeout`)
    /// и регистрация в `self.clients` вынесены в отдельный поток, чтобы не
    /// сериализовать всех клиентов через самый медленный из них.
    fn handle_lobby_client(&self, lobby: &mut SharedServer, buffer: &mut Vec<u8>) {
        let events = match lobby.events() {
            Some(e) => e,
            None => {
                self.handler.on_error(None, ShmError::NotReady);
                return;
            }
        };

        // Ожидаем данные регистрации через событие c2s.data (по событию, без опроса)
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
                    // Блокируемся на событии — просыпаемся, когда клиент запишет данные
                    let wait_time = remaining.min(self.options.poll_timeout);
                    let _ = events.c2s.data.wait(Some(wait_time));
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
            name: request.name.clone(),
        };

        // Создаём AutoServer для выделенного канала этого клиента
        let connect_signal = Arc::new((Mutex::new(false), Condvar::new()));

        let proxy_handler = Arc::new(AutoProxyHandler {
            client_id,
            handler: self.handler.clone(),
            clients: Arc::clone(&self.clients),
            connect_signal: connect_signal.clone(),
        });

        let auto_options = AutoOptions {
            connect_timeout: self.options.channel_connect_timeout,
            poll_timeout: self.options.poll_timeout,
            recv_batch: self.options.recv_batch,
            ..AutoOptions::default()
        };

        let auto_server = match AutoServer::start(&channel_name, proxy_handler, auto_options) {
            Ok(s) => s,
            Err(err) => {
                self.handler.on_error(None, err);
                let reject = protocol::encode_response(&RegistrationResponse {
                    status: protocol::STATUS_REJECTED,
                    client_id: 0,
                    channel_name: String::new(),
                });
                let _ = lobby.send_to_client(&reject);
                return;
            }
        };

        // Отправляем клиенту назначенный канал через лобби
        let response = protocol::encode_response(&RegistrationResponse {
            status: protocol::STATUS_OK,
            client_id,
            channel_name: channel_name.clone(),
        });

        if let Err(err) = lobby.send_to_client(&response) {
            self.handler.on_error(None, err);
            auto_server.stop();
            return;
        }

        // Сигналим о доступности данных в лобби, чтобы клиент мог их прочитать
        if let Some(events) = lobby.events() {
            let _ = events.s2c.data.set();
        }

        // Ожидание подключения к выделенному каналу (до channel_connect_timeout,
        // по умолчанию 30с) и регистрация в self.clients — в ОТДЕЛЬНОМ потоке,
        // а не на потоке лобби: иначе один медленный/зависший клиент блокировал
        // бы регистрацию всех последующих на весь channel_connect_timeout.
        // handle_lobby_client (и, соответственно, worker_loop) возвращается
        // сразу после этого — лобби готово к следующему клиенту немедленно.
        let handler = self.handler.clone();
        let clients_map = Arc::clone(&self.clients);
        let running = Arc::clone(&self.running);
        let channel_connect_timeout = self.options.channel_connect_timeout;
        let poll_timeout = self.options.poll_timeout;
        let join_handle = thread::spawn(move || {
            let (lock, cvar) = &*connect_signal;
            let deadline = std::time::Instant::now() + channel_connect_timeout;

            // Опрашиваем короткими интервалами (не одним долгим wait_timeout),
            // чтобы вовремя заметить остановку сервера -- иначе stop() мог бы
            // ждать этот поток до 30с, а хуже того, on_client_connect мог бы
            // выстрелить УЖЕ ПОСЛЕ того, как stop() вернулся бы без этой
            // проверки (аудит 2026-07-10: тот же класс UAF-гонки, что и в
            // shm_multi_server_stop/shm_dispatch_server_stop).
            let client_connected = loop {
                if !running.load(Ordering::Acquire) {
                    break false;
                }
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    break false;
                }
                let guard = lock.lock().unwrap();
                if *guard {
                    break true;
                }
                let (guard, _) = cvar.wait_timeout(guard, remaining.min(poll_timeout)).unwrap();
                if *guard {
                    break true;
                }
                drop(guard);
            };

            if !client_connected || !running.load(Ordering::Acquire) {
                auto_server.stop();
                return;
            }

            clients_map.write().unwrap().insert(
                client_id,
                DispatchedClient {
                    server: auto_server,
                    info: info.clone(),
                    channel_name,
                    disconnected: AtomicBool::new(false),
                },
            );

            handler.on_client_connect(client_id, &info);
        });

        // Регистрируем handle для join'а в stop(); заодно вычищаем уже
        // завершившиеся записи, чтобы вектор не рос неограниченно на
        // долгоживущем сервере с большим потоком регистраций.
        let mut pending = self.pending_connects.lock().unwrap();
        pending.retain(|h| !h.is_finished());
        pending.push(join_handle);
    }
}

impl Drop for DispatchServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(handle) = self.worker_handle.lock().unwrap().take() {
            let _ = handle.join();
        }
        let pending: Vec<_> = self.pending_connects.lock().unwrap().drain(..).collect();
        for handle in pending {
            let _ = handle.join();
        }
    }
}

// ─── AutoProxyHandler — bridges AutoServer events to DispatchHandler ─────────

struct AutoProxyHandler {
    client_id: u32,
    handler: Arc<dyn DispatchHandler>,
    clients: ClientMap,
    connect_signal: Arc<(Mutex<bool>, Condvar)>,
}

impl AutoHandler for AutoProxyHandler {
    fn on_connect(&self) {
        // Сигналим worker-потоку лобби, что клиент подключился к своему каналу
        let (lock, cvar) = &*self.connect_signal;
        let mut connected = lock.lock().unwrap();
        *connected = true;
        cvar.notify_one();
    }

    fn on_disconnect(&self) {
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
            thread::spawn(move || drop(dispatched_client));
            self.handler.on_client_disconnect(self.client_id);
        }
    }

    fn on_message(&self, _direction: ChannelKind, payload: &[u8]) {
        self.handler.on_message(self.client_id, payload);
    }

    fn on_error(&self, err: ShmError) {
        self.handler.on_error(Some(self.client_id), err);
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
    auto_client: Mutex<Option<AutoClient>>,
    running: Arc<AtomicBool>,
    client_id: u32,
    channel_name: String,
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

        let client_handler = Arc::new(DispatchClientProxy {
            handler: handler.clone(),
        });

        let auto_options = AutoOptions {
            connect_timeout: options.channel_timeout,
            poll_timeout: options.poll_timeout,
            max_send_queue: options.max_send_queue,
            recv_batch: options.recv_batch,
            ..AutoOptions::default()
        };

        let auto_client = AutoClient::connect(&assigned_channel, client_handler, auto_options)?;

        handler.on_connect(assigned_id, &assigned_channel);

        Ok(Self {
            auto_client: Mutex::new(Some(auto_client)),
            running,
            client_id: assigned_id,
            channel_name: assigned_channel,
        })
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

    /// Возвращает назначенный ID клиента.
    pub fn client_id(&self) -> u32 {
        self.client_id
    }

    /// Возвращает имя назначенного канала.
    pub fn channel_name(&self) -> &str {
        &self.channel_name
    }

    /// Проверяет подключение к выделенному каналу.
    pub fn is_connected(&self) -> bool {
        self.running.load(Ordering::Acquire) && self.auto_client.lock().unwrap().is_some()
    }

    /// Останавливает клиента и отключается.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
        let mut guard = self.auto_client.lock().unwrap();
        if let Some(client) = guard.take() {
            client.stop();
        }
    }
}

impl Drop for DispatchClient {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        let mut guard = self.auto_client.lock().unwrap();
        if let Some(client) = guard.take() {
            client.stop();
        }
    }
}

// ─── Регистрация в лобби (блокирующая) ──────────────────────────────────────

/// Выполняет регистрацию в лобби: подключение, отправка запроса, чтение ответа.
fn lobby_register(
    base_name: &str,
    registration: &ClientRegistration,
    options: &DispatchClientOptions,
    buffer: &mut Vec<u8>,
) -> Result<(u32, String)> {
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
                // Блокируемся на событии — просыпаемся, когда сервер запишет ответ
                let _ = events
                    .s2c
                    .data
                    .wait(Some(remaining.min(options.poll_timeout)));
                continue;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Proxy-обработчик, пересылающий события AutoClient в DispatchClientHandler.
struct DispatchClientProxy {
    handler: Arc<dyn DispatchClientHandler>,
}

impl AutoHandler for DispatchClientProxy {
    fn on_disconnect(&self) {
        self.handler.on_disconnect();
    }

    fn on_message(&self, _direction: ChannelKind, payload: &[u8]) {
        self.handler.on_message(payload);
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
    /// обязан синхронно дождаться выхода lobby worker-потока, иначе
    /// FFI-обёртка (shm_dispatch_server_stop) не может гарантировать, что
    /// callbacks перестали дёргаться до освобождения user_data.
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
            client_handler.clone(),
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
        let server =
            DispatchServer::start(&name, server_handler.clone(), DispatchOptions::default())
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
            client_handler.clone(),
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
            server_handler.clone(),
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
        let (_id_a, _channel_a) =
            lobby_register(&name, &reg_a, &DispatchClientOptions::default(), &mut buffer)
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
            client_handler_b.clone(),
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
}
