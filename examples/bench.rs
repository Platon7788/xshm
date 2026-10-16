//! Бенчмарк xshm без внешних зависимостей (только std).
//!
//! ```text
//! cargo run --release --example bench -- throughput 16
//! cargo run --release --example bench -- latency 4096
//! ```
//!
//! Печатает одну строку `mode size msgs/s MiB/s p50_us p99_us`:
//! - `throughput` -- `SharedServer` -> `SharedClient`, два потока одного
//!   процесса: писатель `try_send_to_client` + `wait_for_space` (без
//!   перезаписи), читатель `poll_server` + `receive_from_server`;
//!   перцентилей нет (`-`);
//! - `latency` -- ping-pong `size` байт (клиент -> сервер -> клиент,
//!   ожидание через `poll_*`), 100 000 итераций после разогрева; `msgs/s` --
//!   кругов в секунду, `MiB/s` -- полезные байты одного направления,
//!   `p50_us`/`p99_us` -- время круга (RTT) по отсортированному массиву.
//!
//! Раскладка: кольца по умолчанию (2 МиБ, сообщения до 65 535 байт), если
//! сообщение в них помещается; иначе -- 8 МиБ без лимита числа сообщений с
//! максимальным сообщением 1 МиБ. Выбранная раскладка печатается в stderr.

use std::error::Error;
use std::hint::black_box;
use std::thread;
use std::time::{Duration, Instant};

use xshm::{
    ChannelConfig, ChannelLayout, DEFAULT_MAX_MESSAGE_SIZE, RingConfig, SharedClient, SharedServer,
    ShmError,
};

type BenchResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

/// Итераций ping-pong в замере (по брифу).
const LATENCY_ITERS: usize = 100_000;
/// Итераций ping-pong на разогрев (не попадают в перцентили).
const LATENCY_WARMUP: usize = 2_000;
/// Сколько байт перекачивает замер пропускной способности (по умолчанию).
const THROUGHPUT_BYTES: usize = 1 << 30;
/// Границы числа сообщений замера пропускной способности.
const THROUGHPUT_MIN_MSGS: usize = 2_000;
const THROUGHPUT_MAX_MSGS: usize = 5_000_000;
/// Таймаут любого отдельного ожидания: зависание -- ошибка, а не вечный бенч.
const STEP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Throughput,
    Latency,
}

fn main() -> BenchResult<()> {
    let mut args = std::env::args().skip(1);
    let mode = match args.next().as_deref() {
        Some("throughput") => Mode::Throughput,
        Some("latency") => Mode::Latency,
        _ => return Err(usage()),
    };
    let size: usize = match args.next() {
        Some(s) => s.parse().map_err(|_| usage())?,
        None => return Err(usage()),
    };
    if size == 0 {
        return Err("size должен быть >= 1".into());
    }

    let (layout, layout_desc) = layout_for(size);
    eprintln!("раскладка: {layout_desc}");
    let config = ChannelConfig::new(layout);
    let name = format!("XSHM_BENCH_{}_{}", std::process::id(), size);

    match mode {
        Mode::Throughput => {
            let total = (THROUGHPUT_BYTES / size).clamp(THROUGHPUT_MIN_MSGS, THROUGHPUT_MAX_MSGS);
            let elapsed = run_throughput(&name, &config, size, total)?;
            let secs = elapsed.as_secs_f64();
            let msgs = total as f64 / secs;
            let mib = (total * size) as f64 / secs / f64::from(1u32 << 20);
            println!("mode size msgs/s MiB/s p50_us p99_us");
            println!("throughput {size} {msgs:.0} {mib:.1} - -");
        }
        Mode::Latency => {
            let (elapsed, mut rtt) = run_latency(&name, &config, size)?;
            rtt.sort_unstable();
            let secs = elapsed.as_secs_f64();
            let rps = LATENCY_ITERS as f64 / secs;
            let mib = (LATENCY_ITERS * size) as f64 / secs / f64::from(1u32 << 20);
            let p50 = percentile_us(&rtt, 50);
            let p99 = percentile_us(&rtt, 99);
            println!("mode size msgs/s MiB/s p50_us p99_us");
            println!("latency {size} {rps:.0} {mib:.1} {p50:.2} {p99:.2}");
        }
    }
    Ok(())
}

fn usage() -> Box<dyn Error + Send + Sync> {
    "использование: bench [throughput|latency] <size>".into()
}

/// Раскладка под размер сообщения (см. документацию модуля).
const fn layout_for(size: usize) -> (ChannelLayout, &'static str) {
    if size <= DEFAULT_MAX_MESSAGE_SIZE {
        (
            ChannelLayout::DEFAULT,
            "по умолчанию (2 МиБ, 500 сообщений, до 65 535 байт)",
        )
    } else {
        (
            ChannelLayout::symmetric(RingConfig::new(8 << 20).max_message_size(1 << 20)),
            "8 МиБ, без лимита числа сообщений, до 1 МиБ",
        )
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

/// Отправка без перезаписи: при полном кольце -- ждать места в ядре.
fn send_s2c(server: &SharedServer, payload: &[u8]) -> BenchResult<()> {
    loop {
        match server.try_send_to_client(payload) {
            Ok(_) => return Ok(()),
            Err(ShmError::QueueFull) => {
                if !server.wait_for_space(payload.len(), Some(STEP_TIMEOUT))? {
                    return Err("таймаут ожидания места (сервер)".into());
                }
            }
            Err(err) => return Err(err.into()),
        }
    }
}

fn send_c2s(client: &SharedClient, payload: &[u8]) -> BenchResult<()> {
    loop {
        match client.try_send_to_server(payload) {
            Ok(_) => return Ok(()),
            Err(ShmError::QueueFull) => {
                if !client.wait_for_space(payload.len(), Some(STEP_TIMEOUT))? {
                    return Err("таймаут ожидания места (клиент)".into());
                }
            }
            Err(err) => return Err(err.into()),
        }
    }
}

/// Принять одно сообщение от сервера, ожидая его в ядре.
fn recv_s2c(client: &SharedClient, buf: &mut Vec<u8>) -> BenchResult<usize> {
    loop {
        match client.receive_from_server(buf) {
            Ok(len) => return Ok(len),
            Err(ShmError::QueueEmpty) => {
                if !client.poll_server(Some(STEP_TIMEOUT))? {
                    return Err(timeout_error("клиент", client.receive_from_server(buf)));
                }
            }
            Err(err) => return Err(err.into()),
        }
    }
}

fn recv_c2s(server: &SharedServer, buf: &mut Vec<u8>) -> BenchResult<usize> {
    loop {
        match server.receive_from_client(buf) {
            Ok(len) => return Ok(len),
            Err(ShmError::QueueEmpty) => {
                if !server.poll_client(Some(STEP_TIMEOUT))? {
                    return Err(timeout_error("сервер", server.receive_from_client(buf)));
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
fn timeout_error(side: &str, again: xshm::Result<usize>) -> Box<dyn Error + Send + Sync> {
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
    handle.join().map_err(|_| -> Box<dyn Error + Send + Sync> {
        "поток клиента запаниковал".into()
    })?
}

/// Пропускная способность: `total` сообщений по `size` байт сервер -> клиент.
/// Время -- от первой отправки до приёма последнего сообщения.
fn run_throughput(
    name: &str,
    config: &ChannelConfig,
    size: usize,
    total: usize,
) -> BenchResult<Duration> {
    let (server, client) = connect_pair(name, config, move |client| -> BenchResult<Instant> {
        let mut buf = Vec::with_capacity(size);
        for _ in 0..total {
            let len = recv_s2c(&client, &mut buf)?;
            if len != size {
                return Err(format!("принято {len} байт вместо {size}").into());
            }
            black_box(&buf);
        }
        Ok(Instant::now())
    })?;

    let payload: Vec<u8> = (0..size).map(|i| i as u8).collect();
    let start = Instant::now();
    let sent = (|| -> BenchResult<()> {
        for _ in 0..total {
            send_s2c(&server, &payload)?;
        }
        Ok(())
    })();
    // Клиента ждём в любом случае: его ошибка информативнее нашей.
    let end = join(client)?;
    sent?;
    Ok(end.duration_since(start))
}

/// Ping-pong: клиент шлёт `size` байт, сервер возвращает их же. Возвращает
/// общее время замера и RTT каждой итерации (нс).
fn run_latency(
    name: &str,
    config: &ChannelConfig,
    size: usize,
) -> BenchResult<(Duration, Vec<u64>)> {
    let rounds = LATENCY_WARMUP + LATENCY_ITERS;
    let (server, client) = connect_pair(
        name,
        config,
        move |client| -> BenchResult<(Duration, Vec<u64>)> {
            let payload: Vec<u8> = (0..size).map(|i| i as u8).collect();
            let mut buf = Vec::with_capacity(size);
            let mut rtt = Vec::with_capacity(LATENCY_ITERS);
            let mut start = Instant::now();
            for i in 0..rounds {
                if i == LATENCY_WARMUP {
                    start = Instant::now();
                }
                let t0 = Instant::now();
                send_c2s(&client, &payload)?;
                let len = recv_s2c(&client, &mut buf)?;
                let dt = t0.elapsed();
                if len != size {
                    return Err(format!("эхо {len} байт вместо {size}").into());
                }
                if i >= LATENCY_WARMUP {
                    rtt.push(u64::try_from(dt.as_nanos()).unwrap_or(u64::MAX));
                }
            }
            Ok((start.elapsed(), rtt))
        },
    )?;

    let echoed = (|| -> BenchResult<()> {
        let mut buf = Vec::with_capacity(size);
        for _ in 0..rounds {
            let len = recv_c2s(&server, &mut buf)?;
            send_s2c(&server, &buf[..len])?;
        }
        Ok(())
    })();
    let result = join(client)?;
    echoed?;
    Ok(result)
}
