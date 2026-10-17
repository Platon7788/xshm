//! Бенчмарк xshm без внешних зависимостей (только std).
//!
//! ```text
//! cargo run --release --example bench -- throughput 16
//! cargo run --release --example bench -- throughput 16 5
//! cargo run --release --example bench -- latency 4096 5 --ring 1048576 --max-messages 64
//! cargo run --release --example bench -- latency 16 5 --spin 2000
//! cargo run --release --example bench -- throughput 65536 5 --zero-copy
//! ```
//!
//! `bench [throughput|latency] <size> [runs] [--ring <bytes>] [--max-messages <n>] [--spin <iterations>] [--zero-copy]`
//!
//! Каждый прогон печатает строку `mode size msgs/s MiB/s p50_us p99_us layout`:
//! - `throughput` -- `SharedServer` -> `SharedClient`, два потока одного
//!   процесса: писатель `try_send_to_client` + `wait_for_space` (без
//!   перезаписи), читатель `poll_server` + `receive_from_server`;
//!   перцентилей нет (`-`). Перед каждым прогоном -- разогрев вне замера
//!   (не меньше одного полного круга по кольцу: первое касание страниц не
//!   попадает во время);
//! - `latency` -- ping-pong `size` байт (клиент -> сервер -> клиент,
//!   ожидание через `poll_*`), 100 000 итераций после 2 000 разогревочных;
//!   `msgs/s` -- кругов в секунду, `MiB/s` -- полезные байты одного
//!   направления, `p50_us`/`p99_us` -- время круга (RTT) по отсортированному
//!   массиву;
//! - `layout` -- использованная раскладка (одинаковая в обе стороны).
//!
//! `runs` (по умолчанию 1) -- число прогонов в ОДНОМ процессе по ОДНОМУ
//! подключению (канал не пересоздаётся: меряется передача, а не
//! рукопожатие). При `runs > 1` дополнительно печатаются строки `median` и
//! `min-max`; медиана считается по каждой колонке отдельно: при нечётном
//! числе прогонов -- средний элемент, при чётном -- среднее двух средних.
//!
//! Раскладка по умолчанию: кольца по умолчанию (2 МиБ, 500 сообщений, до
//! 65 535 байт), если сообщение в них помещается; иначе -- 8 МиБ без лимита
//! числа сообщений с максимальным сообщением 1 МиБ. `--ring` меняет только
//! ёмкость (лимит размера сообщения урезается до вмещаемого кольцом и
//! поднимается до `size`), `--max-messages` -- только лимит числа сообщений.
//!
//! `--spin <iterations>` (0.10) -- спин перед ожиданием в ядре на обеих
//! сторонах (`poll_*_spin`, `wait_for_space_spin`); по умолчанию 0 -- без
//! спина. Значение печатается в колонке `layout` (`spin=N`).
//!
//! `--zero-copy` (0.10, только `throughput`) -- читатель принимает
//! сообщение `receive_with` срезом прямо из кольца, без копии в свой буфер
//! (писатель не меняется: `try_send_to_client` копирует payload в кольцо).
//! Проверка порядка -- та же, по срезу. Печатается в колонке `layout`
//! (`zc=1`).
//!
//! Проверка данных: при `size >= 8` первые 8 байт payload -- сквозной номер
//! сообщения `u64` (LE), получатель сверяет порядок (потеря, повтор,
//! перестановка -- ошибка и ненулевой код выхода). При `size < 8` номер не
//! помещается -- проверяется только длина.

use std::error::Error;
use std::hint::black_box;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use xshm::{
    ChannelConfig, ChannelLayout, DEFAULT_MAX_MESSAGE_SIZE, RingConfig, SharedClient, SharedServer,
    ShmError, SpinPolicy, frame_size, max_payload_for,
};

type BenchError = Box<dyn Error + Send + Sync>;
type BenchResult<T> = Result<T, BenchError>;

/// Итераций ping-pong в замере (по брифу).
const LATENCY_ITERS: usize = 100_000;
/// Итераций ping-pong на разогрев (не попадают в перцентили).
const LATENCY_WARMUP: usize = 2_000;
/// Сколько байт перекачивает замер пропускной способности (по умолчанию).
const THROUGHPUT_BYTES: usize = 1 << 30;
/// Границы числа сообщений замера пропускной способности.
const THROUGHPUT_MIN_MSGS: usize = 2_000;
const THROUGHPUT_MAX_MSGS: usize = 5_000_000;
/// Нижняя граница числа сообщений разогрева throughput.
const THROUGHPUT_WARMUP_MIN: usize = 16;
/// Таймаут любого отдельного ожидания: зависание -- ошибка, а не вечный бенч.
const STEP_TIMEOUT: Duration = Duration::from_secs(10);
/// Длина сквозного номера сообщения в начале payload.
const SEQ_LEN: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Throughput,
    Latency,
}

/// Разобранная командная строка.
struct Args {
    mode: Mode,
    size: usize,
    runs: usize,
    ring: Option<usize>,
    max_messages: Option<u32>,
    spin: SpinPolicy,
    zero_copy: bool,
}

/// Результат одного прогона.
#[derive(Clone, Copy)]
struct Sample {
    msgs: f64,
    mib: f64,
    p50: Option<f64>,
    p99: Option<f64>,
}

fn main() -> BenchResult<()> {
    let args = parse_args()?;
    let ring = resolve_ring(&args)?;
    let layout_desc = format!(
        "{},spin={},zc={}",
        describe(&ring),
        args.spin.iterations,
        u8::from(args.zero_copy)
    );
    let config = ChannelConfig::new(ChannelLayout::symmetric(ring));
    let name = format!("XSHM_BENCH_{}_{}", std::process::id(), args.size);
    let spin = args.spin;

    let samples = match args.mode {
        Mode::Throughput => {
            let total =
                (THROUGHPUT_BYTES / args.size).clamp(THROUGHPUT_MIN_MSGS, THROUGHPUT_MAX_MSGS);
            // Один полный круг по кольцу (+1 кадр), но не меньше минимума.
            let warmup = (ring.capacity / frame_size(args.size)).max(THROUGHPUT_WARMUP_MIN) + 1;
            let plan = Plan {
                size: args.size,
                total,
                warmup,
                runs: args.runs,
                spin,
                zero_copy: args.zero_copy,
            };
            run_throughput(&name, &config, &plan)?
        }
        Mode::Latency => run_latency(&name, &config, args.size, args.runs, spin)?,
    };

    let mode = match args.mode {
        Mode::Throughput => "throughput",
        Mode::Latency => "latency",
    };
    let size = args.size;
    println!("mode size msgs/s MiB/s p50_us p99_us layout");
    for s in &samples {
        println!(
            "{mode} {size} {:.0} {:.1} {} {} {layout_desc}",
            s.msgs,
            s.mib,
            opt_us(s.p50),
            opt_us(s.p99)
        );
    }
    if samples.len() > 1 {
        let n = samples.len();
        let col =
            |f: fn(&Sample) -> Option<f64>| -> Vec<f64> { samples.iter().filter_map(f).collect() };
        let msgs = col(|s| Some(s.msgs));
        let mib = col(|s| Some(s.mib));
        let p50 = col(|s| s.p50);
        let p99 = col(|s| s.p99);
        println!(
            "# медиана по {n} прогонам, каждая колонка отдельно: нечётное число -- средний элемент, чётное -- среднее двух средних"
        );
        println!(
            "median {mode} {size} {:.0} {:.1} {} {} {layout_desc}",
            median(&msgs).unwrap_or(0.0),
            median(&mib).unwrap_or(0.0),
            opt_us(median(&p50)),
            opt_us(median(&p99))
        );
        println!(
            "min-max {mode} {size} {} {} {} {} {layout_desc}",
            range(&msgs, 0),
            range(&mib, 1),
            range(&p50, 2),
            range(&p99, 2)
        );
    }
    Ok(())
}

fn usage() -> BenchError {
    "использование: bench [throughput|latency] <size> [runs] [--ring <bytes>] [--max-messages <n>] [--spin <iterations>] [--zero-copy]"
        .into()
}

fn parse_args() -> BenchResult<Args> {
    let mut positional = Vec::new();
    let mut ring = None;
    let mut max_messages = None;
    let mut spin = SpinPolicy::NONE;
    let mut zero_copy = false;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--ring" => {
                let v = it.next().ok_or_else(usage)?;
                ring = Some(v.parse::<usize>().map_err(|_| usage())?);
            }
            "--max-messages" => {
                let v = it.next().ok_or_else(usage)?;
                max_messages = Some(v.parse::<u32>().map_err(|_| usage())?);
            }
            "--spin" => {
                let v = it.next().ok_or_else(usage)?;
                spin = SpinPolicy::new(v.parse::<u32>().map_err(|_| usage())?);
            }
            "--zero-copy" => zero_copy = true,
            _ => positional.push(arg),
        }
    }
    let mode = match positional.first().map(String::as_str) {
        Some("throughput") => Mode::Throughput,
        Some("latency") => Mode::Latency,
        _ => return Err(usage()),
    };
    let size: usize = positional
        .get(1)
        .ok_or_else(usage)?
        .parse()
        .map_err(|_| usage())?;
    let runs: usize = match positional.get(2) {
        Some(v) => v.parse().map_err(|_| usage())?,
        None => 1,
    };
    if positional.len() > 3 {
        return Err(usage());
    }
    if size == 0 {
        return Err("size должен быть >= 1".into());
    }
    if runs == 0 {
        return Err("runs должен быть >= 1".into());
    }
    if zero_copy && mode != Mode::Throughput {
        return Err("--zero-copy -- только для throughput".into());
    }
    Ok(Args {
        mode,
        size,
        runs,
        ring,
        max_messages,
        spin,
        zero_copy,
    })
}

/// Раскладка одного направления: значения по умолчанию для размера, затем
/// явные `--ring`/`--max-messages` (см. документацию модуля).
fn resolve_ring(args: &Args) -> BenchResult<RingConfig> {
    let mut ring = if args.size <= DEFAULT_MAX_MESSAGE_SIZE {
        RingConfig::DEFAULT
    } else {
        RingConfig::new(8 << 20).max_message_size(1 << 20)
    };
    if let Some(capacity) = args.ring {
        let max_size = ring
            .max_message_size
            .min(max_payload_for(capacity))
            .max(args.size);
        ring = RingConfig::new(capacity)
            .max_messages(ring.max_messages)
            .max_message_size(max_size);
    }
    if let Some(n) = args.max_messages {
        ring = ring.max_messages(n);
    }
    ring.validate()
        .map_err(|e| format!("раскладка {}: {e}", describe(&ring)))?;
    if args.size > ring.max_message_size {
        return Err(format!(
            "size {} больше max_message_size {} раскладки",
            args.size, ring.max_message_size
        )
        .into());
    }
    Ok(ring)
}

/// Раскладка одним словом для строки результата.
fn describe(ring: &RingConfig) -> String {
    let max_messages = if ring.max_messages == u32::MAX {
        "unlimited".to_owned()
    } else {
        ring.max_messages.to_string()
    };
    format!(
        "ring={},max_messages={max_messages},max_message={}",
        ring.capacity, ring.max_message_size
    )
}

fn opt_us(v: Option<f64>) -> String {
    v.map_or_else(|| "-".to_owned(), |v| format!("{v:.2}"))
}

/// Медиана: нечётное число -- средний элемент, чётное -- среднее двух средних.
fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        f64::midpoint(v[n / 2 - 1], v[n / 2])
    })
}

/// `мин..макс` с `prec` знаками после запятой, `-` для пустой колонки.
fn range(values: &[f64], prec: usize) -> String {
    let min = values.iter().copied().reduce(f64::min);
    let max = values.iter().copied().reduce(f64::max);
    match (min, max) {
        (Some(min), Some(max)) => format!("{min:.prec$}..{max:.prec$}"),
        _ => "-".to_owned(),
    }
}

/// Перцентиль `p` (0..=100) отсортированного массива наносекунд, в мкс.
fn percentile_us(sorted_ns: &[u64], p: usize) -> f64 {
    if sorted_ns.is_empty() {
        return 0.0;
    }
    let idx = (sorted_ns.len() * p / 100).min(sorted_ns.len() - 1);
    sorted_ns[idx] as f64 / 1_000.0
}

/// Записать сквозной номер в начало payload (если помещается).
fn stamp(payload: &mut [u8], seq: u64) {
    if let Some(head) = payload.get_mut(..SEQ_LEN) {
        head.copy_from_slice(&seq.to_le_bytes());
    }
}

/// Сверить принятое сообщение: длина, а при `size >= 8` -- номер `expected`.
/// Одна загрузка и одно сравнение -- на 16 Б не доминирует.
fn check(buf: &[u8], len: usize, size: usize, expected: u64, side: &str) -> BenchResult<()> {
    if len != size {
        return Err(format!("{side}: принято {len} байт вместо {size}").into());
    }
    if let Some(head) = buf.get(..SEQ_LEN) {
        let mut raw = [0u8; SEQ_LEN];
        raw.copy_from_slice(head);
        let got = u64::from_le_bytes(raw);
        if got != expected {
            return Err(format!(
                "{side}: нарушен порядок -- принято сообщение №{got}, ожидалось №{expected}"
            )
            .into());
        }
    }
    Ok(())
}

/// Отправка без перезаписи: при полном кольце -- ждать места в ядре.
fn send_s2c(server: &SharedServer, payload: &[u8], spin: SpinPolicy) -> BenchResult<()> {
    loop {
        match server.try_send_to_client(payload) {
            Ok(_) => return Ok(()),
            Err(ShmError::QueueFull) => {
                if !server.wait_for_space_spin(payload.len(), Some(STEP_TIMEOUT), spin)? {
                    return Err("таймаут ожидания места (сервер)".into());
                }
            }
            Err(err) => return Err(err.into()),
        }
    }
}

fn send_c2s(client: &SharedClient, payload: &[u8], spin: SpinPolicy) -> BenchResult<()> {
    loop {
        match client.try_send_to_server(payload) {
            Ok(_) => return Ok(()),
            Err(ShmError::QueueFull) => {
                if !client.wait_for_space_spin(payload.len(), Some(STEP_TIMEOUT), spin)? {
                    return Err("таймаут ожидания места (клиент)".into());
                }
            }
            Err(err) => return Err(err.into()),
        }
    }
}

/// Принять одно сообщение от сервера, ожидая его в ядре.
fn recv_s2c(client: &mut SharedClient, buf: &mut Vec<u8>, spin: SpinPolicy) -> BenchResult<usize> {
    loop {
        match client.receive_from_server(buf) {
            Ok(len) => return Ok(len),
            Err(ShmError::QueueEmpty) => {
                if !client.poll_server_spin(Some(STEP_TIMEOUT), spin)? {
                    return Err(timeout_error("клиент", client.receive_from_server(buf)));
                }
            }
            Err(err) => return Err(err.into()),
        }
    }
}

fn recv_c2s(server: &mut SharedServer, buf: &mut Vec<u8>, spin: SpinPolicy) -> BenchResult<usize> {
    loop {
        match server.receive_from_client(buf) {
            Ok(len) => return Ok(len),
            Err(ShmError::QueueEmpty) => {
                if !server.poll_client_spin(Some(STEP_TIMEOUT), spin)? {
                    return Err(timeout_error("сервер", server.receive_from_client(buf)));
                }
            }
            Err(err) => return Err(err.into()),
        }
    }
}

/// `--zero-copy`: принять одно сообщение от сервера срезом прямо из кольца
/// и сверить его (длина и номер -- как у `check`), ожидая его в ядре.
fn recv_s2c_zero_copy(
    client: &mut SharedClient,
    size: usize,
    expected: u64,
    spin: SpinPolicy,
) -> BenchResult<()> {
    loop {
        // SAFETY: писатель кольца S2C -- поток этого же процесса (честный
        // xshm, доверенный) и пишет только `try_send_to_client` без
        // перезаписи: пока выполняется замыкание, кадр никто не меняет.
        // Второго читателя нет по типу (`&mut SharedClient`).
        let got = unsafe {
            client.receive_with(|data| {
                check(data, data.len(), size, expected, "клиент")?;
                black_box(data);
                Ok(())
            })
        };
        match got {
            Ok(checked) => return checked,
            Err(ShmError::QueueEmpty) => {
                if !client.poll_server_spin(Some(STEP_TIMEOUT), spin)? {
                    let mut buf = Vec::new();
                    return Err(timeout_error(
                        "клиент",
                        client.receive_from_server(&mut buf),
                    ));
                }
            }
            Err(err) => return Err(err.into()),
        }
    }
}

/// Ошибка таймаута `poll_*`. `again` -- повторное чтение после таймаута:
/// если сообщение всё же лежит в кольце, писатель записал его, не взведя
/// `DATA` (потерянное пробуждение), -- это дефект протокола, а не медленный
/// пир, и бенчмарк сообщает о нём явно.
fn timeout_error(side: &str, again: xshm::Result<usize>) -> BenchError {
    match again {
        Ok(len) => format!(
            "{side}: потерянное пробуждение DATA -- {}-мс таймаут poll, а в кольце лежало сообщение {len} байт",
            STEP_TIMEOUT.as_millis()
        )
        .into(),
        Err(_) => format!("{side}: таймаут ожидания данных").into(),
    }
}

/// Сервер в текущем потоке, клиент -- в отдельном; возвращает подключённый
/// сервер и handle потока клиента, исполняющего `client_body`.
fn connect_pair<T, F>(
    name: &str,
    config: &ChannelConfig,
    client_body: F,
) -> BenchResult<(SharedServer, thread::JoinHandle<BenchResult<T>>)>
where
    T: Send + 'static,
    F: FnOnce(SharedClient) -> BenchResult<T> + Send + 'static,
{
    let mut server = SharedServer::start_with(name, config)?;
    let client_name = name.to_owned();
    let handle = thread::spawn(move || -> BenchResult<T> {
        let client = SharedClient::connect(&client_name, STEP_TIMEOUT)?;
        client_body(client)
    });
    server.wait_for_client(Some(STEP_TIMEOUT))?;
    Ok((server, handle))
}

fn join<T>(handle: thread::JoinHandle<BenchResult<T>>) -> BenchResult<T> {
    handle
        .join()
        .map_err(|_| -> BenchError { "поток клиента запаниковал".into() })?
}

/// Пропускная способность: `runs` прогонов по `total` сообщений `size` байт
/// сервер -> клиент по одному подключению. Перед каждым прогоном --
/// `warmup` сообщений вне замера; клиент сообщает о приёме разогрева через
/// канал std (с таймаутом на стороне сервера -- без риска взаимной
/// блокировки), и только после этого сервер запускает часы. Время прогона --
/// от первой отправки до приёма последнего сообщения.
/// Параметры замера пропускной способности.
#[derive(Clone, Copy)]
struct Plan {
    size: usize,
    total: usize,
    warmup: usize,
    runs: usize,
    spin: SpinPolicy,
    zero_copy: bool,
}

fn run_throughput(name: &str, config: &ChannelConfig, plan: &Plan) -> BenchResult<Vec<Sample>> {
    let Plan {
        size,
        total,
        warmup,
        runs,
        spin,
        zero_copy,
    } = *plan;
    let (warmed_tx, warmed_rx) = mpsc::channel::<()>();
    let (server, client) = connect_pair(
        name,
        config,
        move |mut client| -> BenchResult<Vec<Instant>> {
            let mut buf = Vec::with_capacity(size);
            let mut expected = 0u64;
            let mut ends = Vec::with_capacity(runs);
            let mut receive = |expected: u64| -> BenchResult<()> {
                if zero_copy {
                    return recv_s2c_zero_copy(&mut client, size, expected, spin);
                }
                let len = recv_s2c(&mut client, &mut buf, spin)?;
                check(&buf, len, size, expected, "клиент")?;
                black_box(&buf);
                Ok(())
            };
            for _ in 0..runs {
                for _ in 0..warmup {
                    receive(expected)?;
                    expected += 1;
                }
                // Сервер мог уже упасть -- тогда его ошибку вернёт join.
                let _ = warmed_tx.send(());
                for _ in 0..total {
                    receive(expected)?;
                    expected += 1;
                }
                ends.push(Instant::now());
            }
            Ok(ends)
        },
    )?;

    let mut payload: Vec<u8> = (0..size).map(|i| i as u8).collect();
    let mut starts = Vec::with_capacity(runs);
    let sent = (|| -> BenchResult<()> {
        let mut seq = 0u64;
        for _ in 0..runs {
            for _ in 0..warmup {
                stamp(&mut payload, seq);
                send_s2c(&server, &payload, spin)?;
                seq += 1;
            }
            warmed_rx
                .recv_timeout(STEP_TIMEOUT)
                .map_err(|_| -> BenchError {
                    "клиент не подтвердил разогрев".into()
                })?;
            starts.push(Instant::now());
            for _ in 0..total {
                stamp(&mut payload, seq);
                send_s2c(&server, &payload, spin)?;
                seq += 1;
            }
        }
        Ok(())
    })();
    // Клиента ждём в любом случае: его ошибка информативнее нашей.
    let ends = join(client)?;
    sent?;
    Ok(starts
        .iter()
        .zip(&ends)
        .map(|(start, end)| {
            let secs = end.duration_since(*start).as_secs_f64();
            Sample {
                msgs: total as f64 / secs,
                mib: (total * size) as f64 / secs / f64::from(1u32 << 20),
                p50: None,
                p99: None,
            }
        })
        .collect())
}

/// Ping-pong: клиент шлёт `size` байт, сервер возвращает их же; `runs`
/// прогонов по одному подключению, у каждого свой разогрев. Сервер сверяет
/// порядок принятого, клиент -- что эхо несёт отправленный номер.
fn run_latency(
    name: &str,
    config: &ChannelConfig,
    size: usize,
    runs: usize,
    spin: SpinPolicy,
) -> BenchResult<Vec<Sample>> {
    let rounds = LATENCY_WARMUP + LATENCY_ITERS;
    let (mut server, client) = connect_pair(
        name,
        config,
        move |mut client| -> BenchResult<Vec<Sample>> {
            let mut payload: Vec<u8> = (0..size).map(|i| i as u8).collect();
            let mut buf = Vec::with_capacity(size);
            let mut rtt = Vec::with_capacity(LATENCY_ITERS);
            let mut samples = Vec::with_capacity(runs);
            let mut seq = 0u64;
            for _ in 0..runs {
                rtt.clear();
                let mut start = Instant::now();
                for i in 0..rounds {
                    if i == LATENCY_WARMUP {
                        start = Instant::now();
                    }
                    stamp(&mut payload, seq);
                    let t0 = Instant::now();
                    send_c2s(&client, &payload, spin)?;
                    let len = recv_s2c(&mut client, &mut buf, spin)?;
                    let dt = t0.elapsed();
                    check(&buf, len, size, seq, "клиент (эхо)")?;
                    seq += 1;
                    if i >= LATENCY_WARMUP {
                        rtt.push(u64::try_from(dt.as_nanos()).unwrap_or(u64::MAX));
                    }
                }
                let secs = start.elapsed().as_secs_f64();
                rtt.sort_unstable();
                samples.push(Sample {
                    msgs: LATENCY_ITERS as f64 / secs,
                    mib: (LATENCY_ITERS * size) as f64 / secs / f64::from(1u32 << 20),
                    p50: Some(percentile_us(&rtt, 50)),
                    p99: Some(percentile_us(&rtt, 99)),
                });
            }
            Ok(samples)
        },
    )?;

    let echoed = (|| -> BenchResult<()> {
        let mut buf = Vec::with_capacity(size);
        for expected in 0..(runs * rounds) as u64 {
            let len = recv_c2s(&mut server, &mut buf, spin)?;
            check(&buf, len, size, expected, "сервер")?;
            send_s2c(&server, &buf[..len], spin)?;
        }
        Ok(())
    })();
    let result = join(client)?;
    echoed?;
    Ok(result)
}
