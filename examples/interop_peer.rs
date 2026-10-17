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
//!   жёсткий самотаймаут 60 с -- `FAIL timeout`.
//!
//! Ключи: `s2c=<ёмкость>:<сообщений>:<макс. размер>`, `c2s=...` (раскладка
//! сервера; по умолчанию `RingConfig::DEFAULT`), `n=<кадров>` (3000),
//! `size=<байт>` (макс. длина кадра генератора / длина кадра lossless:
//! 65 535 / 30 720), `seed=<u32>` (1), `mode=send|recv` (farewell),
//! `expect=<вариант>` (ожидаемая ошибка / причина).
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
//! - `*-die` -- 2 кадра длины 7, `INFO sent=2`, `abort()` без ухода;
//!   `*-expect-death` -- 2 кадра от пира и `reason=PeerDied`;
//! - `*-hold` -- держать сессию до ухода пира;
//! - `client-expect-busy` -- подключение обязано дать `AlreadyConnected`;
//!   `client-expect-err expect=<вариант>` -- ошибку `expect`;
//! - `auto-server-echo`, `auto-client-echo` -- `AutoServer`/`AutoClient`,
//!   эхо в `on_message`, печать `on_error`/`on_disconnect_reason`; конец --
//!   первое отключение (`expect=<причина>` -- его проверка).
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
use std::sync::{Arc, OnceLock, Weak};
use std::task::{Context, Poll, Wake, Waker};
use std::thread;
use std::time::{Duration, Instant};

use xshm::{
    AutoClient, AutoHandler, AutoOptions, AutoServer, ChannelConfig, ChannelKind, ChannelLayout,
    DisconnectReason, RingConfig, SharedClient, SharedServer, ShmError,
};

/// Жёсткий самотаймаут пира: оркестратор не должен ждать зависший пир.
const SELF_TIMEOUT: Duration = Duration::from_secs(60);

/// Сколько клиент ждёт ответа сервера на одну попытку подключения.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Сколько клиент повторяет подключение к ещё не созданному каналу.
const CONNECT_RETRY_FOR: Duration = Duration::from_secs(10);

/// Строка протокола в stdout. Ошибку записи (оркестратор закрыл канал)
/// некому сообщить -- пир всё равно завершится по сценарию или самотаймауту.
fn out(line: &str) {
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
            _ => return Err(format!("unknown key {key}")),
        }
    }
    Ok(args)
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
        let mut server = SharedServer::start_with(name, &ChannelConfig::new(layout))
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
        let started = Instant::now();
        let client = loop {
            match SharedClient::connect(name, CONNECT_TIMEOUT) {
                Ok(client) => break client,
                Err(ShmError::WindowsError { .. } | ShmError::NotReady)
                    if started.elapsed() < CONNECT_RETRY_FOR =>
                {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) => return Err(format!("connect: {}", err_name(&e))),
            }
        };
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
            // процесса -- `PeerDied`; обе -- после дочитанного кольца.
            ShmError::NotConnected => Ended::Gone(DisconnectReason::Graceful),
            ShmError::PeerDied => Ended::Gone(DisconnectReason::PeerDied),
            other => Ended::Failed(other),
        })
    }

    /// Запись с перезаписью (эхо: в кольце не больше одного кадра).
    fn send(&self, data: &[u8]) -> Result<(), ShmError> {
        match self {
            Self::Server(s) => s.send_to_client(data).map(drop),
            Self::Client(c) => c.send_to_server(data).map(drop),
        }
    }

    /// Запись без потерь: `try_send_*`, нет места -- `wait_for_space`.
    fn send_lossless(&self, data: &[u8]) -> Result<(), ShmError> {
        loop {
            let tried = match self {
                Self::Server(s) => s.try_send_to_client(data).map(drop),
                Self::Client(c) => c.try_send_to_server(data).map(drop),
            };
            match tried {
                Err(ShmError::QueueFull) => {}
                other => return other,
            }
            let waited = match self {
                Self::Server(s) => s.wait_for_space(data.len(), None),
                Self::Client(c) => c.wait_for_space(data.len(), None),
            };
            waited?;
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
    for k in 0..args.n {
        side.send_lossless(&payload::frame(k, size))
            .map_err(|e| format!("send {k}: {}", err_name(&e)))?;
    }
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

/// Смерть: 2 кадра и аварийное завершение процесса (без ухода).
fn die(side: &Side) -> Result<(), String> {
    for k in 0..2 {
        side.send(&payload::frame(k, 7))
            .map_err(|e| format!("send {k}: {}", err_name(&e)))?;
    }
    out("INFO sent=2");
    std::process::abort();
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
        "client-echo" => echo(&mut Side::connect(name)?, args),
        "server-lossless-sink" => lossless_sink(&mut Side::serve(name, args.layout)?, args),
        "client-lossless-sink" => lossless_sink(&mut Side::connect(name)?, args),
        "server-lossless-source" => lossless_source(&mut Side::serve(name, args.layout)?, args),
        "client-lossless-source" => lossless_source(&mut Side::connect(name)?, args),
        "server-farewell" => farewell(Side::serve(name, args.layout)?, args),
        "client-farewell" => farewell(Side::connect(name)?, args),
        "server-die" => die(&Side::serve(name, args.layout)?),
        "client-die" => die(&Side::connect(name)?),
        "server-expect-death" => expect_death(&mut Side::serve(name, args.layout)?),
        "client-expect-death" => expect_death(&mut Side::connect(name)?),
        "server-hold" => hold(&mut Side::serve(name, args.layout)?),
        "client-hold" => hold(&mut Side::connect(name)?),
        "client-expect-busy" => client_expect_err(name, "AlreadyConnected"),
        "client-expect-err" => client_expect_err(name, &args.expect),
        "auto-server-echo" => auto_echo(name, args, true),
        "auto-client-echo" => auto_echo(name, args, false),
        other => Err(format!("unknown scenario {other}")),
    }
}

fn main() -> ExitCode {
    // Самотаймаут: зависший сценарий не держит оркестратор.
    thread::spawn(|| {
        thread::sleep(SELF_TIMEOUT);
        out("FAIL timeout");
        std::process::exit(1);
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
