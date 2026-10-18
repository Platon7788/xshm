//! Rust-сторона межъязыковой проверки xshm <-> xipc (C, `xhook/ipc`).
//!
//! ```text
//! interop_peer <сценарий> <имя канала> [ключ=значение ...]
//! ```
//!
//! Пир запускается оркестратором (ctest-исполняемые `test_ipc_interop_*` в
//! `xhook/tests_static`, путь к пиру -- переменная `XIPC_RUST_PEER`) и
//! играет одну сторону одного сценария. Только публичный API xshm, без
//! зависимостей. Протокол stdout -- построчно и только:
//!
//! - `READY` -- объекты сервера созданы (`server-*`) / клиент подключён
//!   (`client-*`);
//! - `INFO <ключ>=<значение>` -- наблюдение: `err=<вариант ShmError>`,
//!   `reason=<вариант DisconnectReason>` (только ИМЯ варианта, без полей),
//!   `frames=<n>`, `layout=<s2c>/<c2s>`, `connected=1`;
//! - `PASS` (код 0) или `FAIL <текст>` (код 1); ошибка аргументов -- код 2;
//!   самотаймаут -- `FAIL timeout`: 60 с без прогресса (строки протокола,
//!   принятого кадра) или 600 с всего.
//!
//! Ключи: `s2c=<ёмкость>:<сообщений>:<макс. размер>`, `c2s=...` (раскладка
//! сервера; по умолчанию `RingConfig::DEFAULT`), `n=<кадров>` (3000; у
//! `multi-server` -- число слотов), `size=<байт>` (макс. длина кадра
//! генератора / длина кадра lossless: 65 535 / 30 720), `seed=<u32>` (1),
//! `mode=send|recv` (farewell) / `echo|ping` (`multi-client`),
//! `expect=<вариант>` (ожидаемая ошибка / причина), `sessions=<k>` (1),
//! `delay=<мс>` (500), `busy=1` (клиент повторяет и `AlreadyConnected`),
//! `count=<k>` (клиентов `multi-client`, 1), `ends=<k>` (отключений до
//! конца сценария Multi), `kick=<слот>` (`multi-server`), `frames=<k>` (10),
//! `release=<имя>` (маяк `<имя>_beacon`, которого `multi-client mode=ping`
//! ждёт перед остановкой; без него -- `hold=<мс>`, 0), `max_clients=<k>`
//! (`dispatch-server`, 64), `name=<s>` (имя регистрации `dispatch-client`,
//! `rust`), `boundary=<s>` (имя границы частного пространства имён `ns-*`),
//! `low=1` (граница с меткой Low: `ObjectSecurity::CurrentUserAllowLowIntegrity`).
//!
//! Сценарии (`server-*` -- `SharedServer`, ждёт клиента после `READY`;
//! `client-*` -- `SharedClient`, повторяет подключение, пока канала нет):
//!
//! - `server-echo`, `client-echo` -- эхо до ухода пира: кадры `0..n`
//!   сверяются с генератором, в конце `frames` и `reason=Graceful`;
//! - `*-lossless-sink` -- медленный читатель `n` кадров длины `size` (по
//!   порядку, без потерь); `*-lossless-source` -- писатель без потерь
//!   (`try_send_*` + `wait_for_space`), затем ждёт ухода пира;
//! - `*-farewell` -- `mode=send`: кадры длины 7, 8, 9 и уход (Drop);
//!   `mode=recv`: эти 3 кадра от пира и его `Graceful`;
//! - `*-die` -- 2 кадра длины 7, `INFO sent=2`, выход с кодом 9 без ухода
//!   (`process::exit`, без деструкторов);
//!   `*-expect-death` -- 2 кадра от пира и `reason=PeerDied` (клиент
//!   сначала шлёт кадр-готовность длины 1: пир умирает только после
//!   подключения клиента);
//! - `*-hold` -- держать сессию до ухода пира;
//! - `client-expect-busy` -- подключение обязано дать `AlreadyConnected`;
//!   `client-expect-err expect=<вариант>` -- ошибку `expect`;
//! - `auto-server-echo`, `auto-client-echo` -- `AutoServer`/`AutoClient`,
//!   эхо в `on_message`, печать `on_error`/`on_disconnect_reason`; конец --
//!   первое отключение (`expect=<причина>` -- его проверка);
//! - `server-sessions sessions=<k>` -- один сервер, `k` эхо-сессий подряд
//!   (каждая -- `n` кадров до ухода клиента, `INFO session=<i>`); провал
//!   рукопожатия (брошенная заявка умершего) -- `INFO handshake=HandshakeFailed`
//!   и ожидание дальше;
//! - `*-stall-leave delay=<мс>` -- сессия без чтения `delay` мс, затем уход;
//! - `*-fill-wait size=<байт> expect=<вариант>` -- `try_send_*` до
//!   `QueueFull` (`INFO full=<k>`), затем `wait_for_space` (20 с):
//!   `INFO err=<вариант>` (`Space` -- место появилось, `Timeout` -- нет);
//! - `client-hello-die` -- рукопожатие в потоке, через 500 мс (`INFO
//!   hello=1`) выход с кодом 9: заявка `CLIENT_HELLO` умершего остаётся;
//! - `client-ping` -- клиент сам ведёт эхо: `n` кадров по генератору, на
//!   каждый -- ответ XOR пира (`server-sessions`, эхо-сервер C), затем
//!   `INFO pinged=<n>` и уход (Drop: `IDLE`, `DISCONNECT`, claim последним);
//! - `multi-server n=<слотов> ends=<k> [kick=<слот>]` -- `MultiServer`, эхо
//!   XOR `0x5A`, `INFO connect=<слот>`, `INFO disconnect=<слот>:<причина>`;
//!   `kick` -- `disconnect_client` после первого кадра слота; конец -- `k`
//!   отключений;
//! - `multi-client count=<k> mode=echo|ping` -- `k` экземпляров
//!   `MultiClient` (`retry_delay` 30 с: без переподключения в сценарии),
//!   `INFO slot=<слот>`, `INFO reason=<причина>`; `echo` -- ответ XOR на
//!   кадры сервера, конец -- `ends` отключений (по умолчанию `count`);
//!   `ping` -- `frames` кадров длины 16 от каждого и их эхо (`INFO
//!   pinged=<k>`), затем сессии держатся до взвода маяка `release` (или
//!   `hold` мс) и остановка;
//! - `dispatch-server max_clients=<k> ends=<e>` -- `DispatchServer` (лобби
//!   `DEFAULT_LOBBY_LAYOUT`), эхо XOR `0x5A`, `INFO connect=<id>:<имя>:<pid>`,
//!   `INFO disconnect=<id>:<причина>`, `INFO err=<вариант>` (`on_error`,
//!   например отказ по `max_clients`); конец -- `e` отключений (1);
//! - `dispatch-client name=<s> count=<k> mode=ping|echo` -- `k` экземпляров
//!   `DispatchClient`, регистрирующихся одновременно; `INFO channel=<id>:<канал>`;
//!   `ping` -- `frames` кадров длины 16 от каждого и их эхо (`INFO
//!   pinged=<k>`), затем уход; `echo` -- ответ XOR на кадры сервера до
//!   `ends` разрывов (`INFO reason=`); `expect=<вариант>` -- регистрация
//!   обязана провалиться этой ошибкой (`INFO err=`; отказ лобби --
//!   `HandshakeFailed`);
//! - `beacon-server` -- `dispatch-server` с явным маяком `<лобби>_beacon`:
//!   `Beacon::raise` после старта лобби, `lower` перед остановкой;
//!   `beacon-server-crash` -- лобби и `raise`, после `READY` выход с кодом 9
//!   без `lower` (маяк упавшего сервера остаётся взведённым);
//! - `beacon-client` -- `Beacon::open` (`INFO waiting=1`), ожидание взвода
//!   (20 с), затем `INFO lobby=<0|1>` (`DispatchClient::lobby_exists`),
//!   `INFO cleared=<true|false>` (`clear_if_stale`), `INFO raised=<0|1>`;
//! - `ns-server`, `ns-client`, `ns-multi-server` -- `server-echo`,
//!   `client-echo`, `multi-server` в частном пространстве имён границы
//!   `boundary` (`PrivateNamespace::open_or_create`, псевдоним `rust`;
//!   `low=1` -- граница с меткой Low); перед остальным -- `INFO
//!   ns=created|opened` (создал ли пространство этот пир).
//!
//! Писатель без потерь печатает `INFO waits=<k>` (сколько раз ждал места)
//! перед `INFO sent=`.
//!
//! Генератор кадров (общий с C-стороной): `x = xorshift32(x)` (начальное
//! `x = seed`, `0` заменяется на `1`); кадр `k` эхо-сценария имеет длину
//! `1 + x % size`, байт `i` кадра `k` -- `(k * 31 + i) & 0xFF`. Эхо-ответ --
//! байты запроса XOR `0x5A`, усечённые до `max_message_size` направления
//! ответа (асимметричная раскладка). Кадры после `n`-го длину по генератору
//! не сверяют (только узор) -- ими C-сторона проверяет крайние длины.

use std::io::Write;
use std::process::ExitCode;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::task::{Context, Poll, Wake, Waker};
use std::thread;
use std::time::{Duration, Instant};

use xshm::{
    AutoClient, AutoHandler, AutoOptions, AutoServer, Beacon, ChannelConfig, ChannelKind,
    ChannelLayout, ClientRegistration, DisconnectReason, DispatchClient, DispatchClientHandler,
    DispatchClientOptions, DispatchHandler, DispatchOptions, DispatchServer, MultiClient,
    MultiClientHandler, MultiClientOptions, MultiHandler, MultiOptions, MultiServer,
    ObjectSecurity, PrivateNamespace, RingConfig, SharedClient, SharedServer, ShmError,
};

/// Самотаймаут пира: оркестратор не должен ждать зависший пир. Зависание --
/// это отсутствие прогресса (`progress`) дольше этого срока, а не общая
/// длительность: под нагрузкой CPU эхо из 3000 кадров идёт медленно, но идёт.
const SELF_TIMEOUT: Duration = Duration::from_secs(60);

/// Абсолютный предел жизни пира (и при медленном прогрессе).
const HARD_TIMEOUT: Duration = Duration::from_secs(600);

/// Шаг сторожевого потока.
const WATCHDOG_STEP: Duration = Duration::from_secs(1);

/// Счётчик прогресса: строки протокола и принятые кадры.
static PROGRESS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Отметить прогресс (сторожевой поток `main` сверяет счётчик).
fn progress() {
    // Relaxed: только счётчик для сторожа, ничего не публикует.
    PROGRESS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Сколько клиент ждёт ответа сервера на одну попытку подключения.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Сколько клиент повторяет подключение к ещё не созданному каналу.
const CONNECT_RETRY_FOR: Duration = Duration::from_secs(10);

/// Сколько клиент с `busy=1` повторяет подключение к занятому каналу:
/// дольше `RESERVE_TIMEOUT` (10 с) -- протухший захват снимается по толчку.
const CONNECT_RETRY_BUSY_FOR: Duration = Duration::from_secs(25);

/// Ожидание места в `*-fill-wait`.
const SPACE_WAIT: Duration = Duration::from_secs(20);

/// Сколько `beacon-client` ждёт взвода маяка.
const BEACON_WAIT: Duration = Duration::from_secs(20);

/// Строка протокола в stdout. Ошибку записи (оркестратор закрыл канал)
/// некому сообщить -- пир всё равно завершится по сценарию или самотаймауту.
fn out(line: &str) {
    progress();
    let mut stdout = std::io::stdout().lock();
    drop(writeln!(stdout, "{line}"));
    drop(stdout.flush());
}

/// Генератор кадров, общий с C-стороной (`ipc_interop.h`).
mod payload {
    /// Поток длин кадров эхо-сценария: `1 + xorshift32 % max`.
    #[derive(Debug)]
    pub struct Lengths {
        state: u32,
        max: u32,
    }

    impl Lengths {
        pub const fn new(seed: u32, max: u32) -> Self {
            Self {
                state: if seed == 0 { 1 } else { seed },
                max: if max == 0 { 1 } else { max },
            }
        }

        /// Длина следующего кадра.
        pub const fn next_len(&mut self) -> usize {
            let mut x = self.state;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.state = x;
            (1 + x % self.max) as usize
        }
    }

    /// Байт `i` кадра `k`.
    const fn byte(k: u32, i: usize) -> u8 {
        (k.wrapping_mul(31).wrapping_add(i as u32) & 0xFF) as u8
    }

    /// Кадр `k` длины `len`.
    pub fn frame(k: u32, len: usize) -> Vec<u8> {
        (0..len).map(|i| byte(k, i)).collect()
    }

    /// Кадр `k` -- узор генератора.
    pub fn is_frame(k: u32, data: &[u8]) -> bool {
        data.iter().enumerate().all(|(i, &b)| b == byte(k, i))
    }
}

/// Имя варианта `ShmError` -- явный `match`, без `Debug`/`Display`: у
/// структурных вариантов (`VersionMismatch { .. }`) они печатают поля.
const fn err_name(err: &ShmError) -> &'static str {
    match err {
        ShmError::NotConnected => "NotConnected",
        ShmError::NotReady => "NotReady",
        ShmError::AlreadyConnected => "AlreadyConnected",
        ShmError::Timeout => "Timeout",
        ShmError::QueueEmpty => "QueueEmpty",
        ShmError::QueueFull => "QueueFull",
        ShmError::MessageTooSmall => "MessageTooSmall",
        ShmError::MessageTooLarge => "MessageTooLarge",
        ShmError::Corrupted => "Corrupted",
        ShmError::Overrun => "Overrun",
        ShmError::HandshakeFailed => "HandshakeFailed",
        ShmError::VersionMismatch { .. } => "VersionMismatch",
        ShmError::WindowsError { .. } => "WindowsError",
        ShmError::AccessDenied => "AccessDenied",
        ShmError::NoFreeSlot => "NoFreeSlot",
        ShmError::InvalidConfig(_) => "InvalidConfig",
        ShmError::PeerDied => "PeerDied",
        ShmError::WorkerPanicked { .. } => "WorkerPanicked",
        _ => "Unknown",
    }
}

/// Имя варианта `DisconnectReason` (явный `match`).
const fn reason_name(reason: DisconnectReason) -> &'static str {
    match reason {
        DisconnectReason::Graceful => "Graceful",
        DisconnectReason::PeerDied => "PeerDied",
        DisconnectReason::Local => "Local",
        DisconnectReason::Error => "Error",
        _ => "Unknown",
    }
}

// ============================================================================
// Аргументы
// ============================================================================

/// Разобранные `ключ=значение`.
#[derive(Debug)]
struct Args {
    n: u32,
    size: Option<u32>,
    seed: u32,
    layout: ChannelLayout,
    mode: String,
    expect: String,
    sessions: u32,
    delay: u64,
    busy: bool,
    count: u32,
    ends: Option<u32>,
    kick: Option<u32>,
    frames: u32,
    hold: u64,
    release: String,
    max_clients: usize,
    cname: String,
    boundary: String,
    low: bool,
}

/// `<ёмкость>:<сообщений>:<макс. размер>`.
fn parse_ring(text: &str) -> Option<RingConfig> {
    let mut parts = text.split(':');
    let capacity = parts.next()?.parse().ok()?;
    let max_messages = parts.next()?.parse().ok()?;
    let max_message_size = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(
        RingConfig::new(capacity)
            .max_messages(max_messages)
            .max_message_size(max_message_size),
    )
}

fn parse_args(pairs: &[String]) -> Result<Args, String> {
    let mut args = Args {
        n: 3000,
        size: None,
        seed: 1,
        layout: ChannelLayout::DEFAULT,
        mode: String::new(),
        expect: String::new(),
        sessions: 1,
        delay: 500,
        busy: false,
        count: 1,
        ends: None,
        kick: None,
        frames: 10,
        hold: 0,
        release: String::new(),
        max_clients: DispatchOptions::default().max_clients,
        cname: "rust".to_owned(),
        boundary: String::new(),
        low: false,
    };
    for pair in pairs {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| format!("bad argument {pair}"))?;
        let bad = || format!("bad value of {key}: {value}");
        match key {
            "n" => args.n = value.parse().map_err(|_| bad())?,
            "size" => args.size = Some(value.parse().map_err(|_| bad())?),
            "seed" => args.seed = value.parse().map_err(|_| bad())?,
            "s2c" => args.layout.server_to_client = parse_ring(value).ok_or_else(bad)?,
            "c2s" => args.layout.client_to_server = parse_ring(value).ok_or_else(bad)?,
            "mode" => value.clone_into(&mut args.mode),
            "expect" => value.clone_into(&mut args.expect),
            "sessions" => args.sessions = value.parse().map_err(|_| bad())?,
            "delay" => args.delay = value.parse().map_err(|_| bad())?,
            "busy" => args.busy = value == "1",
            "count" => args.count = value.parse().map_err(|_| bad())?,
            "ends" => args.ends = Some(value.parse().map_err(|_| bad())?),
            "kick" => args.kick = Some(value.parse().map_err(|_| bad())?),
            "frames" => args.frames = value.parse().map_err(|_| bad())?,
            "hold" => args.hold = value.parse().map_err(|_| bad())?,
            "release" => value.clone_into(&mut args.release),
            "max_clients" => args.max_clients = value.parse().map_err(|_| bad())?,
            "name" => value.clone_into(&mut args.cname),
            "boundary" => value.clone_into(&mut args.boundary),
            "low" => args.low = value == "1",
            _ => return Err(format!("unknown key {key}")),
        }
    }
    Ok(args)
}

/// Частное пространство имён границы `boundary` (обязательна для `ns-*`);
/// `low=1` -- граница с меткой Low. Печатает `INFO ns=created|opened`.
fn namespace(args: &Args) -> Result<Arc<PrivateNamespace>, String> {
    if args.boundary.is_empty() {
        return Err("boundary= is required".to_owned());
    }
    let security = if args.low {
        ObjectSecurity::CurrentUserAllowLowIntegrity
    } else {
        ObjectSecurity::default()
    };
    let ns = PrivateNamespace::open_or_create(&args.boundary, "rust", &security)
        .map_err(|e| format!("namespace: {}", err_name(&e)))?;
    out(if ns.created() {
        "INFO ns=created"
    } else {
        "INFO ns=opened"
    });
    Ok(ns)
}

fn ring_text(ring: &RingConfig) -> String {
    format!(
        "{}:{}:{}",
        ring.capacity, ring.max_messages, ring.max_message_size
    )
}

fn layout_text(layout: &ChannelLayout) -> String {
    format!(
        "{}/{}",
        ring_text(&layout.server_to_client),
        ring_text(&layout.client_to_server)
    )
}

// ============================================================================
// Минимальный исполнитель для `recv_async` (только std)
// ============================================================================

/// Будит поток, ждущий в `block_on`.
struct Unpark(thread::Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Опрашивать future, пока не готов; между опросами -- сон до `wake`.
fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
        thread::park();
    }
}

// ============================================================================
// Сторона single-client канала
// ============================================================================

/// Сервер или клиент single-client канала.
enum Side {
    Server(SharedServer),
    Client(SharedClient),
}

impl Side {
    /// Сервер `name` с раскладкой `layout`: `READY` после создания объектов,
    /// затем ожидание клиента (`HandshakeFailed` -- брошенная заявка --
    /// не конец: ждём следующего).
    fn serve(name: &str, layout: ChannelLayout) -> Result<Self, String> {
        Self::serve_in(name, layout, None)
    }

    /// `serve`; `ns` -- объекты канала в частном пространстве имён.
    fn serve_in(
        name: &str,
        layout: ChannelLayout,
        ns: Option<Arc<PrivateNamespace>>,
    ) -> Result<Self, String> {
        let mut config = ChannelConfig::new(layout);
        if let Some(ns) = ns {
            config = config.namespace(ns);
        }
        let mut server = SharedServer::start_with(name, &config)
            .map_err(|e| format!("start: {}", err_name(&e)))?;
        out("READY");
        loop {
            match server.wait_for_client(None) {
                Ok(()) => break,
                Err(ShmError::HandshakeFailed) => {}
                Err(e) => return Err(format!("accept: {}", err_name(&e))),
            }
        }
        out("INFO connected=1");
        Ok(Self::Server(server))
    }

    /// Клиент `name` (повтор, пока канала ещё нет); `READY` -- подключён.
    fn connect(name: &str) -> Result<Self, String> {
        Self::connect_with(name, false, None)
    }

    /// `connect`; `busy` -- повторять и `AlreadyConnected` (канал держит
    /// чужой захват: каждая попытка толкает сервер `CONNECT_REQ`, и тот
    /// снимает протухший захват через `RESERVE_TIMEOUT`), с `INFO busy_ms=`;
    /// `ns` -- канал в частном пространстве имён (`connect_in`).
    fn connect_with(name: &str, busy: bool, ns: Option<&PrivateNamespace>) -> Result<Self, String> {
        let started = Instant::now();
        let mut busy_seen = false;
        let client = loop {
            let attempt = match ns {
                Some(ns) => SharedClient::connect_in(ns, name, CONNECT_TIMEOUT),
                None => SharedClient::connect(name, CONNECT_TIMEOUT),
            };
            match attempt {
                Ok(client) => break client,
                Err(ShmError::WindowsError { .. } | ShmError::NotReady)
                    if started.elapsed() < CONNECT_RETRY_FOR =>
                {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(ShmError::AlreadyConnected)
                    if busy && started.elapsed() < CONNECT_RETRY_BUSY_FOR =>
                {
                    busy_seen = true;
                    thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return Err(format!("connect: {}", err_name(&e))),
            }
        };
        if busy_seen {
            out(&format!("INFO busy_ms={}", started.elapsed().as_millis()));
        }
        out("READY");
        out(&format!("INFO layout={}", layout_text(client.layout())));
        Ok(Self::Client(client))
    }

    /// Предел payload исходящего направления.
    const fn tx_max(&self) -> usize {
        match self {
            Self::Server(s) => s.layout().server_to_client.max_message_size,
            Self::Client(c) => c.layout().client_to_server.max_message_size,
        }
    }

    /// Следующее сообщение; `Err(причина)` -- пир ушёл (кольцо дочитано).
    fn recv(&mut self, buf: &mut Vec<u8>) -> Result<usize, Ended> {
        let result = match self {
            Self::Server(s) => block_on(s.recv_async(buf)),
            Self::Client(c) => block_on(c.recv_async(buf)),
        };
        result.map_err(|e| match e {
            // `recv_async`: отключение пира -- `NotConnected`, смерть его
            // процесса -- `PeerDied`; обе -- после дочитанного кольца, и
            // сторона отмечена отключённой. `NotConnected` при стороне, всё
            // ещё «подключённой», -- не уход пира, а чужая сессия клиента
            // (`generation` сменился, `ensure_connected`): это ошибка
            // сценария, а не `Graceful`.
            ShmError::NotConnected if !self.is_connected() => {
                Ended::Gone(DisconnectReason::Graceful)
            }
            ShmError::PeerDied if !self.is_connected() => Ended::Gone(DisconnectReason::PeerDied),
            other => Ended::Failed(other),
        })
    }

    const fn is_connected(&self) -> bool {
        match self {
            Self::Server(s) => s.is_connected(),
            Self::Client(c) => c.is_connected(),
        }
    }

    /// Запись с перезаписью (эхо: в кольце не больше одного кадра).
    fn send(&self, data: &[u8]) -> Result<(), ShmError> {
        match self {
            Self::Server(s) => s.send_to_client(data).map(drop),
            Self::Client(c) => c.send_to_server(data).map(drop),
        }
    }

    /// Запись без перезаписи: `Err(QueueFull)` -- места нет, кольцо не
    /// тронуто.
    fn try_send(&self, data: &[u8]) -> Result<(), ShmError> {
        match self {
            Self::Server(s) => s.try_send_to_client(data).map(drop),
            Self::Client(c) => c.try_send_to_server(data).map(drop),
        }
    }

    /// Ожидание места под `len` байт (`Ok(false)` -- таймаут).
    fn wait_for_space(&self, len: usize, timeout: Option<Duration>) -> Result<bool, ShmError> {
        match self {
            Self::Server(s) => s.wait_for_space(len, timeout),
            Self::Client(c) => c.wait_for_space(len, timeout),
        }
    }

    /// Запись без потерь: `try_send_*`, нет места -- `wait_for_space`.
    /// Возвращает, сколько раз пришлось ждать.
    fn send_lossless(&self, data: &[u8]) -> Result<u32, ShmError> {
        let mut waits = 0;
        loop {
            match self.try_send(data) {
                Err(ShmError::QueueFull) => {}
                other => return other.map(|()| waits),
            }
            waits += 1;
            self.wait_for_space(data.len(), None)?;
        }
    }
}

/// Конец приёма.
enum Ended {
    /// Пир ушёл: причина.
    Gone(DisconnectReason),
    /// Ошибка приёма.
    Failed(ShmError),
}

/// Ждать ухода пира, принимая кадры через `on_frame`; печатает `frames` и
/// `reason`. Возвращает причину и число кадров.
fn until_gone(
    side: &mut Side,
    mut on_frame: impl FnMut(&Side, u32, &[u8]) -> Result<(), String>,
) -> Result<(DisconnectReason, u32), String> {
    let mut buf = Vec::new();
    let mut k = 0u32;
    loop {
        match side.recv(&mut buf) {
            Ok(len) => {
                let frame = buf.get(..len).ok_or("bad length")?;
                on_frame(side, k, frame)?;
                progress();
                k += 1;
            }
            Err(Ended::Gone(reason)) => {
                out(&format!("INFO frames={k}"));
                out(&format!("INFO reason={}", reason_name(reason)));
                return Ok((reason, k));
            }
            Err(Ended::Failed(e)) => {
                out(&format!("INFO err={}", err_name(&e)));
                return Err(format!("recv frame {k}: {}", err_name(&e)));
            }
        }
    }
}

/// Причина и число кадров -- как ожидалось.
fn expect_end(
    got: (DisconnectReason, u32),
    reason: DisconnectReason,
    frames: u32,
) -> Result<(), String> {
    if got != (reason, frames) {
        return Err(format!(
            "end {}/{} != expected {}/{frames}",
            reason_name(got.0),
            got.1,
            reason_name(reason)
        ));
    }
    Ok(())
}

// ============================================================================
// Сценарии single-client
// ============================================================================

/// Эхо: кадры `0..n` сверяются с генератором, ответ -- XOR `0x5A` (усечён
/// до предела направления ответа); конец -- уход пира (`Graceful`).
fn echo(side: &mut Side, args: &Args) -> Result<(), String> {
    let mut lengths = payload::Lengths::new(args.seed, args.size.unwrap_or(65_535));
    let n = args.n;
    let got = until_gone(side, |side, k, frame| {
        if k < n {
            let want = lengths.next_len();
            if frame.len() != want {
                return Err(format!("frame {k}: length {} != {want}", frame.len()));
            }
        }
        if !payload::is_frame(k, frame) {
            return Err(format!("frame {k}: wrong bytes"));
        }
        let take = frame.len().min(side.tx_max());
        let reply: Vec<u8> = frame.iter().take(take).map(|b| b ^ 0x5A).collect();
        side.send(&reply)
            .map_err(|e| format!("echo {k}: {}", err_name(&e)))
    })?;
    if got.1 < n {
        return Err(format!("only {} of {n} frames", got.1));
    }
    expect_end((got.0, n), DisconnectReason::Graceful, n)
}

/// Медленный читатель без потерь: `n` кадров длины `size` по порядку.
fn lossless_sink(side: &mut Side, args: &Args) -> Result<(), String> {
    let size = args.size.unwrap_or(30_720) as usize;
    let got = until_gone(side, |_, k, frame| {
        if frame.len() != size || !payload::is_frame(k, frame) {
            return Err(format!("frame {k}: lost, reordered or damaged"));
        }
        // Медленный читатель: писатель упирается в полное кольцо.
        if k % 16 == 15 {
            thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    })?;
    expect_end(got, DisconnectReason::Graceful, args.n)
}

/// Писатель без потерь (`try_send_*` + `wait_for_space`), затем ждёт ухода
/// пира (читатель уходит, дочитав всё).
fn lossless_source(side: &mut Side, args: &Args) -> Result<(), String> {
    let size = args.size.unwrap_or(30_720) as usize;
    let mut waits = 0;
    for k in 0..args.n {
        waits += side
            .send_lossless(&payload::frame(k, size))
            .map_err(|e| format!("send {k}: {}", err_name(&e)))?;
    }
    out(&format!("INFO waits={waits}"));
    out(&format!("INFO sent={}", args.n));
    let got = until_gone(side, |_, k, _| Err(format!("unexpected frame {k}")))?;
    expect_end(got, DisconnectReason::Graceful, 0)
}

/// Прощание: `mode=send` -- 3 кадра и уход (Drop: состояние `IDLE`, потом
/// `DISCONNECT`); `mode=recv` -- 3 кадра от пира до его `Graceful`.
fn farewell(side: Side, args: &Args) -> Result<(), String> {
    match args.mode.as_str() {
        "send" => {
            for k in 0..3 {
                side.send(&payload::frame(k, 7 + k as usize))
                    .map_err(|e| format!("send {k}: {}", err_name(&e)))?;
            }
            drop(side);
            Ok(())
        }
        "recv" => {
            let mut side = side;
            let got = until_gone(&mut side, |_, k, frame| {
                if frame.len() != 7 + k as usize || !payload::is_frame(k, frame) {
                    return Err(format!("frame {k}: wrong"));
                }
                Ok(())
            })?;
            expect_end(got, DisconnectReason::Graceful, 3)
        }
        other => Err(format!("mode {other}")),
    }
}

/// Код выхода сценариев `*-die`.
const DIE_EXIT_CODE: i32 = 9;

/// Смерть: 2 кадра и завершение процесса без ухода. `process::exit` --
/// `ExitProcess` без деструкторов: ни `IDLE`, ни `DISCONNECT`, пир узнаёт
/// о смерти только по handle процесса. Не `abort()`: `__fastfail` проходит
/// через Windows Error Reporting, и задержка отчёта (LocalDumps, очередь
/// WER) отодвигала бы завершение процесса за границу 1 с C-стороны.
fn die(side: &Side) -> Result<(), String> {
    for k in 0..2 {
        side.send(&payload::frame(k, 7))
            .map_err(|e| format!("send {k}: {}", err_name(&e)))?;
    }
    out("INFO sent=2");
    std::process::exit(DIE_EXIT_CODE);
}

/// Ждать смерти пира: 2 кадра, затем `PeerDied`.
fn expect_death(side: &mut Side) -> Result<(), String> {
    let got = until_gone(side, |_, k, frame| {
        if frame.len() != 7 || !payload::is_frame(k, frame) {
            return Err(format!("frame {k}: wrong"));
        }
        Ok(())
    })?;
    expect_end(got, DisconnectReason::PeerDied, 2)
}

/// Клиент ждёт смерти сервера. Сначала кадр-готовность (1 байт): сервер C
/// умирает только после него, то есть когда клиент уже подключён и его
/// сессия жива -- исход сценария (2 кадра, затем `PeerDied`) не зависит от
/// того, в какой момент версия xshm открывает процесс сервера. Проверено
/// воспроизведением (17.10.2026): до 841e00c синхронный `connect` открывал
/// процесс сервера только в `finish()`, после `S2C_CONNECT`; сервер,
/// успевший умереть между S7 и этим шагом, отбрасывался
/// `ProcessWatch::open_peer` (завершившийся процесс), и клиент оставался
/// подключённым без наблюдения -- вечное ожидание (5 из 300 прогонов без
/// кадра). С 841e00c наблюдение берётся в `Hello::post` до C9 (0 из 300).
fn client_expect_death(name: &str) -> Result<(), String> {
    let mut side = Side::connect(name)?;
    side.send(&[0x01])
        .map_err(|e| format!("ready: {}", err_name(&e)))?;
    expect_death(&mut side)
}

/// Держать сессию до ухода пира (кадры игнорируются).
fn hold(side: &mut Side) -> Result<(), String> {
    until_gone(side, |_, _, _| Ok(())).map(drop)
}

/// Подключение должно провалиться ошибкой `expect`.
fn client_expect_err(name: &str, expect: &str) -> Result<(), String> {
    match SharedClient::connect(name, Duration::from_secs(2)) {
        Ok(_) => Err("connected".to_owned()),
        Err(e) => {
            out(&format!("INFO err={}", err_name(&e)));
            if err_name(&e) == expect {
                Ok(())
            } else {
                Err(format!("expected {expect}"))
            }
        }
    }
}

/// Один сервер, `sessions` эхо-сессий подряд: повторное принятие на том же
/// канале (claim предыдущего клиента снят им самим, `generation + 1`).
fn server_sessions(name: &str, args: &Args) -> Result<(), String> {
    let mut server = SharedServer::start_with(name, &ChannelConfig::new(args.layout))
        .map_err(|e| format!("start: {}", err_name(&e)))?;
    out("READY");
    for session in 1..=args.sessions {
        loop {
            match server.wait_for_client(None) {
                Ok(()) => break,
                Err(ShmError::HandshakeFailed) => out("INFO handshake=HandshakeFailed"),
                Err(e) => return Err(format!("accept {session}: {}", err_name(&e))),
            }
        }
        out(&format!("INFO session={session}"));
        let mut side = Side::Server(server);
        echo(&mut side, args)?;
        let Side::Server(back) = side else {
            return Err("side changed".to_owned());
        };
        server = back;
    }
    Ok(())
}

/// Сессия без чтения `delay` мс, затем уход (Drop: состояние `IDLE`,
/// потом `DISCONNECT`): пир, ждущий места, будит только `DISCONNECT`.
fn stall_leave(side: Side, args: &Args) -> Result<(), String> {
    thread::sleep(Duration::from_millis(args.delay));
    out("INFO leaving=1");
    drop(side);
    Ok(())
}

/// Заполнить кольцо (`INFO full=<k>`), затем ждать места: итог --
/// `INFO err=<вариант>` и сверка с `expect`.
fn fill_wait(side: &Side, args: &Args) -> Result<(), String> {
    let size = args.size.unwrap_or(1024) as usize;
    let frame = payload::frame(0, size);
    let mut sent = 0u32;
    loop {
        match side.try_send(&frame) {
            Ok(()) => sent += 1,
            Err(ShmError::QueueFull) => break,
            Err(e) => return Err(format!("fill: {}", err_name(&e))),
        }
    }
    out(&format!("INFO full={sent}"));
    let got = match side.wait_for_space(size, Some(SPACE_WAIT)) {
        Ok(true) => "Space",
        Ok(false) => "Timeout",
        Err(e) => err_name(&e),
    };
    out(&format!("INFO err={got}"));
    if got == args.expect {
        Ok(())
    } else {
        Err(format!("expected {}", args.expect))
    }
}

/// Клиент сам ведёт эхо: `n` кадров по генератору (`seed`, `size`), на
/// каждый -- ответ XOR пира, усечённый до предела направления ответа; затем
/// уход (Drop). Подключение без повтора `AlreadyConnected`: claim
/// предыдущего клиента канала обязан быть уже снят.
fn client_ping(name: &str, args: &Args) -> Result<(), String> {
    let mut side = Side::connect(name)?;
    let rx_max = match &side {
        Side::Client(c) => c.layout().server_to_client.max_message_size,
        Side::Server(_) => return Err("side".to_owned()),
    };
    let mut lengths = payload::Lengths::new(args.seed, args.size.unwrap_or(65_535));
    let mut buf = Vec::new();
    for k in 0..args.n {
        let frame = payload::frame(k, lengths.next_len());
        side.send(&frame)
            .map_err(|e| format!("send {k}: {}", err_name(&e)))?;
        let len = match side.recv(&mut buf) {
            Ok(len) => len,
            Err(Ended::Gone(r)) => return Err(format!("gone at {k}: {}", reason_name(r))),
            Err(Ended::Failed(e)) => return Err(format!("recv {k}: {}", err_name(&e))),
        };
        let reply = buf.get(..len).ok_or("bad length")?;
        let want = xor_reply(&frame, rx_max);
        if reply != want.as_slice() {
            return Err(format!("echo {k}: wrong"));
        }
    }
    out(&format!("INFO pinged={}", args.n));
    drop(side);
    Ok(())
}

/// Клиент, умерший со стоящей заявкой: рукопожатие в потоке (сервер не
/// отвечает), через 500 мс -- выход без деструкторов. Остаются `CLIENT_HELLO`,
/// claim и самоописание умершего -- их снимает S2 сервера.
fn client_hello_die(name: &str) -> Result<(), String> {
    let name = name.to_owned();
    thread::spawn(move || drop(SharedClient::connect(&name, Duration::from_secs(30))));
    thread::sleep(Duration::from_millis(500));
    out("INFO hello=1");
    std::process::exit(DIE_EXIT_CODE);
}

// ============================================================================
// Сценарии Multi (MultiServer / MultiClient)
// ============================================================================

/// Событие обработчика Multi для `main`.
enum MultiEvent {
    /// Кадр от слота.
    Message(u32),
    /// Отключение (сервер: слота; клиент: своё).
    Ended,
    /// Клиент `ping` получил эхо всех своих кадров.
    Pinged,
}

/// Обработчик `MultiServer`: печать подключений и отключений, эхо XOR.
struct MultiEcho {
    server: OnceLock<Weak<MultiServer>>,
    max: usize,
    events: mpsc::Sender<MultiEvent>,
}

impl MultiHandler for MultiEcho {
    fn on_client_connect(&self, client_id: u32) {
        out(&format!("INFO connect={client_id}"));
    }

    fn on_client_disconnect(&self, client_id: u32) {
        self.on_client_disconnect_reason(client_id, DisconnectReason::Graceful);
    }

    fn on_client_disconnect_reason(&self, client_id: u32, reason: DisconnectReason) {
        out(&format!(
            "INFO disconnect={client_id}:{}",
            reason_name(reason)
        ));
        let _ = self.events.send(MultiEvent::Ended);
    }

    fn on_message(&self, client_id: u32, data: &[u8]) {
        if let Some(server) = self.server.get().and_then(Weak::upgrade)
            && let Err(e) = server.send_to(client_id, &xor_reply(data, self.max))
        {
            out(&format!("INFO err={}", err_name(&e)));
        }
        let _ = self.events.send(MultiEvent::Message(client_id));
    }

    fn on_error(&self, _client_id: Option<u32>, err: ShmError) {
        out(&format!("INFO err={}", err_name(&err)));
    }
}

/// `MultiServer` на `n` слотов до `ends` отключений; `kick` -- отключить
/// слот сервером после его первого кадра; `ns` -- слоты в частном
/// пространстве имён.
fn multi_server(name: &str, args: &Args, ns: Option<Arc<PrivateNamespace>>) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    let handler = Arc::new(MultiEcho {
        server: OnceLock::new(),
        max: args.layout.server_to_client.max_message_size,
        events: tx,
    });
    let dyn_handler: Arc<dyn MultiHandler> = handler.clone();
    let options = MultiOptions {
        max_clients: args.n,
        layout: args.layout,
        namespace: ns,
        ..MultiOptions::default()
    };
    let server = MultiServer::start(name, dyn_handler, options)
        .map_err(|e| format!("start: {}", err_name(&e)))?;
    drop(handler.server.set(Arc::downgrade(&server)));
    out("READY");
    let ends = args.ends.unwrap_or(args.n);
    let mut ended = 0;
    let mut kicked = false;
    while ended < ends {
        match rx.recv().map_err(|_| "handler gone".to_owned())? {
            MultiEvent::Message(id) if args.kick == Some(id) && !kicked => {
                kicked = true;
                server
                    .disconnect_client(id)
                    .map_err(|e| format!("kick {id}: {}", err_name(&e)))?;
            }
            MultiEvent::Ended => ended += 1,
            MultiEvent::Message(_) | MultiEvent::Pinged => {}
        }
    }
    server.stop();
    Ok(())
}

/// Обработчик одного `MultiClient`: `echo` -- ответ XOR на кадры сервера;
/// `ping` -- `frames` кадров при подключении и сверка их эха.
struct MultiClientEcho {
    client: OnceLock<Weak<MultiClient>>,
    ping: bool,
    frames: u32,
    replies: std::sync::atomic::AtomicU32,
    events: mpsc::Sender<MultiEvent>,
}

impl MultiClientEcho {
    fn client(&self) -> Option<Arc<MultiClient>> {
        self.client.get().and_then(Weak::upgrade)
    }
}

/// Длина кадра `ping`.
const PING_LEN: usize = 16;

impl MultiClientHandler for MultiClientEcho {
    fn on_connect(&self, slot_id: u32) {
        out(&format!("INFO slot={slot_id}"));
        if self.ping
            && let Some(client) = self.client()
        {
            for k in 0..self.frames {
                if let Err(e) = client.send(&payload::frame(k, PING_LEN)) {
                    out(&format!("INFO err={}", err_name(&e)));
                }
            }
        }
    }

    fn on_disconnect(&self) {}

    fn on_disconnect_reason(&self, reason: DisconnectReason) {
        out(&format!("INFO reason={}", reason_name(reason)));
        let _ = self.events.send(MultiEvent::Ended);
    }

    fn on_message(&self, data: &[u8]) {
        if self.ping {
            let k = self
                .replies
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            let want: Vec<u8> = payload::frame(k, PING_LEN)
                .iter()
                .map(|b| b ^ 0x5A)
                .collect();
            if data != want.as_slice() {
                out(&format!("INFO err=echo{k}"));
            } else if k + 1 == self.frames {
                let _ = self.events.send(MultiEvent::Pinged);
            }
            return;
        }
        if let Some(client) = self.client() {
            let max = client
                .layout()
                .map_or(data.len(), |l| l.client_to_server.max_message_size);
            if let Err(e) = client.send(&xor_reply(data, max)) {
                out(&format!("INFO err={}", err_name(&e)));
            }
        }
    }

    fn on_error(&self, err: ShmError) {
        out(&format!("INFO err={}", err_name(&err)));
    }
}

/// `count` экземпляров `MultiClient` на базовом имени `name`.
fn multi_client(name: &str, args: &Args) -> Result<(), String> {
    let ping = match args.mode.as_str() {
        "" | "echo" => false,
        "ping" => true,
        other => return Err(format!("mode {other}")),
    };
    let (tx, rx) = mpsc::channel();
    let options = MultiClientOptions {
        // Без переподключения внутри сценария: отключённый клиент ждёт
        // паузу, которую прерывает `stop`.
        retry_delay: Duration::from_secs(30),
        ..MultiClientOptions::default()
    };
    let mut clients = Vec::new();
    for _ in 0..args.count {
        let handler = Arc::new(MultiClientEcho {
            client: OnceLock::new(),
            ping,
            frames: args.frames,
            replies: std::sync::atomic::AtomicU32::new(0),
            events: tx.clone(),
        });
        let dyn_handler: Arc<dyn MultiClientHandler> = handler.clone();
        let client = Arc::new(
            MultiClient::connect(name, dyn_handler, options.clone())
                .map_err(|e| format!("connect: {}", err_name(&e)))?,
        );
        drop(handler.client.set(Arc::downgrade(&client)));
        clients.push(client);
    }
    let want = if ping {
        args.count
    } else {
        args.ends.unwrap_or(args.count)
    };
    let mut seen = 0;
    while seen < want {
        match rx.recv().map_err(|_| "handler gone".to_owned())? {
            MultiEvent::Pinged if ping => seen += 1,
            MultiEvent::Ended if !ping => seen += 1,
            MultiEvent::Ended => return Err("disconnected before the echo".to_owned()),
            MultiEvent::Pinged | MultiEvent::Message(_) => {}
        }
    }
    if ping {
        out(&format!("INFO pinged={}", args.count));
        if args.release.is_empty() {
            thread::sleep(Duration::from_millis(args.hold));
        } else {
            // Сессии держатся, пока оркестратор не взведёт маяк: без
            // зависимости от времени.
            let beacon =
                Beacon::open(&args.release).map_err(|e| format!("release: {}", err_name(&e)))?;
            match beacon.wait(Some(SELF_TIMEOUT)) {
                Ok(true) => {}
                Ok(false) => return Err("release: timeout".to_owned()),
                Err(e) => return Err(format!("release: {}", err_name(&e))),
            }
        }
    }
    for client in &clients {
        client.stop();
    }
    Ok(())
}

// ============================================================================
// Сценарии Dispatch (DispatchServer / DispatchClient)
// ============================================================================

/// Обработчик `DispatchServer`: печать подключений, отключений и ошибок,
/// эхо XOR `0x5A` на кадры клиента.
struct DispatchEcho {
    server: OnceLock<Weak<DispatchServer>>,
    max: usize,
    ended: mpsc::Sender<()>,
}

impl DispatchHandler for DispatchEcho {
    fn on_client_connect(&self, client_id: u32, info: &ClientRegistration) {
        out(&format!(
            "INFO connect={client_id}:{}:{}",
            info.name, info.pid
        ));
    }

    fn on_client_disconnect(&self, client_id: u32) {
        self.on_client_disconnect_reason(client_id, DisconnectReason::Graceful);
    }

    fn on_client_disconnect_reason(&self, client_id: u32, reason: DisconnectReason) {
        out(&format!(
            "INFO disconnect={client_id}:{}",
            reason_name(reason)
        ));
        let _ = self.ended.send(());
    }

    fn on_message(&self, client_id: u32, data: &[u8]) {
        if let Some(server) = self.server.get().and_then(Weak::upgrade)
            && let Err(e) = server.send_to(client_id, &xor_reply(data, self.max))
        {
            out(&format!("INFO err={}", err_name(&e)));
        }
    }

    fn on_error(&self, _client_id: Option<u32>, err: ShmError) {
        out(&format!("INFO err={}", err_name(&err)));
    }
}

/// Что сервер Dispatch делает с маяком `<лобби>_beacon` (сверх маяка
/// присутствия, который `DispatchServer` держит сам).
#[derive(Clone, Copy, PartialEq, Eq)]
enum BeaconRole {
    /// Явно не трогает (`dispatch-server`).
    None,
    /// Явный `raise` после старта, `lower` перед остановкой (`beacon-server`).
    Raise,
    /// Явный `raise`, затем выход с кодом 9 без `lower` и остановки
    /// (`beacon-server-crash`).
    Crash,
}

/// `DispatchServer` с лобби по умолчанию (16 КиБ) и `max_clients`; эхо до
/// `ends` отключений клиентов (по умолчанию 1). `beacon` -- явный маяк
/// лобби (`BeaconRole`).
fn dispatch_server(name: &str, args: &Args, beacon: BeaconRole) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    let options = DispatchOptions {
        max_clients: args.max_clients,
        ..DispatchOptions::default()
    };
    let handler = Arc::new(DispatchEcho {
        server: OnceLock::new(),
        max: options.channel_layout.server_to_client.max_message_size,
        ended: tx,
    });
    let dyn_handler: Arc<dyn DispatchHandler> = handler.clone();
    let server = DispatchServer::start(name, dyn_handler, options)
        .map_err(|e| format!("start: {}", err_name(&e)))?;
    drop(handler.server.set(Arc::downgrade(&server)));
    // Маяк -- строго после того, как лобби создано (§b.3).
    let lobby_beacon = match beacon {
        BeaconRole::None => None,
        BeaconRole::Raise | BeaconRole::Crash => {
            let b = Beacon::open(name).map_err(|e| format!("beacon: {}", err_name(&e)))?;
            b.raise().map_err(|e| format!("raise: {}", err_name(&e)))?;
            Some(b)
        }
    };
    out("READY");
    if beacon == BeaconRole::Crash {
        // «Упал» взведённым: без `lower`, без деструкторов.
        std::process::exit(9);
    }
    for _ in 0..args.ends.unwrap_or(1) {
        rx.recv().map_err(|_| "handler gone".to_owned())?;
    }
    // Сброс -- до остановки лобби.
    if let Some(b) = &lobby_beacon {
        b.lower().map_err(|e| format!("lower: {}", err_name(&e)))?;
    }
    server.stop();
    Ok(())
}

/// Клиент маяка: открыть маяк `name` (`INFO waiting=1`), ждать взвода (20 с),
/// затем `INFO lobby=<0|1>` (`DispatchClient::lobby_exists`), `INFO
/// cleared=<true|false>` (`Beacon::clear_if_stale`) и `INFO raised=<0|1>`.
fn beacon_client(name: &str) -> Result<(), String> {
    let beacon = Beacon::open(name).map_err(|e| format!("beacon: {}", err_name(&e)))?;
    out("INFO waiting=1");
    if !beacon
        .wait(Some(BEACON_WAIT))
        .map_err(|e| format!("wait: {}", err_name(&e)))?
    {
        return Err("beacon not raised".to_owned());
    }
    out(&format!(
        "INFO lobby={}",
        u8::from(DispatchClient::lobby_exists(name))
    ));
    let cleared = beacon
        .clear_if_stale()
        .map_err(|e| format!("clear_if_stale: {}", err_name(&e)))?;
    out(&format!("INFO cleared={cleared}"));
    let raised = beacon
        .is_raised()
        .map_err(|e| format!("is_raised: {}", err_name(&e)))?;
    out(&format!("INFO raised={}", u8::from(raised)));
    Ok(())
}

/// Событие клиента Dispatch для `main`.
enum DispatchEvent {
    /// Экземпляр `i` поднял свой канал.
    Connected(usize),
    /// Экземпляр получил эхо всех своих кадров (`ping`).
    Pinged,
    /// Канал экземпляра разорван.
    Ended,
}

/// Клиент эха (`echo`): пока `connect` не вернул ручку клиента, worker
/// канала уже может доставить первые кадры сервера -- они ждут в очереди и
/// отвечаются при `attach`, а не теряются.
#[derive(Default)]
struct EchoTarget {
    client: Option<Weak<DispatchClient>>,
    early: Vec<Vec<u8>>,
}

/// Ответ XOR `0x5A` клиенту `client`; ошибка -- `INFO err=`.
fn echo_back(client: &Weak<DispatchClient>, data: &[u8]) {
    if let Some(client) = client.upgrade()
        && let Err(e) = client.send(&xor_reply(data, data.len()))
    {
        out(&format!("INFO err={}", err_name(&e)));
    }
}

/// Обработчик одного `DispatchClient`: `echo` -- ответ XOR на кадры
/// сервера; `ping` -- сверка эха своих кадров.
struct DispatchClientEcho {
    index: usize,
    client: Mutex<EchoTarget>,
    ping: bool,
    frames: u32,
    replies: std::sync::atomic::AtomicU32,
    events: mpsc::Sender<DispatchEvent>,
}

impl DispatchClientHandler for DispatchClientEcho {
    fn on_connect(&self, client_id: u32, channel_name: &str) {
        out(&format!("INFO channel={client_id}:{channel_name}"));
        let _ = self.events.send(DispatchEvent::Connected(self.index));
    }

    fn on_disconnect(&self) {}

    fn on_disconnect_reason(&self, reason: DisconnectReason) {
        out(&format!("INFO reason={}", reason_name(reason)));
        let _ = self.events.send(DispatchEvent::Ended);
    }

    fn on_message(&self, data: &[u8]) {
        if self.ping {
            let k = self
                .replies
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            let want: Vec<u8> = payload::frame(k, PING_LEN)
                .iter()
                .map(|b| b ^ 0x5A)
                .collect();
            if data != want.as_slice() {
                out(&format!("INFO err=echo{k}"));
            } else if k + 1 == self.frames {
                let _ = self.events.send(DispatchEvent::Pinged);
            }
            return;
        }
        let mut target = self.client.lock().unwrap_or_else(PoisonError::into_inner);
        match &target.client {
            Some(client) => echo_back(client, data),
            None => target.early.push(data.to_vec()),
        }
    }

    fn on_error(&self, err: ShmError) {
        out(&format!("INFO err={}", err_name(&err)));
    }
}

impl DispatchClientEcho {
    /// Ручка клиента известна: ответить на кадры, пришедшие раньше неё (под
    /// тем же замком, что и `on_message` -- ни один кадр не проскочит).
    fn attach(&self, client: &Arc<DispatchClient>) {
        let mut target = self.client.lock().unwrap_or_else(PoisonError::into_inner);
        let weak = Arc::downgrade(client);
        for data in core::mem::take(&mut target.early) {
            echo_back(&weak, &data);
        }
        target.client = Some(weak);
    }
}

/// Как регистрируется экземпляр клиента Dispatch.
#[derive(Clone)]
struct DispatchPlan {
    cname: String,
    frames: u32,
    ping: bool,
}

/// Один `DispatchClient` (регистрация `ClientRegistration` своего PID).
fn dispatch_connect(
    lobby: &str,
    plan: DispatchPlan,
    index: usize,
    events: mpsc::Sender<DispatchEvent>,
) -> Result<Arc<DispatchClient>, ShmError> {
    let handler = Arc::new(DispatchClientEcho {
        index,
        client: Mutex::new(EchoTarget::default()),
        ping: plan.ping,
        frames: plan.frames,
        replies: std::sync::atomic::AtomicU32::new(0),
        events,
    });
    let dyn_handler: Arc<dyn DispatchClientHandler> = handler.clone();
    let registration = ClientRegistration::new(std::process::id(), 1, plan.cname);
    let client = Arc::new(DispatchClient::connect(
        lobby,
        registration,
        dyn_handler,
        DispatchClientOptions::default(),
    )?);
    handler.attach(&client);
    Ok(client)
}

/// `count` экземпляров `DispatchClient`, регистрирующихся одновременно (по
/// потоку на экземпляр). `ping` (по умолчанию): `frames` кадров длины 16 от
/// каждого после подъёма канала и их эхо (`INFO pinged=<k>`), затем уход;
/// `echo`: ответ XOR на кадры сервера до `ends` разрывов. `expect=<вариант>`
/// -- регистрация обязана провалиться этой ошибкой (`INFO err=`; отказ
/// сервера -- `HandshakeFailed`).
fn dispatch_client(lobby: &str, args: &Args) -> Result<(), String> {
    let ping = match args.mode.as_str() {
        "" | "ping" => true,
        "echo" => false,
        other => return Err(format!("mode {other}")),
    };
    let plan = DispatchPlan {
        cname: args.cname.clone(),
        frames: args.frames,
        ping,
    };
    let (tx, rx) = mpsc::channel();
    if !args.expect.is_empty() {
        return match dispatch_connect(lobby, plan, 0, tx) {
            Ok(_) => Err("registered".to_owned()),
            Err(e) => {
                out(&format!("INFO err={}", err_name(&e)));
                if err_name(&e) == args.expect {
                    Ok(())
                } else {
                    Err(format!("expected {}", args.expect))
                }
            }
        };
    }
    let count = args.count as usize;
    let threads: Vec<_> = (0..count)
        .map(|index| {
            let (lobby, plan, events) = (lobby.to_owned(), plan.clone(), tx.clone());
            thread::spawn(move || dispatch_connect(&lobby, plan, index, events))
        })
        .collect();
    drop(tx);
    let mut clients = Vec::with_capacity(count);
    for handle in threads {
        let client = handle
            .join()
            .map_err(|_| "client thread panicked".to_owned())?
            .map_err(|e| format!("connect: {}", err_name(&e)))?;
        clients.push(client);
    }
    let want = if ping {
        count
    } else {
        args.ends.map_or(count, |e| e as usize)
    };
    let (mut connected, mut seen) = (0, 0);
    while connected < count || seen < want {
        match rx.recv().map_err(|_| "handler gone".to_owned())? {
            DispatchEvent::Connected(i) => {
                connected += 1;
                if ping && let Some(client) = clients.get(i) {
                    for k in 0..args.frames {
                        client
                            .send(&payload::frame(k, PING_LEN))
                            .map_err(|e| format!("ping {k}: {}", err_name(&e)))?;
                    }
                }
            }
            DispatchEvent::Pinged if ping => seen += 1,
            DispatchEvent::Ended if !ping => seen += 1,
            DispatchEvent::Ended => return Err("disconnected before the echo".to_owned()),
            DispatchEvent::Pinged => {}
        }
    }
    if ping {
        out(&format!("INFO pinged={count}"));
    }
    for client in &clients {
        client.stop();
    }
    Ok(())
}

// ============================================================================
// Сценарии Auto (AutoServer / AutoClient)
// ============================================================================

/// Конечная точка Auto для эхо-ответов из колбэка.
enum AutoEndpoint {
    Server(Weak<AutoServer>),
    Client(Weak<AutoClient>),
}

/// Обработчик Auto-эхо: печатает подключение, ошибки и причины, отвечает
/// XOR `0x5A`; первая причина отключения уходит в `main`.
struct EchoHandler {
    endpoint: OnceLock<AutoEndpoint>,
    connected: mpsc::Sender<()>,
    reasons: mpsc::Sender<DisconnectReason>,
}

impl AutoHandler for EchoHandler {
    fn on_connect(&self) {
        out("INFO connected=1");
        let _ = self.connected.send(());
    }

    fn on_disconnect_reason(&self, reason: DisconnectReason) {
        out(&format!("INFO reason={}", reason_name(reason)));
        let _ = self.reasons.send(reason);
    }

    fn on_message(&self, _direction: ChannelKind, payload: &[u8]) {
        let (sent, max) = match self.endpoint.get() {
            Some(AutoEndpoint::Server(s)) => match s.upgrade() {
                Some(s) => {
                    let max = s.layout().server_to_client.max_message_size;
                    let reply = xor_reply(payload, max);
                    (s.send(&reply), max)
                }
                None => return,
            },
            Some(AutoEndpoint::Client(c)) => match c.upgrade() {
                Some(c) => {
                    let max = c
                        .layout()
                        .map_or(payload.len(), |l| l.client_to_server.max_message_size);
                    let reply = xor_reply(payload, max);
                    (c.send(&reply), max)
                }
                None => return,
            },
            None => return,
        };
        if let Err(e) = sent {
            out(&format!("INFO err={} max={max}", err_name(&e)));
        }
    }

    fn on_error(&self, err: ShmError) {
        out(&format!("INFO err={}", err_name(&err)));
    }
}

/// Тот же обработчик как `Arc<dyn AutoHandler>`.
fn dyn_handler(handler: &Arc<EchoHandler>) -> Arc<dyn AutoHandler> {
    let handler: Arc<EchoHandler> = Arc::clone(handler);
    handler
}

fn xor_reply(payload: &[u8], max: usize) -> Vec<u8> {
    payload.iter().take(max).map(|b| b ^ 0x5A).collect()
}

/// Auto-эхо до первого отключения; `expect=<причина>` -- его проверка.
fn auto_echo(name: &str, args: &Args, server: bool) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    let (connected_tx, connected_rx) = mpsc::channel();
    let handler = Arc::new(EchoHandler {
        endpoint: OnceLock::new(),
        connected: connected_tx,
        reasons: tx,
    });
    let options = AutoOptions {
        layout: args.layout,
        ..AutoOptions::default()
    };
    let stop: Box<dyn Fn()> = if server {
        let s = Arc::new(
            AutoServer::start(name, dyn_handler(&handler), options)
                .map_err(|e| format!("start: {}", err_name(&e)))?,
        );
        drop(
            handler
                .endpoint
                .set(AutoEndpoint::Server(Arc::downgrade(&s))),
        );
        out("READY");
        Box::new(move || s.stop())
    } else {
        let c = Arc::new(
            AutoClient::connect(name, dyn_handler(&handler), options)
                .map_err(|e| format!("connect: {}", err_name(&e)))?,
        );
        drop(
            handler
                .endpoint
                .set(AutoEndpoint::Client(Arc::downgrade(&c))),
        );
        // `READY` клиента -- подключён и уже отвечает (конечная точка эха
        // известна обработчику до первого кадра пира).
        if connected_rx.recv().is_ok() {
            out("READY");
        }
        Box::new(move || c.stop())
    };
    let reason = rx.recv().map_err(|_| "handler gone".to_owned());
    stop();
    let reason = reason?;
    if !args.expect.is_empty() && reason_name(reason) != args.expect {
        return Err(format!("reason {} != {}", reason_name(reason), args.expect));
    }
    Ok(())
}

// ============================================================================
// main
// ============================================================================

fn run(scenario: &str, name: &str, args: &Args) -> Result<(), String> {
    match scenario {
        "server-echo" => echo(&mut Side::serve(name, args.layout)?, args),
        "client-echo" => echo(&mut Side::connect_with(name, args.busy, None)?, args),
        "server-lossless-sink" => lossless_sink(&mut Side::serve(name, args.layout)?, args),
        "client-lossless-sink" => lossless_sink(&mut Side::connect(name)?, args),
        "server-lossless-source" => lossless_source(&mut Side::serve(name, args.layout)?, args),
        "client-lossless-source" => lossless_source(&mut Side::connect(name)?, args),
        "server-farewell" => farewell(Side::serve(name, args.layout)?, args),
        "client-farewell" => farewell(Side::connect(name)?, args),
        "server-die" => die(&Side::serve(name, args.layout)?),
        "client-die" => die(&Side::connect(name)?),
        "server-expect-death" => expect_death(&mut Side::serve(name, args.layout)?),
        "client-expect-death" => client_expect_death(name),
        "server-hold" => hold(&mut Side::serve(name, args.layout)?),
        "client-hold" => hold(&mut Side::connect(name)?),
        "client-expect-busy" => client_expect_err(name, "AlreadyConnected"),
        "client-expect-err" => client_expect_err(name, &args.expect),
        "auto-server-echo" => auto_echo(name, args, true),
        "auto-client-echo" => auto_echo(name, args, false),
        "server-sessions" => server_sessions(name, args),
        "server-stall-leave" => stall_leave(Side::serve(name, args.layout)?, args),
        "client-stall-leave" => stall_leave(Side::connect(name)?, args),
        "server-fill-wait" => fill_wait(&Side::serve(name, args.layout)?, args),
        "client-fill-wait" => fill_wait(&Side::connect(name)?, args),
        "client-hello-die" => client_hello_die(name),
        "client-ping" => client_ping(name, args),
        "multi-server" => multi_server(name, args, None),
        "ns-server" => echo(
            &mut Side::serve_in(name, args.layout, Some(namespace(args)?))?,
            args,
        ),
        "ns-client" => echo(
            &mut Side::connect_with(name, false, Some(&*namespace(args)?))?,
            args,
        ),
        "ns-multi-server" => multi_server(name, args, Some(namespace(args)?)),
        "multi-client" => multi_client(name, args),
        "dispatch-server" => dispatch_server(name, args, BeaconRole::None),
        "beacon-server" => dispatch_server(name, args, BeaconRole::Raise),
        "beacon-server-crash" => dispatch_server(name, args, BeaconRole::Crash),
        "beacon-client" => beacon_client(name),
        "dispatch-client" => dispatch_client(name, args),
        other => Err(format!("unknown scenario {other}")),
    }
}

fn main() -> ExitCode {
    // Самотаймаут: зависший сценарий (нет прогресса `SELF_TIMEOUT` или прошло
    // `HARD_TIMEOUT`) не держит оркестратор. Медленный, но живой обмен
    // (CPU отдан другим процессам) -- не зависание.
    thread::spawn(|| {
        let started = Instant::now();
        let mut seen = PROGRESS.load(std::sync::atomic::Ordering::Relaxed);
        let mut last = started;
        loop {
            // Сверка раз в секунду: зависание замечается через
            // `SELF_TIMEOUT` после последнего прогресса, а не через окно
            // до `2 * SELF_TIMEOUT`.
            thread::sleep(WATCHDOG_STEP);
            let now = PROGRESS.load(std::sync::atomic::Ordering::Relaxed);
            if now != seen {
                seen = now;
                last = Instant::now();
            }
            if last.elapsed() >= SELF_TIMEOUT || started.elapsed() >= HARD_TIMEOUT {
                out("FAIL timeout");
                std::process::exit(1);
            }
        }
    });
    // Аргументы -- UTF-16 командной строки Windows (имя канала может быть
    // не-ASCII); невалидный UTF-16 -- ошибка аргументов.
    let argv: Option<Vec<String>> = std::env::args_os()
        .skip(1)
        .map(|a| a.into_string().ok())
        .collect();
    let Some(argv) = argv else {
        out("FAIL usage: arguments are not valid Unicode");
        return ExitCode::from(2);
    };
    let [scenario, name, rest @ ..] = argv.as_slice() else {
        out("FAIL usage: interop_peer <scenario> <name> [key=value ...]");
        return ExitCode::from(2);
    };
    let args = match parse_args(rest) {
        Ok(args) => args,
        Err(e) => {
            out(&format!("FAIL usage: {e}"));
            return ExitCode::from(2);
        }
    };
    match run(scenario, name, &args) {
        Ok(()) => {
            out("PASS");
            ExitCode::SUCCESS
        }
        Err(e) => {
            out(&format!("FAIL {e}"));
            ExitCode::from(1)
        }
    }
}
