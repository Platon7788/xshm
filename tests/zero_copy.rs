//! Zero-copy доставка Auto (0.10, `AutoOptions::zero_copy_delivery`):
//! `on_message` получает срез прямо из кольца, без промежуточного буфера.
//!
//! Отличить «из кольца» от «из буфера копии» снаружи крейта можно по
//! адресам срезов: копирующий worker переиспользует один `Vec` (адрес
//! соседних сообщений одинаков), zero-copy отдаёт позиции кадров в кольце
//! (следующее сообщение лежит ровно через `frame_size` предыдущего, либо --
//! после PAD -- снова с начала кольца).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use xshm::{
    AutoClient, AutoHandler, AutoOptions, AutoServer, ChannelKind, ChannelLayout, RingConfig,
    ShmError, frame_size,
};

/// Сообщений в каждую сторону: на кольце 64 КиБ -- несколько обёрток с PAD.
const COUNT: u32 = 300;

fn unique_name(tag: &str) -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    format!(
        "XSHM_ZC_{tag}_{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "таймаут: {what}");
        thread::sleep(Duration::from_millis(5));
    }
}

/// Длина по номеру: 100..3100 байт -- кадры разной длины, PAD у границы.
const fn len_for(seq: u32) -> usize {
    100 + (seq.wrapping_mul(7919) % 3000) as usize
}

fn payload(seq: u32) -> Vec<u8> {
    let len = len_for(seq);
    let mut v: Vec<u8> = (0..len)
        .map(|i| (i as u32).wrapping_mul(131).wrapping_add(seq) as u8)
        .collect();
    v[..4].copy_from_slice(&seq.to_le_bytes());
    v
}

/// Получатель: адрес и содержимое каждого среза `on_message`.
#[derive(Default)]
struct Recorder {
    connected: AtomicBool,
    received: Mutex<Vec<(usize, Vec<u8>)>>,
}

impl Recorder {
    fn received(&self) -> Vec<(usize, Vec<u8>)> {
        self.received
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn count(&self) -> usize {
        self.received
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

impl AutoHandler for Recorder {
    fn on_connect(&self) {
        self.connected.store(true, Ordering::Release);
    }
    fn on_message(&self, _direction: ChannelKind, data: &[u8]) {
        self.received
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((data.as_ptr().addr(), data.to_vec()));
    }
}

const fn layout() -> ChannelLayout {
    ChannelLayout::symmetric(RingConfig::new(64 << 10))
}

/// Опции с доставкой прямо из кольца.
fn zero_copy_options() -> AutoOptions {
    let options = AutoOptions {
        layout: layout(),
        ..AutoOptions::default()
    };
    // SAFETY: оба конца -- этот процесс (честный пир xshm) и пишут только
    // `try_send` (без перезаписи): пока идёт `on_message`, кадр никто не
    // меняет.
    unsafe { options.zero_copy_delivery() }
}

/// Отправить `COUNT` сообщений без потерь (повтор при полной очереди).
fn send_all(send: impl Fn(&[u8]) -> xshm::Result<()>) {
    for seq in 0..COUNT {
        let data = payload(seq);
        loop {
            match send(&data) {
                Ok(()) => break,
                Err(ShmError::QueueFull) => thread::sleep(Duration::from_millis(1)),
                Err(err) => panic!("try_send: {err:?}"),
            }
        }
    }
}

/// Пары соседних сообщений: (лежат в кольце подряд, адрес совпал с
/// предыдущим, адрес «вернулся» к началу кольца).
fn adjacency(received: &[(usize, Vec<u8>)]) -> (usize, usize, usize) {
    let (mut adjacent, mut same, mut wrapped) = (0, 0, 0);
    for pair in received.windows(2) {
        let ((prev_addr, prev), (addr, _)) = (&pair[0], &pair[1]);
        // Payload следующего кадра -- через кадр предыдущего целиком.
        if *addr == prev_addr + frame_size(prev.len()) {
            adjacent += 1;
        } else if addr == prev_addr {
            same += 1;
        } else if addr < prev_addr {
            wrapped += 1;
        }
    }
    (adjacent, same, wrapped)
}

fn check_delivery(received: &[(usize, Vec<u8>)], side: &str) {
    assert_eq!(received.len(), COUNT as usize, "{side}: число сообщений");
    for (seq, (_, data)) in (0..COUNT).zip(received) {
        assert_eq!(*data, payload(seq), "{side}: сообщение {seq}");
    }
}

/// Сообщения в обе стороны доставляются срезами прямо из кольца: соседние
/// лежат через `frame_size` (кадры непрерывны) или после PAD с начала
/// кольца, ни одна пара не делит адрес буфера копии. Контроль -- копирующая
/// доставка по умолчанию: адрес буфера переиспользуется.
#[test]
fn auto_delivers_without_copy_roundtrip() {
    let name = unique_name("RT");
    let server_h = Arc::new(Recorder::default());
    let client_h = Arc::new(Recorder::default());
    let server = AutoServer::start(&name, server_h.clone(), zero_copy_options()).unwrap();
    let client = AutoClient::connect(&name, client_h.clone(), zero_copy_options()).unwrap();
    wait_until("подключение", || {
        server_h.connected.load(Ordering::Acquire) && client_h.connected.load(Ordering::Acquire)
    });

    send_all(|data| client.try_send(data));
    wait_until("клиент -> сервер", || {
        server_h.count() >= COUNT as usize
    });
    send_all(|data| server.try_send(data));
    wait_until("сервер -> клиент", || {
        client_h.count() >= COUNT as usize
    });
    client.stop();
    server.stop();

    for (side, recorder) in [("сервер", &server_h), ("клиент", &client_h)] {
        let received = recorder.received();
        check_delivery(&received, side);
        let (adjacent, same, wrapped) = adjacency(&received);
        assert_eq!(same, 0, "{side}: срез из буфера копии, а не из кольца");
        assert!(wrapped > 0, "{side}: позиции ни разу не обернулись");
        assert_eq!(
            adjacent + wrapped,
            COUNT as usize - 1,
            "{side}: срез не по позиции кадра в кольце"
        );
    }

    // Контроль: по умолчанию доставка копирует в один переиспользуемый
    // буфер -- соседние сообщения в кольце подряд не видны.
    let name = unique_name("COPY");
    let server_h = Arc::new(Recorder::default());
    let client_h = Arc::new(Recorder::default());
    let options = || AutoOptions {
        layout: layout(),
        ..AutoOptions::default()
    };
    let server = AutoServer::start(&name, server_h.clone(), options()).unwrap();
    let client = AutoClient::connect(&name, client_h.clone(), options()).unwrap();
    wait_until("подключение (копия)", || {
        server_h.connected.load(Ordering::Acquire) && client_h.connected.load(Ordering::Acquire)
    });
    send_all(|data| client.try_send(data));
    wait_until("доставка (копия)", || {
        server_h.count() >= COUNT as usize
    });
    client.stop();
    server.stop();
    let received = server_h.received();
    check_delivery(&received, "копия");
    let (adjacent, same, _) = adjacency(&received);
    assert_eq!(adjacent, 0, "копирующая доставка отдала срез из кольца");
    assert!(same > 0, "буфер копии не переиспользуется");
}
