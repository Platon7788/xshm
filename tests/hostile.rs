//! Враждебная память и шторм рукопожатий (Task 18).
//!
//! - `random_corruption_never_panics_or_hangs` -- пир-«вандал»: в случайное
//!   слово `ControlBlock`/`LayoutBlock`/`RingHeader`/данных кольца пишется
//!   случайное значение, затем -- случайная операция API. Ни паники, ни
//!   операции дольше бюджета, ни ошибки вне перечня. Single-client и слот
//!   Multi (поля 0.10: времена старта, `slot_count`, PAD-кадры,
//!   `LayoutBlock`).
//! - `handshake_storm` -- 8 клиентов × 200 подключений к одному `AutoServer`:
//!   ревизия 3 -- каждое принятие клиентом ровно одна сессия сервера, занятый
//!   канал -- `AlreadyConnected`, причины отключений -- только `Graceful`;
//!   `handshake_storm_with_retry` -- повтор при занятом канале до 200 настоящих
//!   рукопожатий на клиента (1600 сессий).
//! - `overwrite_flood_*` -- 10⁶ малых сообщений с перезаписью: ни одного
//!   потерянного `DATA`, порядок и сходимость счётчиков (`drop_count`).
//!
//! Разделяемую память тест видит вторым отображением той же секции
//! (`OpenFileMappingW` по имени `Local\<канал>`), как её видел бы другой
//! процесс. Генератор -- xorshift64* с фиксированным seed; seed и номер
//! итерации -- в каждом сообщении об ошибке.
//!
//! Фаза Multi портит память параллельно с потоками библиотеки (атомарные
//! записи теста против `copy_nonoverlapping` payload-а): это модель
//! межпроцессного пира, а для модели памяти Rust -- гонка данных. Miri и
//! санитайзеры к этому тесту неприменимы.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use xshm::{
    AutoClient, AutoHandler, AutoOptions, AutoServer, ChannelConfig, ChannelKind, ChannelLayout,
    DisconnectReason, MultiClient, MultiClientHandler, MultiClientOptions, MultiHandler,
    MultiOptions, MultiServer, RingConfig, SharedClient, SharedServer, ShmError,
};

// ─── второе отображение секции ──────────────────────────────────────────────

#[cfg_attr(
    target_arch = "x86",
    link(
        name = "kernel32",
        kind = "raw-dylib",
        import_name_type = "undecorated"
    )
)]
#[cfg_attr(not(target_arch = "x86"), link(name = "kernel32", kind = "raw-dylib"))]
unsafe extern "system" {
    fn OpenFileMappingW(desired_access: u32, inherit: i32, name: *const u16) -> *mut c_void;
    fn CreateFileMappingW(
        file: *mut c_void,
        attributes: *const c_void,
        protect: u32,
        size_high: u32,
        size_low: u32,
        name: *const u16,
    ) -> *mut c_void;
    fn MapViewOfFile(
        mapping: *mut c_void,
        desired_access: u32,
        offset_high: u32,
        offset_low: u32,
        bytes: usize,
    ) -> *mut c_void;
    fn UnmapViewOfFile(base: *const c_void) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn OpenEventW(desired_access: u32, inherit: i32, name: *const u16) -> *mut c_void;
    fn SetEvent(event: *mut c_void) -> i32;
}

/// `FILE_MAP_ALL_ACCESS`.
const FILE_MAP_ALL_ACCESS: u32 = 0x000F_001F;

/// Отображение сегмента канала «глазами пира».
struct Segment {
    handle: NonNull<c_void>,
    base: NonNull<u8>,
    size: usize,
}

impl Segment {
    /// Открыть секцию канала `name` (без явного namespace -- `Local\`) и
    /// отобразить первые `size` байт.
    fn open(name: &str, size: usize) -> Self {
        let wide: Vec<u16> = format!("Local\\{name}")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        // SAFETY: `wide` -- строка с нулём на конце, живёт весь вызов.
        let handle = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, wide.as_ptr()) };
        let handle = NonNull::new(handle).unwrap_or_else(|| panic!("OpenFileMappingW({name})"));
        // SAFETY: `handle` -- только что открытая секция; размер не больше
        // секции (его задал сервер этого теста).
        let base = unsafe { MapViewOfFile(handle.as_ptr(), FILE_MAP_ALL_ACCESS, 0, 0, size) };
        let base = NonNull::new(base.cast::<u8>()).unwrap_or_else(|| panic!("MapViewOfFile"));
        Self { handle, base, size }
    }

    /// Атомарное слово по смещению `offset` (кратно 4, внутри сегмента).
    fn word(&self, offset: usize) -> &AtomicU32 {
        assert!(
            offset.is_multiple_of(4) && offset + 4 <= self.size,
            "{offset}"
        );
        // SAFETY: offset + 4 <= size -- адрес внутри отображения.
        let ptr = unsafe { self.base.as_ptr().add(offset) };
        // SAFETY: адрес выровнен на 4 (база -- на гранулу выделения), память
        // отображена, пока жив `self`; другие стороны обращаются к словам
        // заголовков атомарно -- «чужой процесс» здесь тоже пишет атомарно.
        unsafe { AtomicU32::from_ptr(ptr.cast()) }
    }

    fn load(&self, offset: usize) -> u32 {
        self.word(offset).load(Ordering::Acquire)
    }

    fn store(&self, offset: usize, value: u32) {
        self.word(offset).store(value, Ordering::Release);
    }
}

impl Drop for Segment {
    fn drop(&mut self) {
        // SAFETY: база получена от `MapViewOfFile` и ещё не размаплена.
        let _ = unsafe { UnmapViewOfFile(self.base.as_ptr().cast_const().cast()) };
        // SAFETY: handle открыт `OpenFileMappingW` и закрывается один раз.
        let _ = unsafe { CloseHandle(self.handle.as_ptr()) };
    }
}

// ─── раскладка тестового канала ─────────────────────────────────────────────

/// Ёмкость колец: маленькая, чтобы PAD-кадры и переполнение шли постоянно.
const CAP: usize = 4096;
/// Лимит сообщений: и по счётчику, и по байтам кольцо упирается часто.
const MAX_MESSAGES: u32 = 16;
/// `max_payload_for(4096)` = 4096 / 2 - 8.
const MAX_PAYLOAD: usize = CAP / 2 - 8;
const RING_HEADER: usize = 64;
const CONTROL: usize = 0;
const LAYOUT: usize = 64;
const S2C_HEADER: usize = 128;
const S2C_DATA: usize = S2C_HEADER + RING_HEADER;
const C2S_HEADER: usize = S2C_DATA + CAP;
const C2S_DATA: usize = C2S_HEADER + RING_HEADER;
const SEGMENT: usize = C2S_DATA + CAP;
/// Смещения полей `RingHeader`.
const WRITE_POS: usize = 0;
const READ_POS: usize = 4;
/// Флаг PAD-кадра (`flags` бит 0, INTEROP §c.4).
const FRAME_PAD: u32 = 1;

const fn layout() -> ChannelLayout {
    ChannelLayout::symmetric(RingConfig::new(CAP).max_messages(MAX_MESSAGES))
}

fn unique(tag: &str) -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    format!(
        "XSHM_HOSTILE_{tag}_{}_{}",
        std::process::id(),
        // Relaxed: уникальность имени, ничего не публикует.
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

// ─── генератор и учёт паник ─────────────────────────────────────────────────

/// xorshift64* (Виґна): без зависимостей, детерминированный.
struct Rng(u64);

impl Rng {
    const fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    const fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// `usize` в `0..n` (`n` -- небольшие длины и индексы).
    fn index(&mut self, n: usize) -> usize {
        usize::try_from(self.below(n as u64)).unwrap_or(0)
    }

    const fn word(&mut self) -> u32 {
        (self.next() >> 32) as u32
    }
}

/// Паники любого потока процесса (worker-ы, диспетчер, колбэки) --
/// счётчик поверх штатного хука.
static PANICS: AtomicU32 = AtomicU32::new(0);

fn count_panics() {
    static HOOK: OnceLock<()> = OnceLock::new();
    HOOK.get_or_init(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            PANICS.fetch_add(1, Ordering::AcqRel);
            previous(info);
        }));
    });
}

/// Граница одной операции: бюджет брифа 100 мс плюс запас на шум хоста.
/// Зависание, которое ищет тест, -- секунды и больше (или навсегда).
const OP_BUDGET: Duration = Duration::from_millis(100);
const OP_LIMIT: Duration = Duration::from_millis(250);

/// Контекст воспроизведения: seed, фаза, итерация, последняя порча.
#[derive(Clone, Copy, Debug)]
struct Case {
    #[expect(dead_code, reason = "печатается в сообщениях об ошибке через Debug")]
    seed: u64,
    phase: &'static str,
    iteration: u32,
    offset: usize,
    value: u32,
}

/// Выполнить операцию и проверить её время.
fn timed<T>(case: &Case, op: &str, f: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let result = f();
    let elapsed = started.elapsed();
    assert!(
        elapsed <= OP_LIMIT,
        "{op} заняла {elapsed:?} (бюджет {OP_BUDGET:?}, предел {OP_LIMIT:?}); {case:?}"
    );
    result
}

/// Ошибки, допустимые для операций канала после порчи памяти пиром
/// (перечень брифа):
/// - `Corrupted` -- позиции, заголовок кадра, флаги или длина не сходятся;
/// - `QueueFull` -- `try_send*`: места по байтам или по лимиту нет (в том
///   числе «занятость» из испорченных позиций);
/// - `QueueEmpty` -- `receive*`: по позициям пусто;
/// - `NotConnected` -- `wait_for_space`: взведённый `DISCONNECT` при
///   испорченном состоянии пира (`client_state`/`server_state`/
///   `generation` != `SERVER_READY` или сессия сменилась);
/// - `MessageTooLarge` -- длина больше лимита кольца (тест иногда шлёт
///   слишком длинные; лимит берётся из локальной копии раскладки).
///
/// `PeerDied` не допускается: пир -- этот же процесс, наблюдаемый handle
/// процесса не сигналит, а PID/время в сегменте после рукопожатия никто не
/// перечитывает. `Overrun` -- тоже: `receive_with` выполняется в том же
/// потоке, что и порча, перезаписи во время `f` нет. `WindowsError` -- нет:
/// объекты ядра порча памяти не трогает.
const fn channel_error_allowed(err: &ShmError) -> bool {
    matches!(
        err,
        ShmError::Corrupted
            | ShmError::QueueFull
            | ShmError::QueueEmpty
            | ShmError::NotConnected
            | ShmError::MessageTooLarge
    )
}

fn check<T>(case: &Case, op: &str, result: xshm::Result<T>) -> Option<T> {
    match result {
        Ok(value) => {
            tally("Ok");
            Some(value)
        }
        Err(err) => {
            assert!(
                channel_error_allowed(&err),
                "{op}: недопустимая ошибка {err:?}; {case:?}"
            );
            tally(error_kind(&err));
            None
        }
    }
}

/// Сводка исходов операций (для отчёта: какие пути порча реально задела).
static OUTCOMES: Mutex<Vec<(&'static str, u64)>> = Mutex::new(Vec::new());

#[expect(
    clippy::unwrap_used,
    reason = "тестовый помощник: паника -- провал теста"
)]
fn tally(kind: &'static str) {
    let mut outcomes = OUTCOMES.lock().unwrap();
    match outcomes.iter_mut().find(|(k, _)| *k == kind) {
        Some((_, n)) => *n += 1,
        None => outcomes.push((kind, 1)),
    }
}

const fn error_kind(err: &ShmError) -> &'static str {
    match err {
        ShmError::Corrupted => "Corrupted",
        ShmError::QueueFull => "QueueFull",
        ShmError::QueueEmpty => "QueueEmpty",
        ShmError::NotConnected => "NotConnected",
        ShmError::MessageTooLarge => "MessageTooLarge",
        _ => "другое",
    }
}

/// Результат операции не нужен -- только её проверка (`check`).
fn ignore<T>(_: Option<T>) {}

// ─── порча ──────────────────────────────────────────────────────────────────

/// Слово для порчи: заголовки -- целиком, данные кольца -- чаще заголовок
/// кадра в позиции чтения (там его увидит читатель) или в опубликованных
/// данных.
fn pick_offset(rng: &mut Rng, seg: &Segment) -> usize {
    let ring_data = |rng: &mut Rng, header: usize, data: usize| {
        let read = seg.load(header + READ_POS) as usize;
        let write = seg.load(header + WRITE_POS) as usize;
        let index = match rng.below(4) {
            // Заголовок кадра в позиции чтения: len или flags.
            0 | 1 => (read & (CAP - 1) & !7) + 4 * rng.index(2),
            // Внутри опубликованного (если позиции правдоподобны).
            2 => {
                let used = write.wrapping_sub(read).min(CAP);
                (read.wrapping_add(rng.index(used.max(1))) & (CAP - 1)) & !3
            }
            _ => 4 * rng.index(CAP / 4),
        };
        data + index
    };
    let header_words = if rng.below(4) == 0 { 16 } else { 8 };
    match rng.below(100) {
        // ControlBlock: magic, version, generation, состояния, claim, PID,
        // времена старта (0.10), slot_count (0.10).
        0..15 => CONTROL + 4 * rng.index(16),
        // LayoutBlock (0.10): segment_size, flags, дескрипторы колец.
        15..25 => LAYOUT + 4 * rng.index(16),
        // RingHeader: чаще позиции, счётчики и заявка (первые 8 слов).
        25..45 => S2C_HEADER + 4 * rng.index(header_words),
        45..65 => C2S_HEADER + 4 * rng.index(header_words),
        65..82 => ring_data(rng, S2C_HEADER, S2C_DATA),
        _ => ring_data(rng, C2S_HEADER, C2S_DATA),
    }
}

/// Значение порчи: граничные, «почти верные» позиции, PAD, мусор.
const fn pick_value(rng: &mut Rng, old: u32) -> u32 {
    let step = 8 * (1 + rng.word() % 64);
    match rng.below(12) {
        0 => 0,
        1 => u32::MAX,
        2 => old.wrapping_add(step),
        3 => old.wrapping_sub(step),
        4 => old ^ (1 << (rng.word() % 32)),
        5 => FRAME_PAD,
        6 => FRAME_PAD | (1 << (1 + rng.word() % 31)),
        7 => rng.word() % 16,
        8 => rng.word() % 6000,
        // Невыровненная позиция.
        9 => old.wrapping_add(1 + rng.word() % 7),
        10 => 0x8000_0000 | (rng.word() % 64),
        _ => rng.word(),
    }
}

/// Испортить одно слово сегмента; вернуть (смещение, прежнее значение,
/// новое значение).
fn corrupt(rng: &mut Rng, seg: &Segment) -> (usize, u32, u32) {
    let offset = pick_offset(rng, seg);
    let old = seg.load(offset);
    let value = pick_value(rng, old);
    seg.store(offset, value);
    (offset, old, value)
}

fn payload(rng: &mut Rng) -> Vec<u8> {
    let len = match rng.below(16) {
        // Иногда -- около предела и за ним (MessageTooLarge).
        0 => MAX_PAYLOAD - 16 + rng.index(48),
        1..4 => 1 + rng.index(MAX_PAYLOAD),
        _ => 1 + rng.index(300),
    };
    (0..len).map(|_| rng.word() as u8).collect()
}

// ─── single-client ──────────────────────────────────────────────────────────

/// Подключённая пара на канале `name`.
#[expect(
    clippy::unwrap_used,
    reason = "тестовый помощник: паника -- провал теста"
)]
fn connect_pair(name: &str, case: &Case) -> (SharedServer, SharedClient) {
    let config = ChannelConfig::new(layout());
    let mut server = SharedServer::start_with(name, &config).unwrap();
    let waiter = thread::spawn(move || {
        server
            .wait_for_client(Some(Duration::from_secs(10)))
            .map(|()| server)
    });
    let client = SharedClient::connect(name, Duration::from_secs(10))
        .unwrap_or_else(|err| panic!("connect: {err:?}; {case:?}"));
    let server = waiter
        .join()
        .unwrap()
        .unwrap_or_else(|err| panic!("wait_for_client: {err:?}; {case:?}"));
    (server, client)
}

/// Подключение к сегменту, испорченному ДО рукопожатия (магия, версия,
/// раскладка, времена, `slot_count`): сервер не ждёт клиента, поэтому
/// неиспорченное -- `Timeout`. Допустимо (`SharedClient::connect`):
/// `NotReady` (`magic == 0`), `VersionMismatch`, `Corrupted` (чужая магия,
/// раскладка не прошла проверки §c.2), `Timeout` (раскладка цела -- никто
/// не ответил), `AlreadyConnected` -- только при порче `client_state`/
/// `slot_claim` (ревизия 3: канал выглядит занятым).
fn attach_after_corruption(rng: &mut Rng, name: &str, seg: &Segment, case: &mut Case) {
    let offset = if rng.below(2) == 0 {
        CONTROL + 4 * rng.index(16)
    } else {
        LAYOUT + 4 * rng.index(16)
    };
    let old = seg.load(offset);
    let value = pick_value(rng, old);
    seg.store(offset, value);
    (case.offset, case.value) = (offset, value);
    let result = timed(case, "connect после порчи", || {
        SharedClient::connect(name, Duration::from_millis(20))
    });
    // Ревизия 3 (Task 22): `client_state` (@16) не `IDLE` или `slot_claim`
    // (@20) не свободен -- канал выглядит занятым (чужая заявка/сессия или
    // чужой claim): C1/C6 дают `AlreadyConnected`, ничего не записав.
    let occupied = offset == CONTROL + 16 || offset == CONTROL + 20;
    match result {
        Err(
            ShmError::NotReady
            | ShmError::VersionMismatch { .. }
            | ShmError::Corrupted
            | ShmError::Timeout,
        ) => {}
        Err(ShmError::AlreadyConnected) if occupied => {}
        other => panic!("connect после порчи: {:?}; {case:?}", other.map(drop)),
    }
    seg.store(offset, old);
}

/// Одна случайная операция над парой.
fn single_op(
    rng: &mut Rng,
    server: &mut SharedServer,
    client: &mut SharedClient,
    buf: &mut Vec<u8>,
    case: &Case,
) {
    let data = payload(rng);
    let short = Duration::from_millis(2);
    let check_len = |len: usize| {
        assert!((1..=MAX_PAYLOAD).contains(&len), "длина {len}; {case:?}");
    };
    match rng.below(16) {
        0 => ignore(check(
            case,
            "send_to_client",
            timed(case, "send_to_client", || server.send_to_client(&data)),
        )),
        1 => ignore(check(
            case,
            "try_send_to_client",
            timed(case, "try_send_to_client", || {
                server.try_send_to_client(&data)
            }),
        )),
        2 => ignore(check(
            case,
            "send_to_server",
            timed(case, "send_to_server", || client.send_to_server(&data)),
        )),
        3 => ignore(check(
            case,
            "try_send_to_server",
            timed(case, "try_send_to_server", || {
                client.try_send_to_server(&data)
            }),
        )),
        4 | 5 => {
            if let Some(len) = check(
                case,
                "receive_from_client",
                timed(case, "receive_from_client", || {
                    server.receive_from_client(buf)
                }),
            ) {
                check_len(len);
                assert_eq!(buf.len(), len);
            }
        }
        6 | 7 => {
            if let Some(len) = check(
                case,
                "receive_from_server",
                timed(case, "receive_from_server", || {
                    client.receive_from_server(buf)
                }),
            ) {
                check_len(len);
                assert_eq!(buf.len(), len);
            }
        }
        8 => timed(case, "free_space", || {
            let (a, b) = (server.free_space(), client.free_space());
            assert!(a.max_frame <= a.bytes && a.bytes <= CAP, "{a:?}; {case:?}");
            assert!(b.max_frame <= b.bytes && b.bytes <= CAP, "{b:?}; {case:?}");
            let _ = (a.fits(data.len()), b.max_payload());
        }),
        9 => ignore(check(
            case,
            "server wait_for_space",
            timed(case, "server wait_for_space", || {
                server.wait_for_space(data.len(), Some(short))
            }),
        )),
        10 => ignore(check(
            case,
            "client wait_for_space",
            timed(case, "client wait_for_space", || {
                client.wait_for_space(data.len(), Some(short))
            }),
        )),
        11 => ignore(check(
            case,
            "poll_client",
            timed(case, "poll_client", || server.poll_client(Some(short))),
        )),
        12 => ignore(check(
            case,
            "poll_server",
            timed(case, "poll_server", || {
                client.poll_server(Some(Duration::ZERO))
            }),
        )),
        13 => {
            // SAFETY: кадр во время `f` не меняет никто -- порча идёт в этом
            // же потоке и только между операциями.
            let result = timed(case, "receive_with", || unsafe {
                server.receive_with(<[u8]>::len)
            });
            if let Some(len) = check(case, "receive_with", result) {
                check_len(len);
            }
        }
        14 => {
            // SAFETY: см. выше.
            let result = timed(case, "receive_with", || unsafe {
                client.receive_with(<[u8]>::len)
            });
            if let Some(len) = check(case, "receive_with", result) {
                check_len(len);
            }
        }
        _ => {
            let len = data.len();
            let result = timed(case, "send_with", || {
                server.send_with(len, |slot| slot.copy_from_slice(&data))
            });
            ignore(check(case, "send_with", result));
        }
    }
}

/// Эпоха single-client: свежий канал, `iterations` пар «порча + операция».
#[expect(
    clippy::unwrap_used,
    reason = "тестовый помощник: паника -- провал теста"
)]
fn single_epoch(rng: &mut Rng, case: &mut Case, iterations: u32) {
    let name = unique("SINGLE");
    if rng.below(4) == 0 {
        // Порча до рукопожатия: сервер есть, клиента он не ждёт.
        let config = ChannelConfig::new(layout());
        let server = SharedServer::start_with(&name, &config).unwrap();
        let seg = Segment::open(&name, SEGMENT);
        attach_after_corruption(rng, &name, &seg, case);
        drop(seg);
        drop(server);
    }
    let (mut server, mut client) = connect_pair(&name, case);
    let seg = Segment::open(&name, SEGMENT);
    let mut buf = Vec::new();
    // Честные кадры в обе стороны: PAD-кадры и непустые кольца с начала.
    for _ in 0..rng.below(12) {
        let _ = server.try_send_to_client(&payload(rng));
        let _ = client.try_send_to_server(&payload(rng));
    }
    for _ in 0..iterations {
        let (offset, old, value) = corrupt(rng, &seg);
        (case.offset, case.value) = (offset, value);
        single_op(rng, &mut server, &mut client, &mut buf, case);
        // Половину порч пир «чинит» после операции (если слово с тех пор не
        // менялось): кольцо чаще возвращается в почти верное состояние, и
        // следующие порчи бьют по живым путям, а не по уже отвергнутому
        // кольцу.
        if rng.below(2) == 0 {
            let _ =
                seg.word(offset)
                    .compare_exchange(value, old, Ordering::AcqRel, Ordering::Acquire);
        }
        case.iteration += 1;
    }
    timed(case, "drop client", || drop(client));
    timed(case, "drop server", || drop(server));
}

// ─── Multi ──────────────────────────────────────────────────────────────────

/// Учёт колбэков Multi (сервер и клиенты).
#[derive(Default)]
struct MultiProbe {
    connected: AtomicU32,
    messages: [AtomicU64; 2],
    errors: Mutex<Vec<ShmError>>,
}

impl MultiProbe {
    #[expect(
        clippy::unwrap_used,
        reason = "тестовый помощник: паника -- провал теста"
    )]
    fn record(&self, err: ShmError) {
        self.errors.lock().unwrap().push(err);
    }
}

impl MultiHandler for MultiProbe {
    fn on_client_connect(&self, _client_id: u32) {}
    fn on_client_disconnect(&self, _client_id: u32) {}
    fn on_client_disconnect_reason(&self, _client_id: u32, _reason: DisconnectReason) {}
    fn on_message(&self, client_id: u32, _data: &[u8]) {
        if let Some(count) = self.messages.get(client_id as usize) {
            count.fetch_add(1, Ordering::AcqRel);
        }
    }
    fn on_error(&self, _client_id: Option<u32>, err: ShmError) {
        self.record(err);
    }
}

impl MultiClientHandler for MultiProbe {
    fn on_connect(&self, _slot_id: u32) {
        self.connected.fetch_add(1, Ordering::AcqRel);
    }
    fn on_disconnect(&self) {}
    fn on_disconnect_reason(&self, _reason: DisconnectReason) {}
    fn on_message(&self, _data: &[u8]) {}
    fn on_error(&self, err: ShmError) {
        self.record(err);
    }
}

/// Ошибки, которые worker-ы Multi сообщают в `on_error` под порчей слота:
/// перечень операций канала (`channel_error_allowed`) плюс ошибки
/// переподключения клиента после разрыва -- слот испорчен до или во время
/// рукопожатия: `HandshakeFailed`/`Timeout` (сервер не ответил на
/// испорченную заявку), `VersionMismatch`/`NotReady` (испорчены `version`/
/// `magic`), `NoFreeSlot` (испорчен `slot_claim` слота жертвы: слот выглядит
/// занятым чужим токеном, второй слот держит честный клиент -- свободных
/// нет, пока сервер не снимет протухший захват по `RESERVE_TIMEOUT`).
const fn multi_error_allowed(err: &ShmError) -> bool {
    channel_error_allowed(err)
        || matches!(
            err,
            ShmError::HandshakeFailed
                | ShmError::Timeout
                | ShmError::VersionMismatch { .. }
                | ShmError::NotReady
                | ShmError::NoFreeSlot
        )
}

fn wait_for(what: &str, limit: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !cond() {
        assert!(Instant::now() < deadline, "таймаут: {what}");
        thread::sleep(Duration::from_millis(1));
    }
}

/// Эпоха Multi: сервер на 2 слота, клиент-жертва (его слот портится) и
/// честный клиент (его слот не трогается -- по нему видно, что диспетчер
/// жив).
#[expect(
    clippy::unwrap_used,
    reason = "тестовый помощник: паника -- провал теста"
)]
fn multi_epoch(rng: &mut Rng, case: &mut Case, iterations: u32) {
    let name = unique("MULTI");
    let probe = Arc::new(MultiProbe::default());
    let server = MultiServer::start(
        &name,
        probe.clone(),
        MultiOptions {
            max_clients: 2,
            layout: layout(),
            ..MultiOptions::default()
        },
    )
    .unwrap();
    let client_options = || MultiClientOptions {
        retry_delay: Duration::from_millis(10),
        ..MultiClientOptions::default()
    };
    let victim_probe = Arc::new(MultiProbe::default());
    let victim = MultiClient::connect(&name, victim_probe.clone(), client_options()).unwrap();
    wait_for(
        &format!("жертва подключена; {case:?}"),
        Duration::from_secs(10),
        || victim.is_connected(),
    );
    let honest_probe = Arc::new(MultiProbe::default());
    let honest = MultiClient::connect(&name, honest_probe.clone(), client_options()).unwrap();
    wait_for(
        &format!("честный подключён; {case:?}"),
        Duration::from_secs(10),
        || honest.is_connected(),
    );
    let slot = victim.slot_id();
    let honest_slot = honest.slot_id();
    assert_ne!(slot, honest_slot);
    let seg = Segment::open(&format!("{name}_{slot}"), SEGMENT);

    for i in 0..iterations {
        let (offset, old, value) = corrupt(rng, &seg);
        (case.offset, case.value) = (offset, value);
        let heal = rng.below(2) == 0;
        let data = payload(rng);
        match rng.below(6) {
            0 => ignore(check(
                case,
                "send_to",
                timed(case, "send_to", || server.send_to(slot, &data)),
            )),
            1 => ignore(check(
                case,
                "try_send_to",
                timed(case, "try_send_to", || server.try_send_to(slot, &data)),
            )),
            2 => timed(case, "free_space", || {
                if let Some(free) = server.free_space(slot) {
                    assert!(
                        free.max_frame <= free.bytes && free.bytes <= CAP,
                        "{free:?}"
                    );
                }
            }),
            3 => ignore(check(
                case,
                "broadcast",
                timed(case, "broadcast", || server.broadcast(&data)),
            )),
            4 => ignore(check(
                case,
                "victim send",
                timed(case, "victim send", || victim.send(&data)),
            )),
            _ => ignore(check(
                case,
                "honest send",
                timed(case, "honest send", || honest.send(b"ping")),
            )),
        }
        if i % 2 == 1 {
            // Дать worker-ам и диспетчеру разобрать сделанное.
            thread::sleep(Duration::from_millis(1));
        }
        if heal {
            // Как в single-client: половину порч пир «чинит» (если слово не
            // менялось) -- жертва чаще остаётся подключённой и порча бьёт по
            // живой сессии, а не по уже разорванной.
            let _ =
                seg.word(offset)
                    .compare_exchange(value, old, Ordering::AcqRel, Ordering::Acquire);
        }
        case.iteration += 1;
    }

    // Диспетчер не застрял на испорченном слоте: честный слот обслуживается.
    let before = probe.messages[honest_slot as usize].load(Ordering::Acquire);
    honest.send(b"liveness").unwrap();
    wait_for(
        &format!("честный слот обслуживается; {case:?}"),
        Duration::from_secs(5),
        || probe.messages[honest_slot as usize].load(Ordering::Acquire) > before,
    );
    drop(seg);
    MULTI_SESSIONS.fetch_add(
        u64::from(victim_probe.connected.load(Ordering::Acquire)),
        Ordering::AcqRel,
    );
    // `stop()` синхронный (join worker-ов): граница -- секунды, не бюджет
    // одной операции.
    let stop_within = |what: &str, stop: &dyn Fn()| {
        let started = Instant::now();
        stop();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "{what}: {elapsed:?}; {case:?}"
        );
    };
    stop_within("victim.stop", &|| victim.stop());
    stop_within("honest.stop", &|| honest.stop());
    stop_within("server.stop", &|| server.stop());
    for errors in [&probe.errors, &victim_probe.errors, &honest_probe.errors] {
        for err in errors.lock().unwrap().iter() {
            assert!(multi_error_allowed(err), "on_error({err:?}); {case:?}");
        }
    }
    assert!(
        honest_probe.errors.lock().unwrap().is_empty(),
        "честный клиент: {:?}",
        honest_probe.errors.lock().unwrap()
    );
}

/// Сколько раз жертва Multi подключалась за все эпохи (переподключения
/// после разрывов, вызванных порчей).
static MULTI_SESSIONS: AtomicU64 = AtomicU64::new(0);

/// Итераций (брифа -- 20 000 single-client; Multi -- сверх того). Debug и
/// release -- одинаково: в debug тест укладывается в бюджет (см. отчёт).
const SINGLE_ITERATIONS: u32 = 20_000;
const SINGLE_EPOCH: u32 = 50;
const MULTI_ITERATIONS: u32 = 4_000;
const MULTI_EPOCH: u32 = 200;

#[test]
fn random_corruption_never_panics_or_hangs() {
    const SEED: u64 = 0x5EED_0018_C0FF_EE01;
    // Seed фиксирован; для поиска и воспроизведения -- `XSHM_HOSTILE_SEED`
    // (hex), значение из сообщения об ошибке.
    let seed = std::env::var("XSHM_HOSTILE_SEED")
        .ok()
        .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        .unwrap_or(SEED);
    count_panics();
    let panics_before = PANICS.load(Ordering::Acquire);
    let mut rng = Rng(seed);
    let mut case = Case {
        seed,
        phase: "single",
        iteration: 0,
        offset: 0,
        value: 0,
    };
    let started = Instant::now();
    while case.iteration < SINGLE_ITERATIONS {
        single_epoch(&mut rng, &mut case, SINGLE_EPOCH);
    }
    let single = started.elapsed();
    let single_outcomes = std::mem::take(&mut *OUTCOMES.lock().unwrap());
    case.phase = "multi";
    case.iteration = 0;
    while case.iteration < MULTI_ITERATIONS {
        multi_epoch(&mut rng, &mut case, MULTI_EPOCH);
    }
    println!(
        "random_corruption: seed {seed:#x}; single {single:?}, исходы {single_outcomes:?}; \
         multi {:?}, сессий жертвы {}, исходы {:?}",
        started.elapsed() - single,
        MULTI_SESSIONS.load(Ordering::Acquire),
        OUTCOMES.lock().unwrap()
    );
    assert_eq!(
        PANICS.load(Ordering::Acquire),
        panics_before,
        "паника в потоке библиотеки; seed {seed:#x}"
    );
}

// ─── шторм рукопожатий ──────────────────────────────────────────────────────

/// Колбэки сервера шторма: подключение и отключение обязаны чередоваться.
#[derive(Default)]
struct StormProbe {
    connected: AtomicBool,
    connects: AtomicU32,
    disconnects: AtomicU32,
    /// Причины отключения: `Graceful`, `PeerDied`, `Local`, `Error`.
    reasons: [AtomicU32; 4],
    /// Нарушения чередования (on_connect при подключённом и наоборот).
    violations: AtomicU32,
}

impl AutoHandler for StormProbe {
    fn on_connect(&self) {
        if self.connected.swap(true, Ordering::AcqRel) {
            self.violations.fetch_add(1, Ordering::AcqRel);
        }
        self.connects.fetch_add(1, Ordering::AcqRel);
    }
    fn on_disconnect_reason(&self, reason: DisconnectReason) {
        if !self.connected.swap(false, Ordering::AcqRel) {
            self.violations.fetch_add(1, Ordering::AcqRel);
        }
        self.disconnects.fetch_add(1, Ordering::AcqRel);
        let index = match reason {
            DisconnectReason::Graceful => 0,
            DisconnectReason::PeerDied => 1,
            DisconnectReason::Local => 2,
            _ => 3,
        };
        self.reasons[index].fetch_add(1, Ordering::AcqRel);
    }
}

/// Режим клиента шторма.
#[derive(Clone, Copy, Debug)]
enum StormMode {
    /// `ROUNDS` попыток подключения; `AlreadyConnected`/`Timeout` -- исход
    /// попытки.
    Attempts(u32),
    /// Повтор при `AlreadyConnected` (короткая пауза) до `n` настоящих
    /// рукопожатий на клиента: под нагрузкой снова идут S1-S7, W и S5.
    Sessions(u32),
}

/// 8 клиентов одного `AutoServer` со случайными паузами 0–2 мс. Клиенты
/// соревнуются за один single-client канал: занятый канал --
/// `AlreadyConnected` (ревизия 3), отозванная по таймауту заявка --
/// `Timeout`; зависание -- нет (ни один `connect`/`Drop` не дольше 5 с).
/// Итог: колбэки сервера чередуются, `on_connect` равен сумме
/// `on_disconnect_reason` и текущего подключения (0 или 1), каждое принятие
/// клиентом -- ровно одна сессия сервера, причины отключений -- только
/// `Graceful`.
#[expect(
    clippy::too_many_lines,
    reason = "один сценарий шторма: клиенты, тишина, итоговые утверждения"
)]
#[expect(
    clippy::unwrap_used,
    reason = "тестовый помощник: паника -- провал теста"
)]
fn run_storm(tag: &str, mode: StormMode) {
    const CLIENTS: u64 = 8;
    const SEED: u64 = 0x5EED_0018_5708_0001;
    const HANG: Duration = Duration::from_secs(5);
    /// Общий предел шторма (с большим запасом: шторм с повтором в debug
    /// занимает около 2,4–2,7 с, без повтора -- меньше 1 с).
    const OVERALL: Duration = Duration::from_secs(120);
    let name = unique(tag);
    let probe = Arc::new(StormProbe::default());
    let server = AutoServer::start(
        &name,
        probe.clone(),
        AutoOptions {
            layout: layout(),
            ..AutoOptions::default()
        },
    )
    .unwrap();
    let started = Instant::now();
    let workers: Vec<_> = (0..CLIENTS)
        .map(|index| {
            let name = name.clone();
            thread::spawn(move || {
                let mut rng = Rng(SEED ^ (index + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
                let (mut accepted, mut timeouts, mut busy) = (0u32, 0u32, 0u32);
                let mut round = 0u32;
                loop {
                    let done = match mode {
                        StormMode::Attempts(rounds) => round >= rounds,
                        StormMode::Sessions(sessions) => accepted >= sessions,
                    };
                    if done {
                        break;
                    }
                    assert!(
                        started.elapsed() < OVERALL,
                        "шторм дольше {OVERALL:?}; seed {SEED:#x}, клиент {index}, \
                         принято {accepted}"
                    );
                    round += 1;
                    let t = Instant::now();
                    let result = SharedClient::connect(&name, Duration::from_millis(200));
                    let elapsed = t.elapsed();
                    assert!(
                        elapsed < HANG,
                        "connect {elapsed:?}; клиент {index}, раунд {round}"
                    );
                    match result {
                        Ok(client) => {
                            accepted += 1;
                            let _ = client.try_send_to_server(b"storm");
                            thread::sleep(Duration::from_micros(rng.below(2001)));
                            let t = Instant::now();
                            drop(client);
                            assert!(t.elapsed() < HANG, "drop; клиент {index}, раунд {round}");
                        }
                        Err(ShmError::Timeout) => timeouts += 1,
                        // Ревизия 3: канал занят другим клиентом шторма.
                        Err(ShmError::AlreadyConnected) => {
                            busy += 1;
                            if let StormMode::Sessions(_) = mode {
                                // Повтор: короткая пауза, без паузы раунда.
                                thread::sleep(Duration::from_micros(rng.below(301)));
                                continue;
                            }
                        }
                        Err(err) => panic!(
                            "connect: {err:?}; seed {SEED:#x}, клиент {index}, раунд {round}"
                        ),
                    }
                    thread::sleep(Duration::from_micros(rng.below(2001)));
                }
                (accepted, timeouts, busy)
            })
        })
        .collect();
    let (mut accepted, mut timeouts, mut busy) = (0u32, 0u32, 0u32);
    for worker in workers {
        let (a, t, b) = worker.join().unwrap();
        accepted += a;
        timeouts += t;
        busy += b;
    }
    let storm = started.elapsed();

    // Тишина: счётчики не меняются 200 мс (последний DISCONNECT разобран).
    let deadline = Instant::now() + HANG;
    let mut last = (u32::MAX, u32::MAX);
    loop {
        let now = (
            probe.connects.load(Ordering::Acquire),
            probe.disconnects.load(Ordering::Acquire),
        );
        if now == last {
            break;
        }
        last = now;
        assert!(Instant::now() < deadline, "сервер не затих: {now:?}");
        thread::sleep(Duration::from_millis(200));
    }
    let (connects, disconnects) = last;
    let current = u32::from(probe.connected.load(Ordering::Acquire));
    let reasons = probe.reasons.each_ref().map(|r| r.load(Ordering::Acquire));
    println!(
        "{tag}: {mode:?}, seed {SEED:#x}, {storm:?}, клиент: принято {accepted}, \
         таймаутов {timeouts}, занято {busy}; \
         сервер: on_connect {connects}, on_disconnect_reason {disconnects}, сейчас {current}; \
         причины [Graceful, PeerDied, Local, Error] = {reasons:?}"
    );
    // Ревизия 3 (Task 22): каждое принятие клиентом -- ровно одна сессия
    // сервера (раньше несколько ждущих засчитывали одно рукопожатие по смене
    // `generation`), и ни одного разрыва из-за двух писателей в C2S.
    assert_eq!(accepted, connects, "seed {SEED:#x}: принятия против сессий");
    assert_eq!(
        reasons,
        [disconnects, 0, 0, 0],
        "seed {SEED:#x}: причины отключений"
    );
    assert_eq!(
        probe.violations.load(Ordering::Acquire),
        0,
        "чередование нарушено"
    );
    assert_eq!(connects, disconnects + current);
    assert!(connects > 0 && accepted > 0, "ни одного подключения");
    if let StormMode::Sessions(sessions) = mode {
        let expected = u32::try_from(CLIENTS).unwrap() * sessions;
        assert_eq!(accepted, expected, "seed {SEED:#x}: не все сессии");
    }
    let t = Instant::now();
    drop(server);
    assert!(t.elapsed() < HANG, "остановка сервера {:?}", t.elapsed());
}

/// 8 клиентов × 200 попыток: занятый канал -- исход попытки (в основном
/// проверяет конкуренцию за claim).
#[test]
fn handshake_storm() {
    run_storm("STORM", StormMode::Attempts(200));
}

/// Fix round 1 (находка 4 ревью): клиенты повторяют при
/// `AlreadyConnected`, пока каждый не наберёт 200 настоящих рукопожатий --
/// 1600 сессий под нагрузкой проходят полный путь S1-S7, W/S5 и уход.
#[test]
fn handshake_storm_with_retry() {
    run_storm("STORM_RETRY", StormMode::Sessions(200));
}

// ─── поток с перезаписью ────────────────────────────────────────────────────

/// Число сообщений потока (брифа -- 10⁶).
const FLOOD: u32 = 1_000_000;
/// Маркер конца потока (последнее сообщение; его не вытесняет ничто).
const END: u32 = u32::MAX;
/// Нет ни одного нового сообщения столько времени -- зависание (потерянный
/// `DATA`), а не медленный хост.
const STALL: Duration = Duration::from_secs(10);
/// Общий дедлайн потока.
const FLOOD_DEADLINE: Duration = Duration::from_secs(120);
/// Пауза читателя раз в `SLOW_EVERY` сообщений.
const SLOW_EVERY: u64 = 2048;
const SLOW_PAUSE: Duration = Duration::from_millis(1);
/// Раскладка потока: 8 КиБ и 256 сообщений -- кольцо упирается и в байты,
/// и в лимит сообщений (кадры 16..48 байт).
const FLOOD_CAP: usize = 8192;
const FLOOD_SEGMENT: usize = 256 + 2 * FLOOD_CAP;
/// `drop_count` в `RingHeader`; заголовки колец в каноническом сегменте.
const DROP_COUNT: usize = 12;
const FLOOD_S2C_HEADER: usize = 128;
const FLOOD_C2S_HEADER: usize = FLOOD_S2C_HEADER + RING_HEADER + FLOOD_CAP;

const fn flood_layout() -> ChannelLayout {
    ChannelLayout::symmetric(RingConfig::new(FLOOD_CAP).max_messages(256))
}

/// Сообщение потока: seq (LE) + заполнитель длины от seq (8..40 байт всего).
fn flood_message(seq: u32) -> Vec<u8> {
    let mut msg = seq.to_le_bytes().to_vec();
    msg.resize(8 + (seq % 33) as usize, seq as u8);
    msg
}

#[expect(
    clippy::unwrap_used,
    reason = "тестовый помощник: паника -- провал теста"
)]
fn seq_of(data: &[u8]) -> u32 {
    u32::from_le_bytes(data[..4].try_into().unwrap())
}

/// Приёмник потока: строгий рост seq (пропуски -- вытеснение -- допустимы),
/// счёт принятого, момент последнего сообщения.
struct FloodSink {
    next: AtomicU64,
    received: AtomicU64,
    ended: AtomicBool,
    last_progress: Mutex<Instant>,
    disorder: Mutex<Option<(u64, u32)>>,
}

impl FloodSink {
    fn new() -> Self {
        Self {
            next: AtomicU64::new(0),
            received: AtomicU64::new(0),
            ended: AtomicBool::new(false),
            last_progress: Mutex::new(Instant::now()),
            disorder: Mutex::new(None),
        }
    }

    #[expect(
        clippy::unwrap_used,
        reason = "тестовый помощник: паника -- провал теста"
    )]
    fn accept(&self, data: &[u8]) {
        let seq = seq_of(data);
        let expected = self.next.load(Ordering::Acquire);
        if u64::from(seq) < expected {
            self.disorder.lock().unwrap().get_or_insert((expected, seq));
        }
        self.next.store(u64::from(seq) + 1, Ordering::Release);
        let received = self.received.fetch_add(1, Ordering::AcqRel) + 1;
        if received.is_multiple_of(SLOW_EVERY) {
            // Читатель периодически отстаёт: писатель упирается в кольцо и
            // вытесняет (без этого быстрый читатель почти не даёт перезаписи).
            thread::sleep(SLOW_PAUSE);
        }
        *self.last_progress.lock().unwrap() = Instant::now();
        if seq == END {
            self.ended.store(true, Ordering::Release);
        }
    }

    /// Ждать конца потока: общий дедлайн и «нет прогресса `STALL`».
    #[expect(
        clippy::unwrap_used,
        reason = "тестовый помощник: паника -- провал теста"
    )]
    fn wait_end(&self, what: &str) {
        let deadline = Instant::now() + FLOOD_DEADLINE;
        while !self.ended.load(Ordering::Acquire) {
            let idle = self.last_progress.lock().unwrap().elapsed();
            assert!(
                idle < STALL,
                "{what}: нет сообщений {idle:?} (принято {}) -- потерян DATA?",
                self.received.load(Ordering::Acquire)
            );
            assert!(Instant::now() < deadline, "{what}: общий дедлайн");
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            *self.disorder.lock().unwrap(),
            None,
            "{what}: порядок нарушен"
        );
    }

    fn received(&self) -> u64 {
        self.received.load(Ordering::Acquire)
    }
}

/// Дождаться, пока `got()` станет равно `want` (колбэки вытеснения могут
/// прийти чуть позже маркера конца).
fn converge(what: &str, want: u64, mut got: impl FnMut() -> u64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while got() != want {
        assert!(Instant::now() < deadline, "{what}: {} != {want}", got());
        thread::sleep(Duration::from_millis(5));
    }
}

/// `SharedServer::send_to_client` с перезаписью, клиент спит только на
/// `DATA` (`poll_server`): потерянный сигнал -- `poll` по таймауту при
/// непустом кольце. Итог: принято + вытеснено = отправлено, `drop_count`
/// заголовка = сумма `overwritten`.
#[test]
fn overwrite_flood_shared() {
    let name = unique("FLOOD_SHARED");
    let config = ChannelConfig::new(flood_layout());
    let mut server = SharedServer::start_with(&name, &config).unwrap();
    let waiter = thread::spawn(move || {
        server
            .wait_for_client(Some(Duration::from_secs(10)))
            .unwrap();
        server
    });
    let mut client = SharedClient::connect(&name, Duration::from_secs(10)).unwrap();
    let server = waiter.join().unwrap();
    let seg = Segment::open(&name, FLOOD_SEGMENT);
    let started = Instant::now();
    let producer = thread::spawn(move || {
        let mut overwritten = 0u64;
        for seq in (0..FLOOD).chain([END]) {
            overwritten += u64::from(
                server
                    .send_to_client(&flood_message(seq))
                    .unwrap()
                    .overwritten,
            );
        }
        (server, overwritten)
    });
    let sink = FloodSink::new();
    let mut buf = Vec::new();
    while !sink.ended.load(Ordering::Acquire) {
        assert!(started.elapsed() < FLOOD_DEADLINE, "общий дедлайн");
        if !client.poll_server(Some(Duration::from_secs(2))).unwrap() {
            // Таймаут ожидания DATA: в кольце не должно быть ни кадра.
            assert_eq!(
                client.receive_from_server(&mut buf),
                Err(ShmError::QueueEmpty),
                "DATA потерян: кадр лежит, а читатель спал (принято {})",
                sink.received()
            );
            let idle = sink.last_progress.lock().unwrap().elapsed();
            assert!(idle < STALL, "нет сообщений {idle:?}");
            continue;
        }
        loop {
            match client.receive_from_server(&mut buf) {
                Ok(_) => sink.accept(&buf),
                Err(ShmError::QueueEmpty) => break,
                Err(err) => panic!("receive: {err:?}"),
            }
        }
    }
    let (server, overwritten) = producer.join().unwrap();
    assert_eq!(*sink.disorder.lock().unwrap(), None, "порядок нарушен");
    let sent = u64::from(FLOOD) + 1;
    println!(
        "overwrite_flood_shared: {:?}, принято {}, вытеснено {overwritten}",
        started.elapsed(),
        sink.received()
    );
    assert_eq!(sink.received() + overwritten, sent);
    let drop_count = seg.load(FLOOD_S2C_HEADER + DROP_COUNT);
    assert_eq!(u64::from(drop_count), overwritten % (1 << 32), "drop_count");
    drop(client);
    drop(server);
}

/// Колбэки сервера Auto: подключение и вытеснения направления S2C.
#[derive(Default)]
struct AutoFloodServer {
    connected: AtomicBool,
    overflows: AtomicU64,
}

impl AutoHandler for AutoFloodServer {
    fn on_connect(&self) {
        self.connected.store(true, Ordering::Release);
    }
    fn on_overflow(&self, direction: ChannelKind, count: u32) {
        assert!(matches!(direction, ChannelKind::ServerToClient));
        self.overflows.fetch_add(u64::from(count), Ordering::AcqRel);
    }
}

impl AutoHandler for FloodSink {
    fn on_message(&self, _direction: ChannelKind, payload: &[u8]) {
        self.accept(payload);
    }
}

/// Worker-ы Auto: `AutoServer::send` (очередь с вытеснением + кольцо с
/// перезаписью) под потоком 10⁶; клиентский worker спит на событиях.
/// Итог: принято + `on_overflow` = отправлено.
#[test]
fn overwrite_flood_auto() {
    let name = unique("FLOOD_AUTO");
    let server_probe = Arc::new(AutoFloodServer::default());
    let server = AutoServer::start(
        &name,
        server_probe.clone(),
        AutoOptions {
            layout: flood_layout(),
            ..AutoOptions::default()
        },
    )
    .unwrap();
    let sink = Arc::new(FloodSink::new());
    let client = AutoClient::connect(&name, sink.clone(), AutoOptions::default()).unwrap();
    wait_for("Auto подключён", Duration::from_secs(10), || {
        server_probe.connected.load(Ordering::Acquire)
    });
    let started = Instant::now();
    for seq in (0..FLOOD).chain([END]) {
        server.send(&flood_message(seq)).unwrap();
    }
    sink.wait_end("Auto");
    let sent = u64::from(FLOOD) + 1;
    converge("Auto: принято + вытеснено", sent, || {
        sink.received() + server_probe.overflows.load(Ordering::Acquire)
    });
    println!(
        "overwrite_flood_auto: {:?}, принято {}, вытеснено {}",
        started.elapsed(),
        sink.received(),
        server_probe.overflows.load(Ordering::Acquire)
    );
    client.stop();
    server.stop();
}

/// Приёмник Multi: сервер (от слота) и клиент; клиент считает вытеснения
/// своей очереди.
struct MultiFlood {
    sink: FloodSink,
    connected: AtomicBool,
    evicted: AtomicU64,
}

impl MultiFlood {
    fn new() -> Self {
        Self {
            sink: FloodSink::new(),
            connected: AtomicBool::new(false),
            evicted: AtomicU64::new(0),
        }
    }
}

impl MultiHandler for MultiFlood {
    fn on_client_connect(&self, _client_id: u32) {
        self.connected.store(true, Ordering::Release);
    }
    fn on_client_disconnect(&self, _client_id: u32) {}
    fn on_message(&self, _client_id: u32, data: &[u8]) {
        self.sink.accept(data);
    }
}

impl MultiClientHandler for MultiFlood {
    fn on_connect(&self, _slot_id: u32) {}
    fn on_disconnect(&self) {}
    fn on_message(&self, data: &[u8]) {
        self.sink.accept(data);
    }
    fn on_overflow(&self, dropped: u32) {
        self.evicted.fetch_add(u64::from(dropped), Ordering::AcqRel);
    }
}

/// Multi в обе стороны под потоком 10⁶: `MultiServer::send_to` (кольцо с
/// перезаписью) -> клиент; `MultiClient::send` (очередь + кольцо) ->
/// диспетчер. Итог: принято + вытеснено (`drop_count` кольца + вытеснения
/// очереди) = отправлено.
#[test]
fn overwrite_flood_multi() {
    let name = unique("FLOOD_MULTI");
    let server_side = Arc::new(MultiFlood::new());
    let server = MultiServer::start(
        &name,
        server_side.clone(),
        MultiOptions {
            max_clients: 1,
            layout: flood_layout(),
            ..MultiOptions::default()
        },
    )
    .unwrap();
    let client_side = Arc::new(MultiFlood::new());
    let client =
        MultiClient::connect(&name, client_side.clone(), MultiClientOptions::default()).unwrap();
    wait_for("Multi подключён", Duration::from_secs(10), || {
        client.is_connected() && server_side.connected.load(Ordering::Acquire)
    });
    let slot = client.slot_id();
    let seg = Segment::open(&format!("{name}_{slot}"), FLOOD_SEGMENT);
    let sent = u64::from(FLOOD) + 1;

    // Сервер -> клиент.
    let started = Instant::now();
    for seq in (0..FLOOD).chain([END]) {
        server.send_to(slot, &flood_message(seq)).unwrap();
    }
    client_side.sink.wait_end("Multi S2C");
    let s2c_dropped = u64::from(seg.load(FLOOD_S2C_HEADER + DROP_COUNT));
    assert_eq!(client_side.sink.received() + s2c_dropped, sent, "Multi S2C");
    let s2c = started.elapsed();

    // Клиент -> сервер.
    let started = Instant::now();
    for seq in (0..FLOOD).chain([END]) {
        client.send(&flood_message(seq)).unwrap();
    }
    server_side.sink.wait_end("Multi C2S");
    converge(
        "Multi C2S: принято + вытеснено",
        sent,
        || {
            server_side.sink.received()
                + client_side.evicted.load(Ordering::Acquire)
                + u64::from(seg.load(FLOOD_C2S_HEADER + DROP_COUNT))
        },
    );
    println!(
        "overwrite_flood_multi: S2C {s2c:?} (принято {}, drop_count {s2c_dropped}), C2S {:?} \
         (принято {}, очередь {}, drop_count {})",
        client_side.sink.received(),
        started.elapsed(),
        server_side.sink.received(),
        client_side.evicted.load(Ordering::Acquire),
        seg.load(FLOOD_C2S_HEADER + DROP_COUNT)
    );
    drop(seg);
    client.stop();
    server.stop();
}

// ─── секция с атрибутом кэширования под именем канала ───────────────────────

/// `INVALID_HANDLE_VALUE` -- секция из файла подкачки.
const INVALID_HANDLE_VALUE: *mut c_void = std::ptr::without_provenance_mut(usize::MAX);
const PAGE_READWRITE: u32 = 0x04;
const SEC_COMMIT: u32 = 0x0800_0000;
const SEC_NOCACHE: u32 = 0x1000_0000;
const SEC_WRITECOMBINE: u32 = 0x4000_0000;

/// Надёжность (17.10.2026, пункт 3): чужой процесс занял имя канала секцией
/// `SEC_NOCACHE`/`SEC_WRITECOMBINE` (WC-память нарушает TSO, UC -- каждая
/// атомарная операция мимо кэша). Клиент обязан отвергнуть её при открытии
/// (`Corrupted`), не дожидаясь таймаута и не трогая кольца.
#[test]
fn client_rejects_uncached_squatter() {
    for (tag, extra) in [("NOCACHE", SEC_NOCACHE), ("WRITECOMBINE", SEC_WRITECOMBINE)] {
        let name = unique(tag);
        let wide: Vec<u16> = format!("Local\\{name}")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        // SAFETY: `wide` -- строка с нулём на конце, живёт весь вызов;
        // атрибуты безопасности -- NULL (дескриптор по умолчанию).
        let raw = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE | SEC_COMMIT | extra,
                0,
                65536,
                wide.as_ptr(),
            )
        };
        let squatter =
            NonNull::new(raw).unwrap_or_else(|| panic!("CreateFileMappingW(SEC_{tag}) не удался"));

        let started = Instant::now();
        let result = SharedClient::connect(&name, Duration::from_secs(1));
        match result {
            Err(ShmError::Corrupted) => {}
            Ok(_) => panic!("секция SEC_{tag} принята клиентом"),
            Err(other) => panic!("SEC_{tag}: ожидался Corrupted, получено {other:?}"),
        }
        assert!(
            started.elapsed() < Duration::from_millis(900),
            "SEC_{tag}: отказ должен быть сразу, а не по таймауту"
        );

        // SAFETY: handle создан `CreateFileMappingW` и закрывается один раз.
        let _ = unsafe { CloseHandle(squatter.as_ptr()) };
    }
}

// ─── поток пустых CONNECT_REQ (Task 10) ─────────────────────────────────────

/// `EVENT_MODIFY_STATE`: права на `SetEvent`.
const EVENT_MODIFY_STATE: u32 = 0x0002;

/// Событие канала, открытое «чужим процессом» через kernel32.
struct ForeignEvent(NonNull<c_void>);

// SAFETY: handle события -- просто дескриптор ядра; `SetEvent` и
// `CloseHandle` допустимы с любого потока.
unsafe impl Send for ForeignEvent {}

impl ForeignEvent {
    /// Открыть `Local\<канал>_C2S_CONNECT_REQ` с правом `EVENT_MODIFY_STATE`.
    fn connect_req(channel: &str) -> Self {
        let wide: Vec<u16> = format!("Local\\{channel}_C2S_CONNECT_REQ")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        // SAFETY: `wide` -- строка с нулём на конце, живёт весь вызов.
        let raw = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, wide.as_ptr()) };
        Self(NonNull::new(raw).unwrap_or_else(|| panic!("OpenEventW({channel}) не удался")))
    }

    fn set(&self) {
        // SAFETY: handle открыт `OpenEventW` и жив до `Drop`.
        let _ = unsafe { SetEvent(self.0.as_ptr()) };
    }
}

impl Drop for ForeignEvent {
    fn drop(&mut self) {
        // SAFETY: handle открыт `OpenEventW` и закрывается один раз.
        let _ = unsafe { CloseHandle(self.0.as_ptr()) };
    }
}

/// Сервер и клиент: факт и число подключений.
#[derive(Default)]
struct Sessions {
    connected: AtomicBool,
}

impl AutoHandler for Sessions {
    fn on_connect(&self) {
        self.connected.store(true, Ordering::Release);
    }
    fn on_disconnect(&self) {
        self.connected.store(false, Ordering::Release);
    }
}

/// Task 10 (П1, П2): два атакующих взводят `CONNECT_REQ` Auto-сервера без
/// пауз (как чужой процесс -- kernel32), а 10 клиентов подряд всё равно
/// подключаются каждый ≤ 500 мс, и `stop()` под потоком -- ≤ 300 мс.
#[test]
fn connect_req_flood_does_not_starve_auto_server() {
    let name = unique("REQFLOOD");
    let server_probe = Arc::new(Sessions::default());
    let server = AutoServer::start(&name, server_probe.clone(), AutoOptions::default()).unwrap();
    let run = Arc::new(AtomicBool::new(true));
    let started = Instant::now();
    let attackers: Vec<_> = (0..2)
        .map(|_| {
            let event = ForeignEvent::connect_req(&name);
            let run = run.clone();
            thread::spawn(move || {
                while run.load(Ordering::Acquire) && started.elapsed() < Duration::from_secs(3) {
                    event.set();
                }
            })
        })
        .collect();
    thread::sleep(Duration::from_millis(50));
    for round in 0..10 {
        let probe = Arc::new(Sessions::default());
        let t = Instant::now();
        let client = AutoClient::connect(
            &name,
            probe.clone(),
            AutoOptions {
                reconnect_delay: Duration::from_millis(50),
                ..AutoOptions::default()
            },
        )
        .unwrap();
        wait_for(
            "подключение клиента",
            Duration::from_secs(5),
            || probe.connected.load(Ordering::Acquire),
        );
        let elapsed = t.elapsed();
        assert!(
            elapsed <= Duration::from_millis(500),
            "клиент {round}: {elapsed:?}"
        );
        client.stop();
        wait_for(
            "сервер увидел уход",
            Duration::from_secs(5),
            || !server_probe.connected.load(Ordering::Acquire),
        );
    }
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "поток кончился раньше клиентов: {:?}",
        started.elapsed()
    );
    let t = Instant::now();
    server.stop();
    let stopped_in = t.elapsed();
    run.store(false, Ordering::Release);
    for attacker in attackers {
        attacker.join().unwrap();
    }
    assert!(stopped_in <= Duration::from_millis(300), "{stopped_in:?}");
}
