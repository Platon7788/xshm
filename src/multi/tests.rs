use super::*;

struct TestHandler {
    connects: AtomicU32,
    disconnects: AtomicU32,
    messages: AtomicU32,
    last_reason: Mutex<Option<DisconnectReason>>,
    errors: Mutex<Vec<(Option<u32>, ShmError)>>,
}

impl TestHandler {
    fn new() -> Self {
        Self {
            connects: AtomicU32::new(0),
            disconnects: AtomicU32::new(0),
            messages: AtomicU32::new(0),
            last_reason: Mutex::new(None),
            errors: Mutex::new(Vec::new()),
        }
    }
}

impl MultiHandler for TestHandler {
    fn on_client_connect(&self, _client_id: u32) {
        self.connects.fetch_add(1, Ordering::AcqRel);
    }
    fn on_client_disconnect(&self, _client_id: u32) {
        self.disconnects.fetch_add(1, Ordering::AcqRel);
    }
    fn on_client_disconnect_reason(&self, client_id: u32, reason: DisconnectReason) {
        *self.last_reason.lock().unwrap() = Some(reason);
        self.on_client_disconnect(client_id);
    }
    fn on_message(&self, _client_id: u32, _data: &[u8]) {
        self.messages.fetch_add(1, Ordering::AcqRel);
    }
    fn on_error(&self, client_id: Option<u32>, err: ShmError) {
        self.errors.lock().unwrap().push((client_id, err));
    }
}

#[derive(Default)]
struct ClientRecorder {
    connects: AtomicU32,
    messages: AtomicU32,
    reasons: Mutex<Vec<DisconnectReason>>,
}

impl MultiClientHandler for ClientRecorder {
    fn on_connect(&self, _slot_id: u32) {
        self.connects.fetch_add(1, Ordering::AcqRel);
    }
    fn on_disconnect(&self) {}
    fn on_disconnect_reason(&self, reason: DisconnectReason) {
        self.reasons.lock().unwrap().push(reason);
    }
    fn on_message(&self, _data: &[u8]) {
        self.messages.fetch_add(1, Ordering::AcqRel);
    }
}

fn unique(tag: &str) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("TEST_MULTI_{tag}_{}_{}", std::process::id(), ts % 1_000_000)
}

fn wait_for(what: &str, limit: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !cond() {
        assert!(Instant::now() < deadline, "таймаут: {what}");
        thread::sleep(Duration::from_millis(5));
    }
}

fn single_slot(max_clients: u32) -> MultiOptions {
    MultiOptions {
        max_clients,
        ..MultiOptions::default()
    }
}

/// Остановить worker-ы и дождаться их выхода, чтобы тест мутировал состояние
/// слотов без конкуренции с ними.
fn stop_workers(server: &MultiServer) {
    server.stop();
    assert!(server.worker_handles.lock().unwrap().is_empty());
}

/// Регрессия (аудит 2026-07-10): `slot_timeout` не ограничивался
/// сверху, позволяя caller-у задать значение >= RESERVE_TIMEOUT, из-за
/// чего сервер мог отнять слот у ещё легитимно подключающегося клиента.
#[test]
fn clamp_slot_timeout_stays_below_reserve_timeout_with_margin() {
    // Большой запрошенный timeout — клампится с запасом.
    let clamped = clamp_slot_timeout(Duration::from_secs(9999));
    assert!(clamped < RESERVE_TIMEOUT);
    assert!(clamped <= RESERVE_TIMEOUT.saturating_sub(RESERVE_SAFETY_MARGIN));

    // Запрошенный timeout ровно на границе RESERVE_TIMEOUT — тоже клампится.
    let clamped_at_boundary = clamp_slot_timeout(RESERVE_TIMEOUT);
    assert!(clamped_at_boundary < RESERVE_TIMEOUT);

    // Маленький (дефолтный) timeout — не трогается, он и так безопасен.
    let small = Duration::from_secs(5);
    assert_eq!(clamp_slot_timeout(small), small);
}

/// Регрессия (аудит 2026-10-16): лимит `max_send_queue` действовал только
/// у подключённого клиента -- пока сервера нет, mpsc-канал рос без
/// предела. Теперь `send` сам вытесняет старейшее (overwrite-семантика,
/// как у `AutoOptions`) в любом состоянии, а длина проверяется синхронно.
#[test]
fn send_queue_is_bounded_while_disconnected() {
    let client = MultiClient::connect(
        &unique("NO_SERVER_QUEUE"),
        Arc::new(ClientRecorder::default()),
        MultiClientOptions {
            max_send_queue: 2,
            ..MultiClientOptions::default()
        },
    )
    .expect("connect");
    for data in [b"a", b"b", b"c"] {
        client.send(data).unwrap();
    }
    let queued: Vec<Vec<u8>> = client
        .outbox
        .queue
        .lock()
        .unwrap()
        .iter()
        .cloned()
        .collect();
    assert_eq!(queued, [b"b".to_vec(), b"c".to_vec()]);
    assert_eq!(client.send(b""), Err(ShmError::MessageTooSmall));
}

/// Регрессия (аудит 2026-07-10): токены захвата не должны предсказуемо
/// повторяться/коллизировать (старая схема `pid<<8 ^ n` гарантированно
/// коллизировала между процессами при n >= 256) и никогда не должны
/// совпадать с CLAIM_FREE.
#[test]
fn claim_tokens_are_never_free_and_practically_unique() {
    use std::collections::HashSet;

    let tokens: HashSet<u32> = (0..1000).map(|_| crate::shared::claim_token()).collect();

    assert!(
        !tokens.contains(&CLAIM_FREE),
        "claim_token() никогда не должен вернуть CLAIM_FREE"
    );
    // При случайной 32-битной генерации 1000 значений коллизии
    // практически исключены (день рождений: ~1e-4 при 2^32 пространстве).
    assert_eq!(
        tokens.len(),
        1000,
        "токены из 1000 последовательных вызовов должны быть различны"
    );
}

#[test]
fn test_multi_server_start() {
    let handler = Arc::new(TestHandler::new());
    let server = MultiServer::start(&unique("START"), handler, MultiOptions::default());
    assert!(server.is_ok());
    let server = server.unwrap();
    assert_eq!(server.client_count(), 0);
    server.stop();
}

/// `stop()` обязан синхронно дождаться выхода worker-потоков: после
/// возврата список handle-ов пуст (взяты и заджойнены), иначе вызывающий
/// может освободить состояние handler'а до того, как worker перестал дёргать
/// callbacks.
#[test]
fn stop_synchronously_joins_worker() {
    let handler = Arc::new(TestHandler::new());
    let server =
        MultiServer::start(&unique("STOP_JOIN"), handler, MultiOptions::default()).expect("start");

    server.stop();

    assert!(
        server.worker_handles.lock().unwrap().is_empty(),
        "stop() должен забрать и заджойнить worker-ы синхронно"
    );

    // Повторный stop() — идемпотентен, не паникует и не виснет.
    server.stop();
}

/// Регрессия на High-баг из аудита 2026-07-10: клиент, упавший ПОСЛЕ
/// завершения handshake (claim НЕ снят, connected=true), без наблюдаемого
/// процесса должен быть обнаружен разовой проверкой РЕАЛЬНО мёртвого
/// процесса-владельца и попасть в orphaned — иначе слот терялся бы навсегда.
#[test]
fn reclaim_detects_connected_slot_with_dead_owner_process() {
    let handler = Arc::new(TestHandler::new());
    let server = MultiServer::start(&unique("DEAD_OWNER"), handler, single_slot(1)).expect("start");
    stop_workers(&server);

    // Реальный завершившийся процесс -- гарантированно мёртвый PID.
    let mut child = std::process::Command::new("cmd")
        .args(["/C", "exit", "0"])
        .spawn()
        .expect("spawn short-lived child process");
    let dead_pid = child.id();
    child.wait().expect("wait for child exit");

    const STALE_TOKEN: u32 = 0xDEAD_0001;
    {
        let slots = server.slots.read().unwrap();
        let mut slot0 = slots[0].lock().unwrap();
        slot0.connected = true;
        slot0.last_liveness_check = None; // форсируем немедленную проверку
        let control = slot0.server.view().control_block();
        control.slot_claim.store(STALE_TOKEN, Ordering::Release);
        control.slot_owner_pid.store(dead_pid, Ordering::Release);
    }

    // is_process_alive может не сразу увидеть завершение (см. запас в
    // win::tests::exited_process_is_detected_as_dead) -- ретраим.
    let mut orphaned = Vec::new();
    for _ in 0..50 {
        {
            let slots = server.slots.read().unwrap();
            slots[0].lock().unwrap().last_liveness_check = None;
        }
        orphaned = server.reclaim_stale_claims(0..1, Instant::now()).orphaned;
        if !orphaned.is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }

    assert_eq!(
        orphaned,
        vec![(0, STALE_TOKEN, DisconnectReason::PeerDied)],
        "connected-слот с claim от мёртвого процесса должен быть обнаружен"
    );
}

/// Контрольная проверка: connected-слот с claim от ЖИВОГО процесса (наш
/// собственный) НЕ должен попасть в orphaned — иначе liveness-проверка
/// отключала бы легитимно работающих клиентов (false positive).
#[test]
fn reclaim_does_not_flag_connected_slot_with_alive_owner_process() {
    let handler = Arc::new(TestHandler::new());
    let server =
        MultiServer::start(&unique("ALIVE_OWNER"), handler, single_slot(1)).expect("start");
    stop_workers(&server);

    {
        let slots = server.slots.read().unwrap();
        let mut slot0 = slots[0].lock().unwrap();
        slot0.connected = true;
        slot0.last_liveness_check = None;
        let control = slot0.server.view().control_block();
        control.slot_claim.store(0xABCD_0001, Ordering::Release);
        control
            .slot_owner_pid
            .store(std::process::id(), Ordering::Release);
    }

    let sweep = server.reclaim_stale_claims(0..1, Instant::now());
    assert!(
        sweep.orphaned.is_empty(),
        "connected-слот с живым процессом-владельцем не должен считаться осиротевшим"
    );
    assert!(
        sweep.next_deadline.is_none(),
        "проверка живости без handle -- не таймер: дедлайн не назначается"
    );
}

/// Детерминированная проверка обнаружения «осиротевшего» слота
/// (брошенное рукопожатие): connected=true, но claim снят клиентом.
#[test]
fn reclaim_detects_orphaned_connected_slot() {
    let handler = Arc::new(TestHandler::new());
    let server = MultiServer::start(&unique("ORPHAN"), handler, single_slot(2)).expect("start");
    stop_workers(&server);

    // Слот 0: handshake «завершён», но claim снят клиентом. Слот 1 — свободен.
    {
        let slots = server.slots.read().unwrap();
        let mut slot0 = slots[0].lock().unwrap();
        slot0.connected = true;
        slot0
            .server
            .view()
            .control_block()
            .slot_claim
            .store(CLAIM_FREE, Ordering::Release);
    }

    let orphaned = server.reclaim_stale_claims(0..2, Instant::now()).orphaned;
    assert_eq!(
        orphaned,
        vec![(0, CLAIM_FREE, DisconnectReason::Graceful)],
        "осиротевший connected-слот (claim=FREE) должен быть обнаружен"
    );
}

/// Регрессия на race "unconditional store race" (аудит 2026-07-10):
/// если между обнаружением orphaned-слота и его обработкой новый клиент
/// успел легитимно захватить слот (CAS FREE->token), обработка НЕ должна
/// затирать его claim/connected — слот должен быть просто пропущен.
#[test]
fn handle_orphaned_slot_disconnect_does_not_clobber_racing_new_claim() {
    let handler = Arc::new(TestHandler::new());
    let server =
        MultiServer::start(&unique("ORPHAN_RACE"), handler, single_slot(1)).expect("start");
    stop_workers(&server);

    const RACING_TOKEN: u32 = 0xDEAD_BEEF;
    {
        let slots = server.slots.read().unwrap();
        let mut slot0 = slots[0].lock().unwrap();
        slot0.connected = true;
        let claim_field = &slot0.server.view().control_block().slot_claim;
        claim_field.store(CLAIM_FREE, Ordering::Release);
        claim_field
            .compare_exchange(
                CLAIM_FREE,
                RACING_TOKEN,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .expect("simulated racing claim must succeed");
    }

    server.handle_orphaned_slot_disconnect(0, CLAIM_FREE, DisconnectReason::Graceful);

    let slots = server.slots.read().unwrap();
    let slot0 = slots[0].lock().unwrap();
    assert!(
        slot0.connected,
        "connected не должен быть сброшен — новый клиент уже владеет слотом"
    );
    assert_eq!(
        slot0.claim(),
        RACING_TOKEN,
        "claim нового клиента не должен быть затёрт обратно в FREE"
    );
}

/// Контрольная проверка: без гонки (claim реально всё ещё FREE)
/// `handle_orphaned_slot_disconnect` обязан нормально отключить слот.
#[test]
fn handle_orphaned_slot_disconnect_disconnects_when_still_free() {
    let handler = Arc::new(TestHandler::new());
    let server =
        MultiServer::start(&unique("ORPHAN_NORACE"), handler, single_slot(1)).expect("start");
    stop_workers(&server);

    {
        let slots = server.slots.read().unwrap();
        let mut slot0 = slots[0].lock().unwrap();
        slot0.connected = true;
        slot0
            .server
            .view()
            .control_block()
            .slot_claim
            .store(CLAIM_FREE, Ordering::Release);
    }

    server.handle_orphaned_slot_disconnect(0, CLAIM_FREE, DisconnectReason::Graceful);

    let slots = server.slots.read().unwrap();
    assert!(
        !slots[0].lock().unwrap().connected,
        "слот должен быть отключён — claim реально FREE"
    );
}

/// Регрессия (аудит 2026-07-28): `slot_id` сбрасывался только на ветке
/// disconnect-события, поэтому выход worker-а по `running == false`
/// (`stop()`/`Drop`) оставлял `is_connected()` навсегда `true`.
#[test]
fn is_connected_becomes_false_after_stop() {
    let name = unique("STOP_FLAG");
    let server =
        MultiServer::start(&name, Arc::new(TestHandler::new()), single_slot(2)).expect("start");
    let client = MultiClient::connect(
        &name,
        Arc::new(ClientRecorder::default()),
        MultiClientOptions::default(),
    )
    .expect("client connect");

    wait_for("подключение", Duration::from_secs(5), || {
        client.is_connected()
    });
    client.stop();
    wait_for(
        "is_connected()==false после stop()",
        Duration::from_secs(5),
        || !client.is_connected(),
    );
    server.stop();
}

/// Главное требование 0.9: в простое ни сервер, ни клиент не просыпаются
/// (раньше -- каждые 50 мс, ~20 пробуждений в секунду на каждой стороне).
#[test]
fn idle_server_and_client_do_not_wake_up() {
    let name = unique("IDLE");
    let server_handler = Arc::new(TestHandler::new());
    let server = MultiServer::start(&name, server_handler.clone(), single_slot(4)).expect("start");
    let recorder = Arc::new(ClientRecorder::default());
    let client = MultiClient::connect(&name, recorder.clone(), MultiClientOptions::default())
        .expect("client connect");
    wait_for("подключение", Duration::from_secs(5), || {
        server.client_count() == 1 && client.is_connected()
    });

    // Живой обмен работает (и считается пробуждениями -- это события).
    client.send(b"ping").unwrap();
    wait_for(
        "сообщение серверу",
        Duration::from_secs(2),
        || server_handler.messages.load(Ordering::Acquire) == 1,
    );
    server.send_to(client.slot_id(), b"pong").unwrap();
    wait_for(
        "сообщение клиенту",
        Duration::from_secs(2),
        || recorder.messages.load(Ordering::Acquire) == 1,
    );

    // Разовая проверка брошенного рукопожатия -- дедлайн, а не тик: ждём,
    // пока он пройдёт, и дальше не должно быть НИ ОДНОГО пробуждения.
    thread::sleep(HANDSHAKE_VERIFY_DELAY + Duration::from_millis(300));
    let (s0, c0) = (server.wakeups(), client.wakeups());
    thread::sleep(Duration::from_secs(1));
    let (s1, c1) = (server.wakeups(), client.wakeups());
    assert_eq!(s1 - s0, 0, "сервер просыпался в простое");
    assert_eq!(c1 - c0, 0, "клиент просыпался в простое");

    drop(client);
    server.stop();
}

/// 0.10: спин включён у сервера и клиента -- обмен работает (данные,
/// увиденные спином, обрабатываются без ядра), а в простое бюджет
/// отспинивается один раз: ноль пробуждений, как без спина.
#[test]
fn idle_with_spin_does_not_wake_up() {
    let name = unique("IDLE_SPIN");
    let server_handler = Arc::new(TestHandler::new());
    let server = MultiServer::start(
        &name,
        server_handler.clone(),
        MultiOptions {
            spin: SpinPolicy::new(20_000),
            ..single_slot(4)
        },
    )
    .expect("start");
    let recorder = Arc::new(ClientRecorder::default());
    let client = MultiClient::connect(
        &name,
        recorder.clone(),
        MultiClientOptions {
            spin: SpinPolicy::new(20_000),
            ..MultiClientOptions::default()
        },
    )
    .expect("client connect");
    wait_for("подключение", Duration::from_secs(5), || {
        server.client_count() == 1 && client.is_connected()
    });

    for round in 1..=50 {
        client.send(b"ping").unwrap();
        wait_for(
            "сообщение серверу",
            Duration::from_secs(2),
            || server_handler.messages.load(Ordering::Acquire) == round,
        );
        server.send_to(client.slot_id(), b"pong").unwrap();
        wait_for(
            "сообщение клиенту",
            Duration::from_secs(2),
            || recorder.messages.load(Ordering::Acquire) == round,
        );
    }

    thread::sleep(HANDSHAKE_VERIFY_DELAY + Duration::from_millis(300));
    let (s0, c0) = (server.wakeups(), client.wakeups());
    thread::sleep(Duration::from_secs(1));
    assert_eq!(server.wakeups() - s0, 0, "сервер просыпался в простое");
    assert_eq!(client.wakeups() - c0, 0, "клиент просыпался в простое");

    drop(client);
    server.stop();
}

/// Протухший захват (клиент упал между CAS и подключением) освобождается по
/// ДЕДЛАЙНУ `reserve_timeout`: сервер просыпается от «толчка» и затем ровно
/// один раз -- в момент дедлайна, без тика.
#[test]
fn stale_claim_is_released_at_deadline_not_by_ticking() {
    let name = unique("STALE_CLAIM");
    let reserve = Duration::from_millis(400);
    let server =
        MultiServer::start_with(&name, Arc::new(TestHandler::new()), single_slot(1), reserve)
            .expect("start");
    let slot_name = format!("{name}_0");
    // Worker успел пройти первый обход и уснуть (иначе этот обход увидел бы
    // захват раньше толчка).
    thread::sleep(Duration::from_millis(100));

    // «Упавший» клиент: захватил слот и не подключился.
    assert!(try_claim_slot(None, &slot_name, 0xC1A1_0001).unwrap());
    thread::sleep(Duration::from_millis(100));
    let w0 = server.wakeups();
    assert_eq!(w0, 0, "захват сам по себе сервер не будит");

    let t0 = Instant::now();
    nudge_stale_claims(None, &name);
    let claim_of_slot0 = || {
        let slots = server.slots.read().unwrap();
        slots[0].lock().unwrap().claim()
    };
    wait_for(
        "освобождение захвата",
        Duration::from_secs(3),
        || claim_of_slot0() == CLAIM_FREE,
    );
    let released_after = t0.elapsed();
    assert!(
        released_after >= reserve - Duration::from_millis(20),
        "захват снят раньше дедлайна: {released_after:?}"
    );
    thread::sleep(Duration::from_millis(100));
    let wakes = server.wakeups() - w0;
    // Толчок + дедлайн (+1 запас на совпадение сигналов).
    assert!(
        (2..=3).contains(&wakes),
        "ожидалось пробуждение от толчка и на дедлайне, было {wakes}"
    );

    // Слот снова в обороте.
    let recorder = Arc::new(ClientRecorder::default());
    let client = MultiClient::connect(&name, recorder, MultiClientOptions::default()).unwrap();
    wait_for(
        "подключение к освобождённому слоту",
        Duration::from_secs(5),
        || client.is_connected(),
    );
    drop(client);
    server.stop();
}

/// Сквозной сценарий: клиент, не нашедший свободного слота (его держит
/// захват упавшего клиента), сам толкает сервер и подключается после
/// дедлайна -- без тика на сервере.
#[test]
fn client_waiting_for_slot_gets_it_after_stale_claim_deadline() {
    let name = unique("STALE_E2E");
    let server = MultiServer::start_with(
        &name,
        Arc::new(TestHandler::new()),
        single_slot(1),
        Duration::from_millis(300),
    )
    .expect("start");
    assert!(try_claim_slot(None, &format!("{name}_0"), 0xC1A1_0002).unwrap());

    let client = MultiClient::connect(
        &name,
        Arc::new(ClientRecorder::default()),
        MultiClientOptions {
            retry_delay: Duration::from_millis(100),
            ..MultiClientOptions::default()
        },
    )
    .unwrap();
    wait_for(
        "подключение после дедлайна",
        Duration::from_secs(5),
        || client.is_connected(),
    );
    drop(client);
    server.stop();
}

/// Захватчик, создавший заранее `B_N` за последним слотом с запрещающим
/// DACL, превращает итог перебора в `AccessDenied` -- но толчок протухших
/// захватов от этого не отключается (иначе сервер спал бы, а захват
/// упавшего клиента держал бы слот вечно).
#[test]
fn denied_squatter_slot_does_not_disable_nudge() {
    let name = unique("STALE_DENIED");
    let server = MultiServer::start_with(
        &name,
        Arc::new(TestHandler::new()),
        single_slot(1),
        Duration::from_millis(300),
    )
    .expect("start");
    // Пустой DACL: открыть секцию не может никто, включая этого пользователя.
    let mut deny_all = vec![1u8, 0, 0x04, 0x80];
    deny_all.extend_from_slice(&[0; 12]);
    deny_all.extend_from_slice(&20u32.to_le_bytes());
    deny_all.extend_from_slice(&[2, 0, 8, 0, 0, 0, 0, 0]);
    let attributes = ObjectSecurity::Descriptor(deny_all).attributes().unwrap();
    let _squatter = Mapping::create(format!("{name}_1"), 4096, &attributes).unwrap();
    // Worker успел пройти первый обход и уснуть: захват он увидит только
    // после толчка.
    thread::sleep(Duration::from_millis(100));
    assert!(try_claim_slot(None, &format!("{name}_0"), 0xC1A1_0003).unwrap());
    assert_eq!(
        claim_free_slot(None, &name).unwrap_err(),
        ShmError::AccessDenied
    );

    let client = MultiClient::connect(
        &name,
        Arc::new(ClientRecorder::default()),
        MultiClientOptions {
            retry_delay: Duration::from_millis(100),
            ..MultiClientOptions::default()
        },
    )
    .unwrap();
    wait_for(
        "подключение после дедлайна при захватчике",
        Duration::from_secs(5),
        || client.is_connected(),
    );
    drop(client);
    server.stop();
}

/// `stop()` сервера (два worker-а, есть подключённый клиент) и Drop клиента
/// (подключённого и ждущего сервер в паузе `retry_delay`) возвращаются сразу:
/// их будит событие, а не тик.
#[test]
fn stop_is_prompt() {
    let name = unique("PROMPT");
    let server =
        MultiServer::start(&name, Arc::new(TestHandler::new()), single_slot(31)).expect("start");
    // 0.10: 31 слот -- один поток-диспетчер (до 0.10 -- два worker-а).
    assert_eq!(server.worker_handles.lock().unwrap().len(), 1);
    let client = MultiClient::connect(
        &name,
        Arc::new(ClientRecorder::default()),
        MultiClientOptions::default(),
    )
    .unwrap();
    wait_for("подключение", Duration::from_secs(5), || {
        client.is_connected()
    });

    let t0 = Instant::now();
    drop(client);
    let client_stop = t0.elapsed();
    let t0 = Instant::now();
    server.stop();
    let server_stop = t0.elapsed();
    assert!(
        client_stop < Duration::from_millis(200),
        "Drop клиента: {client_stop:?}"
    );
    assert!(
        server_stop < Duration::from_millis(200),
        "stop() сервера: {server_stop:?}"
    );

    // Клиент без сервера спит в паузе между попытками -- Drop будит его.
    let orphan = MultiClient::connect(
        &unique("PROMPT_NOSERVER"),
        Arc::new(ClientRecorder::default()),
        MultiClientOptions {
            retry_delay: Duration::from_secs(30),
            ..MultiClientOptions::default()
        },
    )
    .unwrap();
    thread::sleep(Duration::from_millis(100));
    let t0 = Instant::now();
    drop(orphan);
    assert!(
        t0.elapsed() < Duration::from_millis(200),
        "Drop в паузе ждёт тик"
    );
}

/// Сообщений больше, чем `recv_batch`, за один сигнал `DATA`: сервер
/// дочитывает всё сам (нулевым ожиданием), без таймера.
#[test]
fn backlog_beyond_recv_batch_is_delivered_without_tick() {
    let name = unique("BACKLOG");
    let handler = Arc::new(TestHandler::new());
    let server = MultiServer::start(
        &name,
        handler.clone(),
        MultiOptions {
            max_clients: 1,
            recv_batch: 4,
            ..MultiOptions::default()
        },
    )
    .expect("start");
    let client = MultiClient::connect(
        &name,
        Arc::new(ClientRecorder::default()),
        MultiClientOptions::default(),
    )
    .unwrap();
    wait_for("подключение", Duration::from_secs(5), || {
        server.client_count() == 1
    });
    for i in 0..200u32 {
        client.send(&i.to_le_bytes()).unwrap();
    }
    wait_for(
        "все 200 сообщений",
        Duration::from_secs(5),
        || handler.messages.load(Ordering::Acquire) == 200,
    );
    drop(client);
    server.stop();
}

/// `disconnect_client`: клиент узнаёт об отключении (сигнал не теряется,
/// даже если его поглотил спящий worker сервера), сервер -- `Local`, слот
/// снова в обороте (клиент переподключается).
#[test]
fn disconnect_client_notifies_client_and_frees_slot() {
    let name = unique("KICK");
    let handler = Arc::new(TestHandler::new());
    let server = MultiServer::start(&name, handler.clone(), single_slot(1)).expect("start");
    let recorder = Arc::new(ClientRecorder::default());
    let client = MultiClient::connect(
        &name,
        recorder.clone(),
        MultiClientOptions {
            retry_delay: Duration::from_millis(50),
            ..MultiClientOptions::default()
        },
    )
    .unwrap();
    wait_for("подключение", Duration::from_secs(5), || {
        server.client_count() == 1
    });

    server.disconnect_client(0).unwrap();
    assert_eq!(
        *handler.last_reason.lock().unwrap(),
        Some(DisconnectReason::Local)
    );
    wait_for(
        "клиент узнал об отключении",
        Duration::from_secs(2),
        || !recorder.reasons.lock().unwrap().is_empty(),
    );
    assert_eq!(
        recorder.reasons.lock().unwrap()[0],
        DisconnectReason::Graceful
    );
    wait_for(
        "переподключение",
        Duration::from_secs(5),
        || recorder.connects.load(Ordering::Acquire) == 2 && server.client_count() == 1,
    );
    drop(client);
    server.stop();
}

/// Слоты за прежней границей группы worker-а (21 слот до 0.10) обслуживаются
/// так же: 24 клиента на 31 слоте, каждый обменивается сообщением.
#[test]
fn clients_across_two_worker_groups() {
    /// Слотов в группе worker-а до 0.10 (`SLOTS_PER_WORKER`, удалён).
    const SLOTS_PER_WORKER: u32 = 21;
    let name = unique("GROUPS");
    let handler = Arc::new(TestHandler::new());
    let server = MultiServer::start(&name, handler.clone(), single_slot(31)).expect("start");
    const N: usize = 24;
    let recorders: Vec<Arc<ClientRecorder>> = (0..N)
        .map(|_| Arc::new(ClientRecorder::default()))
        .collect();
    let clients: Vec<MultiClient> = recorders
        .iter()
        .map(|r| MultiClient::connect(&name, r.clone(), MultiClientOptions::default()).unwrap())
        .collect();
    wait_for(
        "все подключены",
        Duration::from_secs(10),
        || server.client_count() as usize == N,
    );
    assert!(
        server
            .connected_clients()
            .iter()
            .any(|&id| id >= SLOTS_PER_WORKER),
        "вторая группа слотов не задействована"
    );
    for client in &clients {
        client.send(b"hi").unwrap();
    }
    wait_for(
        "сообщения от всех",
        Duration::from_secs(5),
        || handler.messages.load(Ordering::Acquire) as usize == N,
    );
    assert_eq!(server.broadcast(b"all").unwrap() as usize, N);
    wait_for("broadcast всем", Duration::from_secs(5), || {
        recorders
            .iter()
            .all(|r| r.messages.load(Ordering::Acquire) == 1)
    });
    drop(clients);
    server.stop();
}

/// Находка 24: слоты за прежним пределом 31 доступны клиенту -- он
/// перебирает слоты до первого отсутствующего (не дальше 256).
#[test]
fn client_claims_slot_beyond_31() {
    let name = unique("BEYOND31");
    let server =
        MultiServer::start(&name, Arc::new(TestHandler::new()), single_slot(40)).expect("start");
    {
        let slots = server.slots.read().unwrap();
        for slot in slots.iter().take(39) {
            let slot = slot.lock().unwrap();
            let claim = &slot.server.view().control_block().slot_claim;
            claim
                .compare_exchange(CLAIM_FREE, 0xB0B0_0001, Ordering::AcqRel, Ordering::Acquire)
                .expect("слот свободен");
        }
    }
    let client = MultiClient::connect(
        &name,
        Arc::new(ClientRecorder::default()),
        MultiClientOptions::default(),
    )
    .unwrap();
    wait_for("подключение", Duration::from_secs(5), || {
        client.is_connected()
    });
    assert_eq!(client.slot_id(), 39);
    drop(client);
    server.stop();
}

/// Порча `slot_count` (враждебный пир): поле -- только подсказка, перебор
/// им не ограничивается. Мусор не даёт паники и выхода за границы; все
/// слоты заняты -- `NoFreeSlot`, сервера нет -- `NotConnected`.
#[test]
fn corrupted_slot_count_is_ignored() {
    let name = unique("SLOTCOUNT");
    let server =
        MultiServer::start(&name, Arc::new(TestHandler::new()), single_slot(2)).expect("start");
    stop_workers(&server);
    {
        let slots = server.slots.read().unwrap();
        for slot in slots.iter() {
            let slot = slot.lock().unwrap();
            let control = slot.server.view().control_block();
            assert_eq!(control.slot_count.load(Ordering::Acquire), 2);
            control.slot_claim.store(0xB0B0_0002, Ordering::Release);
            control.slot_count.store(u32::MAX, Ordering::Release);
        }
    }
    assert_eq!(
        claim_free_slot(None, &name).unwrap_err(),
        ShmError::NoFreeSlot
    );
    nudge_stale_claims(None, &name); // не паникует на испорченном числе
    assert_eq!(
        claim_free_slot(None, &unique("NO_SERVER")).unwrap_err(),
        ShmError::NotConnected
    );
    server.stop();
}

/// Безопасность (раунд 1): любой клиент может записать `slot_count = 1` в
/// слот 0. Если бы перебор ограничивался этим полем, остальные клиенты
/// видели бы только слот 0 -- постоянный отказ в обслуживании. Перебор
/// идёт до первого отсутствующего слота: клиент всё равно получает слот 39.
#[test]
fn slot_count_hint_does_not_bound_the_scan() {
    let name = unique("SLOTHINT");
    let server =
        MultiServer::start(&name, Arc::new(TestHandler::new()), single_slot(40)).expect("start");
    {
        let slots = server.slots.read().unwrap();
        for slot in slots.iter().take(39) {
            let slot = slot.lock().unwrap();
            let control = slot.server.view().control_block();
            control
                .slot_claim
                .compare_exchange(CLAIM_FREE, 0xB0B0_0003, Ordering::AcqRel, Ordering::Acquire)
                .expect("слот свободен");
            control.slot_count.store(1, Ordering::Release);
        }
    }
    let client = MultiClient::connect(
        &name,
        Arc::new(ClientRecorder::default()),
        MultiClientOptions::default(),
    )
    .unwrap();
    wait_for("подключение", Duration::from_secs(5), || {
        client.is_connected()
    });
    assert_eq!(client.slot_id(), 39);
    drop(client);
    server.stop();
}

/// Регрессия (ревизия 2, дефект 2): `disconnect_client` освобождал claim
/// раньше, чем клиент A получал `DISCONNECT`; клиент B захватывал слот, его
/// `complete_handshake` сбрасывал `DISCONNECT` A -- два клиента на одном
/// SPSC-кольце. Теперь claim держится, пока A сам его не снимет: B не может
/// захватить слот, сигнал A цел; после ухода A слот снова в обороте.
#[test]
fn disconnect_client_keeps_claim_until_the_client_leaves() {
    const TOKEN_A: u32 = 0xA11C_E001;
    const TOKEN_B: u32 = 0xB0B0_0001;
    let name = unique("KEEPCLAIM");
    let server =
        MultiServer::start(&name, Arc::new(TestHandler::new()), single_slot(1)).expect("start");
    let slot_name = format!("{name}_0");
    for round in 0..20 {
        assert!(
            try_claim_slot(None, &slot_name, TOKEN_A).unwrap(),
            "раунд {round}"
        );
        let mut a = connect_held(&slot_name, TOKEN_A).unwrap();
        wait_for("подключение A", Duration::from_secs(5), || {
            server.client_count() == 1
        });

        server.disconnect_client(0).unwrap();
        // Гонка из дефекта: B пытается захватить слот сразу после отключения.
        assert!(
            !try_claim_slot(None, &slot_name, TOKEN_B).unwrap(),
            "раунд {round}: слот освобождён раньше, чем A узнал об отключении"
        );
        assert!(
            a.events()
                .disconnect
                .wait(Some(Duration::from_secs(2)))
                .unwrap(),
            "раунд {round}: DISCONNECT A потерян"
        );
        // A уходит так, как это делает MultiClient: без встречного
        // DISCONNECT, затем снимает свой claim.
        a.mark_disconnected();
        drop(a);
        release_claim(None, &slot_name, TOKEN_A);

        assert!(
            try_claim_slot(None, &slot_name, TOKEN_B).unwrap(),
            "раунд {round}"
        );
        let b = connect_held(&slot_name, TOKEN_B).unwrap();
        wait_for("подключение B", Duration::from_secs(5), || {
            server.client_count() == 1
        });
        assert!(b.is_session_current());
        drop(b); // штатный уход B: DISCONNECT -> сервер снимает claim CAS-ом
        release_claim(None, &slot_name, TOKEN_B);
        wait_for("B отключён", Duration::from_secs(5), || {
            server.client_count() == 0
        });
    }
    server.stop();
}

/// Клиент, отключённый `disconnect_client` и так и не проснувшийся
/// (мёртвый процесс без наблюдаемого handle), держит слот только до
/// дедлайна `reserve_timeout` -- как любой протухший захват.
#[test]
fn claim_of_a_kicked_client_that_never_leaves_is_released_at_deadline() {
    let name = unique("KICKDEAD");
    let reserve = Duration::from_millis(300);
    let server =
        MultiServer::start_with(&name, Arc::new(TestHandler::new()), single_slot(1), reserve)
            .expect("start");
    let slot_name = format!("{name}_0");
    assert!(try_claim_slot(None, &slot_name, 0xDEAD_0001).unwrap());
    let client = connect_held(&slot_name, 0xDEAD_0001).unwrap();
    wait_for("подключение", Duration::from_secs(5), || {
        server.client_count() == 1
    });
    server.disconnect_client(0).unwrap();
    let t0 = Instant::now();
    let claim_of_slot0 = || server.slots.read().unwrap()[0].lock().unwrap().claim();
    assert_ne!(claim_of_slot0(), CLAIM_FREE);
    wait_for(
        "claim снят по дедлайну",
        Duration::from_secs(3),
        || claim_of_slot0() == CLAIM_FREE,
    );
    assert!(t0.elapsed() >= reserve - Duration::from_millis(20));
    // «Мёртвый» клиент так и не узнал об отключении -- просто исчезает.
    let mut client = client;
    client.mark_disconnected();
    drop(client);
    server.stop();
}

/// Регрессия (ревизия 2, дефект 6): `MultiServer::stop()` сигналит
/// `DISCONNECT` подключённым клиентам сразу, а не когда отпустят последний
/// `Arc` сервера.
#[test]
fn stop_disconnects_clients_while_the_server_arc_is_alive() {
    let name = unique("STOPDISC");
    let server =
        MultiServer::start(&name, Arc::new(TestHandler::new()), single_slot(2)).expect("start");
    let keep_alive = Arc::clone(&server);
    let recorder = Arc::new(ClientRecorder::default());
    let client = MultiClient::connect(
        &name,
        recorder.clone(),
        MultiClientOptions {
            retry_delay: Duration::from_secs(30),
            ..MultiClientOptions::default()
        },
    )
    .unwrap();
    wait_for("подключение", Duration::from_secs(5), || {
        client.is_connected()
    });
    server.stop();
    wait_for(
        "клиент узнал об остановке",
        Duration::from_secs(2),
        || !recorder.reasons.lock().unwrap().is_empty(),
    );
    assert_eq!(
        recorder.reasons.lock().unwrap()[0],
        DisconnectReason::Graceful
    );
    assert!(!client.is_connected());
    assert_eq!(keep_alive.client_count(), 0);
    drop(client);
    drop(keep_alive);
}

/// Ревизия 2, дефект 4: Drop клиента, ждущего `S2C_CONNECT` слота (сервер
/// слота не принимает), возвращается сразу, а не через `slot_timeout`;
/// захват снят.
#[test]
fn drop_during_slot_handshake_is_prompt() {
    let name = unique("DROPCONN");
    let slot_name = format!("{name}_0");
    // «Сервер» из одного слота без worker-а: заявку никто не примет.
    let slot = SharedServer::start(&slot_name).unwrap();
    let control = slot.view().control_block();
    // Лучшая из трёх попыток: всплеск планировщика под нагрузкой полного
    // прогона -- не то, что проверяется; проверяется, что Drop не ждёт
    // `slot_timeout` (8 с).
    let mut best = Duration::MAX;
    for _ in 0..3 {
        let client = MultiClient::connect(
            &name,
            Arc::new(ClientRecorder::default()),
            MultiClientOptions {
                slot_timeout: Duration::from_secs(8),
                ..MultiClientOptions::default()
            },
        )
        .unwrap();
        wait_for("заявка подана", Duration::from_secs(5), || {
            control.client_state.load(Ordering::Acquire) == crate::constants::HANDSHAKE_CLIENT_HELLO
        });
        let t0 = Instant::now();
        drop(client);
        let took = t0.elapsed();
        assert!(
            took < Duration::from_secs(1),
            "Drop ждал рукопожатия: {took:?}"
        );
        best = best.min(took);
        assert_eq!(
            control.slot_claim.load(Ordering::Acquire),
            CLAIM_FREE,
            "захват не снят"
        );
        assert_eq!(
            control.client_state.load(Ordering::Acquire),
            crate::constants::HANDSHAKE_IDLE
        );
    }
    assert!(
        best < Duration::from_millis(200),
        "лучшая из трёх: {best:?}"
    );
}

/// Сколько рабочих потоков xshm стартовало с хуком теста
/// `two_hundred_fifty_six_slots_one_thread`.
static STARTED_THREADS: AtomicUsize = AtomicUsize::new(0);

fn count_thread_start() {
    STARTED_THREADS.fetch_add(1, Ordering::SeqCst);
}

/// Task 17: сервер на 256 слотов -- один поток-диспетчер (до 0.10 --
/// 13 worker-ов по 21 слоту). Потоки считает хук старта рабочих потоков
/// (`set_thread_start_hook`), поставленный на потоке теста: его получают
/// только потоки, созданные отсюда, -- параллельные тесты счёт не портят.
/// Слот 255 обслуживается тем же потоком: клиент подключается к нему и
/// обменивается сообщениями.
#[test]
fn two_hundred_fifty_six_slots_one_thread() {
    let name = unique("SLOTS256");
    let handler = Arc::new(TestHandler::new());
    crate::thread_hook::set_thread_start_hook(Some(count_thread_start));
    let before = STARTED_THREADS.load(Ordering::SeqCst);
    let server = MultiServer::start(
        &name,
        handler.clone(),
        MultiOptions {
            max_clients: MAX_MULTI_CLIENTS,
            layout: ChannelLayout::symmetric(crate::layout::RingConfig::new(4096)),
            ..MultiOptions::default()
        },
    )
    .expect("start 256");
    crate::thread_hook::set_thread_start_hook(None);
    // Хук зовётся первой строкой нового потока -- дать ему стартовать.
    wait_for(
        "старт диспетчера",
        Duration::from_secs(5),
        || STARTED_THREADS.load(Ordering::SeqCst) > before,
    );
    thread::sleep(Duration::from_millis(200));
    let grown = STARTED_THREADS.load(Ordering::SeqCst) - before;
    assert!(grown <= 2, "256 слотов -- {grown} потоков");
    assert_eq!(grown, 1, "один поток-диспетчер");

    // Слоты 0..255 заняты захватами -- клиент получит последний слот.
    for slot_id in 0..MAX_MULTI_CLIENTS - 1 {
        assert!(try_claim_slot(None, &format!("{name}_{slot_id}"), 0xC1A1_1000 + slot_id).unwrap());
    }
    let recorder = Arc::new(ClientRecorder::default());
    let client = MultiClient::connect(&name, recorder.clone(), MultiClientOptions::default())
        .expect("client");
    wait_for(
        "подключение к слоту 255",
        Duration::from_secs(5),
        || client.is_connected(),
    );
    assert_eq!(client.slot_id(), MAX_MULTI_CLIENTS - 1);
    client.send(b"ping").unwrap();
    wait_for(
        "сообщение со слота 255",
        Duration::from_secs(2),
        || handler.messages.load(Ordering::Acquire) == 1,
    );
    server.send_to(client.slot_id(), b"pong").unwrap();
    wait_for(
        "ответ на слот 255",
        Duration::from_secs(2),
        || recorder.messages.load(Ordering::Acquire) == 1,
    );
    drop(client);
    server.stop();
}

/// Обработчик с медленным слотом: `on_message` слота `slow` спит 50 мс.
struct SlowSlotHandler {
    slow: AtomicU32,
    slow_done: AtomicU32,
    /// Первое сообщение другого слота: когда и сколько медленных к тому
    /// моменту обработано.
    fast_seen: Mutex<Option<(Instant, u32)>>,
}

impl MultiHandler for SlowSlotHandler {
    fn on_client_connect(&self, _client_id: u32) {}
    fn on_client_disconnect(&self, _client_id: u32) {}
    fn on_message(&self, client_id: u32, _data: &[u8]) {
        if client_id == self.slow.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(50));
            self.slow_done.fetch_add(1, Ordering::AcqRel);
        } else {
            let mut seen = self.fast_seen.lock().unwrap();
            if seen.is_none() {
                *seen = Some((Instant::now(), self.slow_done.load(Ordering::Acquire)));
            }
        }
    }
}

/// Task 17, честная ротация: медленный `on_message` слота A (50 мс на
/// сообщение, 40 сообщений -- 2 с работы) не задерживает сообщение слота B
/// дольше пары кругов: за круг -- не больше `recv_batch` (2) сообщений слота.
#[test]
fn slow_slot_does_not_stall_other_slots() {
    let name = unique("FAIR");
    let handler = Arc::new(SlowSlotHandler {
        slow: AtomicU32::new(u32::MAX),
        slow_done: AtomicU32::new(0),
        fast_seen: Mutex::new(None),
    });
    let server = MultiServer::start(
        &name,
        handler.clone(),
        MultiOptions {
            max_clients: 4,
            recv_batch: 2,
            ..MultiOptions::default()
        },
    )
    .expect("start");
    let slow = MultiClient::connect(
        &name,
        Arc::new(ClientRecorder::default()),
        MultiClientOptions::default(),
    )
    .unwrap();
    wait_for(
        "медленный клиент",
        Duration::from_secs(5),
        || slow.is_connected(),
    );
    handler.slow.store(slow.slot_id(), Ordering::Release);
    let fast = MultiClient::connect(
        &name,
        Arc::new(ClientRecorder::default()),
        MultiClientOptions::default(),
    )
    .unwrap();
    wait_for(
        "быстрый клиент",
        Duration::from_secs(5),
        || fast.is_connected() && server.client_count() == 2,
    );

    for i in 0..40u32 {
        slow.send(&i.to_le_bytes()).unwrap();
    }
    wait_for(
        "медленный слот в работе",
        Duration::from_secs(5),
        || handler.slow_done.load(Ordering::Acquire) >= 1,
    );
    let t0 = Instant::now();
    fast.send(b"fast").unwrap();
    wait_for(
        "сообщение быстрого слота",
        Duration::from_secs(5),
        || handler.fast_seen.lock().unwrap().is_some(),
    );
    let (at, slow_before) = handler.fast_seen.lock().unwrap().unwrap();
    let latency = at - t0;
    assert!(
        latency < Duration::from_millis(1000),
        "быстрый слот ждал {latency:?}"
    );
    assert!(
        slow_before < 20,
        "быстрый слот ждал почти всю очередь медленного: {slow_before} из 40"
    );
    drop(fast);
    drop(slow);
    server.stop();
}

/// Task 17, устаревшее завершение: `DISCONNECT` первой сессии извлечён из
/// порта, но не обработан, а слот тем временем отключён и подключён снова
/// (новая привязка `DISCONNECT`). Старое завершение отбрасывается по
/// поколению -- новую сессию оно не отключает; поглощённый сигнал при
/// снятии привязки возвращается; своё `DISCONNECT` новая сессия получает.
///
/// Диспетчер ведётся вручную на ядре остановленного сервера (порт жив,
/// потока нет), смена сессий -- флагом `connected` под блокировкой слота.
#[test]
fn stale_completion_is_not_delivered_to_new_session() {
    let name = unique("STALE_COMPLETION");
    let handler = Arc::new(TestHandler::new());
    let server = MultiServer::start(&name, handler.clone(), single_slot(1)).expect("start");
    stop_workers(&server);
    let set_connected = |connected: bool| {
        server.slots.read().unwrap()[0].lock().unwrap().connected = connected;
    };
    let disconnect_set = || {
        let slots = server.slots.read().unwrap();
        let slot = slots[0].lock().unwrap();
        slot.server.events().unwrap().disconnect.set().unwrap();
    };
    let disconnect_signalled = || {
        let slots = server.slots.read().unwrap();
        let slot = slots[0].lock().unwrap();
        slot.server
            .events()
            .unwrap()
            .disconnect
            .wait(Some(Duration::ZERO))
            .unwrap()
    };
    // Пост остановки мог остаться в порту (диспетчер вышел по `running`, не
    // дойдя до ожидания) -- убрать.
    let mut leftover = Vec::new();
    server
        .port
        .wait_batch(&mut leftover, Some(Duration::ZERO))
        .unwrap();
    assert!(leftover.iter().all(|c| c.key == WAKE_KEY));
    let mut dispatcher = Dispatcher::new(&server.core);
    dispatcher.reconcile(0); // свободный слот: CONNECT_REQ
    set_connected(true);
    dispatcher.reconcile(0); // сессия 1: DISCONNECT, DATA

    // Сессия 1: DISCONNECT сработал, завершение извлечено, не обработано.
    disconnect_set();
    let mut old = Vec::new();
    server
        .port
        .wait_batch(&mut old, Some(Duration::from_secs(1)))
        .unwrap();
    assert_eq!(old.len(), 1);
    assert_eq!(old[0].key, WaitKind::Disconnect.key(0));

    // Отключение и новая сессия на том же слоте.
    set_connected(false);
    dispatcher.reconcile(0);
    assert!(
        disconnect_signalled(),
        "поглощённый снятой привязкой DISCONNECT не возвращён"
    );
    // (`wait` выше и погасил сигнал -- как сброс DISCONNECT рукопожатием.)
    set_connected(true);
    dispatcher.reconcile(0); // сессия 2: новая привязка DISCONNECT

    let stale = server.stale_completions.load(Ordering::Relaxed);
    dispatcher.complete(old[0]);
    assert_eq!(
        server.stale_completions.load(Ordering::Relaxed),
        stale + 1,
        "старое завершение не отброшено"
    );
    assert!(server.slots.read().unwrap()[0].lock().unwrap().connected);
    assert_eq!(handler.disconnects.load(Ordering::Acquire), 0);

    // Своё DISCONNECT новая сессия получает.
    disconnect_set();
    let mut fresh = Vec::new();
    server
        .port
        .wait_batch(&mut fresh, Some(Duration::from_secs(1)))
        .unwrap();
    assert_eq!(fresh.len(), 1);
    assert_ne!(fresh[0].tag, old[0].tag, "поколение не сменилось");
    dispatcher.complete(fresh[0]);
    assert!(!server.slots.read().unwrap()[0].lock().unwrap().connected);
    assert_eq!(handler.disconnects.load(Ordering::Acquire), 1);
    assert_eq!(
        *handler.last_reason.lock().unwrap(),
        Some(DisconnectReason::Graceful)
    );
    dispatcher.release_all();
}

/// Журнал сервера по слотам (тесты враждебного клиента и паники колбэка).
#[derive(Default)]
struct SlotJournal {
    /// Сообщения по слотам 0..4.
    messages: [AtomicU32; 4],
    disconnects: Mutex<Vec<(u32, DisconnectReason, Instant)>>,
    errors: Mutex<Vec<(Option<u32>, ShmError)>>,
    /// Слот, чей `on_message` паникует (`u32::MAX` -- никакой).
    panic_slot: AtomicU32,
    /// Слот, чей `on_message` медленный (1 мс; `u32::MAX` -- никакой).
    slow_slot: AtomicU32,
    /// Первые 4 байта каждого сообщения (LE) по слотам.
    seqs: Mutex<Vec<(u32, u32)>>,
}

impl SlotJournal {
    fn new() -> Self {
        Self {
            panic_slot: AtomicU32::new(u32::MAX),
            slow_slot: AtomicU32::new(u32::MAX),
            ..Self::default()
        }
    }

    fn count(&self, slot: u32) -> u32 {
        self.messages[slot as usize].load(Ordering::Acquire)
    }

    fn disconnect_of(&self, slot: u32) -> Option<(DisconnectReason, Instant)> {
        self.disconnects
            .lock()
            .unwrap()
            .iter()
            .find(|(id, _, _)| *id == slot)
            .map(|&(_, reason, at)| (reason, at))
    }
}

impl MultiHandler for SlotJournal {
    fn on_client_connect(&self, _client_id: u32) {}
    fn on_client_disconnect(&self, _client_id: u32) {}
    fn on_client_disconnect_reason(&self, client_id: u32, reason: DisconnectReason) {
        self.disconnects
            .lock()
            .unwrap()
            .push((client_id, reason, Instant::now()));
    }
    fn on_message(&self, client_id: u32, data: &[u8]) {
        assert!(
            client_id != self.panic_slot.load(Ordering::Acquire),
            "тестовая паника колбэка"
        );
        if client_id == self.slow_slot.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(1));
        }
        if let Some(seq) = data.first_chunk::<4>() {
            self.seqs
                .lock()
                .unwrap()
                .push((client_id, u32::from_le_bytes(*seq)));
        }
        self.messages[client_id as usize].fetch_add(1, Ordering::AcqRel);
    }
    fn on_error(&self, client_id: Option<u32>, err: ShmError) {
        self.errors.lock().unwrap().push((client_id, err));
    }
}

/// Fix round 1, п. 1: враждебный клиент сигналит `DISCONNECT` и продолжает
/// писать в тесном цикле. Дочитывание ограничено снимком `write_pos` в
/// момент обнаружения: колбэк отключения приходит за ограниченное время, а
/// другой слот всё это время обслуживается. До исправления `drain_slot`
/// читал, пока пачки выбирались целиком, -- писатель, успевающий за
/// читателем, держал единственный поток-диспетчер вечно.
///
/// Task 18 (Task 17): поток идёт через `try_send_to_server` -- запись без
/// перезаписи `read_pos` не трогает, поэтому дочитывание всегда доходит
/// до снимка и причина -- строго `Graceful` (с перезаписью писатель мог
/// сдвинуть `read_pos` посреди дочитывания, и тест принимал досрочный
/// `Corrupted`/`Error`, не проверяя границу).
#[test]
fn hostile_disconnect_flood_does_not_stall_dispatcher() {
    let name = unique("HOSTILE_DRAIN");
    let journal = Arc::new(SlotJournal::new());
    let server = MultiServer::start(
        &name,
        journal.clone(),
        MultiOptions {
            max_clients: 4,
            // Маленький лимит сообщений: дочитывание до снимка -- не больше
            // 65 кадров по 1 мс.
            layout: ChannelLayout::symmetric(
                crate::layout::RingConfig::new(64 << 10).max_messages(64),
            ),
            ..MultiOptions::default()
        },
    )
    .expect("start");
    // Честный клиент -- слот 0.
    let honest = MultiClient::connect(
        &name,
        Arc::new(ClientRecorder::default()),
        MultiClientOptions::default(),
    )
    .unwrap();
    wait_for(
        "честный клиент",
        Duration::from_secs(5),
        || honest.is_connected() && honest.slot_id() == 0,
    );

    // Враждебный клиент -- слот 1: захват, рукопожатие, 50 сообщений, затем
    // DISCONNECT и поток записи без остановки. Колбэк слота 1 медленный
    // (1 мс): поток всегда опережает читателя, и без границы дочитывание
    // выбирало бы полные пачки вечно.
    const BEFORE: u32 = 50;
    let slot_name = format!("{name}_1");
    assert!(try_claim_slot(None, &slot_name, 0xBAD0_0001).unwrap());
    let flooding = Arc::new(AtomicBool::new(true));
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let flooder = {
        let flooding = flooding.clone();
        thread::spawn(move || {
            let client = connect_held(&slot_name, 0xBAD0_0001).unwrap();
            for _ in 0..BEFORE {
                client.try_send_to_server(b"before").unwrap();
            }
            go_rx.recv().unwrap();
            // Кольцо полно кадрами потока ДО `DISCONNECT`: снимок дочитывания
            // гарантированно непуст, тест не проходит «вхолостую», даже если
            // диспетчер, разбуженный сигналом, вытеснит флудер до его первой
            // записи после сигнала (а после закрытия слота клиент уже не
            // пишет -- Task 6 надёжности).
            let mut written = 0u64;
            while client.try_send_to_server(b"flood-flood-flood").is_ok() {
                written += 1;
            }
            let prefilled = written;
            client.events().disconnect.set().unwrap();
            ready_tx.send((Instant::now(), prefilled)).unwrap();
            while flooding.load(Ordering::Acquire) {
                // Без перезаписи: `QueueFull`, пока читатель не освободит
                // место, -- `read_pos` двигает только он.
                if client.try_send_to_server(b"flood-flood-flood").is_ok() {
                    written += 1;
                }
            }
            written
        })
    };
    wait_for(
        "сообщения до DISCONNECT",
        Duration::from_secs(5),
        || journal.count(1) == BEFORE,
    );
    journal.slow_slot.store(1, Ordering::Release);
    go_tx.send(()).unwrap();
    let (signalled_at, prefilled) = ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    // Медленный читатель (1 мс на кадр) не успевает за записью: кольцо
    // заполнено до лимита сообщений (64, +1 по правилу пересчёта).
    assert!(
        prefilled >= 64,
        "кольцо не заполнено до DISCONNECT: {prefilled}"
    );

    // Пока идёт поток, честный слот обслуживается.
    let before = journal.count(0);
    for i in 0..20u32 {
        honest.send(&i.to_le_bytes()).unwrap();
        thread::sleep(Duration::from_millis(10));
    }
    wait_for(
        "честный слот обслуживается во время потока",
        Duration::from_secs(3),
        || journal.count(0) >= before + 20,
    );
    wait_for(
        "колбэк отключения враждебного слота",
        Duration::from_secs(3),
        || journal.disconnect_of(1).is_some(),
    );
    let (reason, at) = journal.disconnect_of(1).unwrap();
    // Дочитано до снимка: писатель без перезаписи `read_pos` не трогает,
    // досрочного `Corrupted` (`Error`) быть не может.
    assert_eq!(reason, DisconnectReason::Graceful);
    assert!(
        journal.errors.lock().unwrap().is_empty(),
        "ошибки: {:?}",
        journal.errors.lock().unwrap()
    );
    assert!(
        at.saturating_duration_since(signalled_at) < Duration::from_secs(2),
        "отключение через {:?}",
        at.saturating_duration_since(signalled_at)
    );
    assert!(!flooder.is_finished(), "поток шёл всё это время");

    flooding.store(false, Ordering::Release);
    let written = flooder.join().unwrap();
    // После обнаружения -- не больше кадров снимка (лимит кольца 64 + 1),
    // плюс обычные пачки, прочитанные до разбора DISCONNECT; поток
    // (`written` -- обычно сотни тысяч кадров) не доставлен. `<=`, а не
    // `<`: после закрытия слота (`server_state = IDLE`) клиент больше не
    // пишет (Task 6 надёжности), и если после снимка дочитывания флудер
    // не опубликовал ни одного кадра (диспетчер, разбуженный `DISCONNECT`,
    // вытеснил его до закрытия слота), доставлено ровно всё записанное --
    // это лучший исход, граница доставки от этого не меняется. Поток
    // дошёл до дочитывания: кольцо было полно кадрами потока к сигналу.
    let flood_delivered = journal.count(1) - BEFORE;
    assert!(flood_delivered > 0, "дочитывание не состоялось");
    assert!(
        flood_delivered < 300 && u64::from(flood_delivered) <= written,
        "доставлено из потока {flood_delivered} из {written}"
    );
    drop(honest);
    server.stop();
}

/// Fix round 1, п. 2 (решение 26): паника `on_message` слота A
/// перехвачена -- `on_error(Some(A), WorkerPanicked)`, слот A отключён
/// локально, слот B обслуживается дальше.
#[test]
fn panic_in_callback_is_isolated_to_its_slot() {
    let name = unique("PANIC_ISOLATED");
    let journal = Arc::new(SlotJournal::new());
    let server = MultiServer::start(
        &name,
        journal.clone(),
        MultiOptions {
            max_clients: 4,
            ..MultiOptions::default()
        },
    )
    .expect("start");
    let a = MultiClient::connect(
        &name,
        Arc::new(ClientRecorder::default()),
        MultiClientOptions::default(),
    )
    .unwrap();
    wait_for("клиент A", Duration::from_secs(5), || {
        a.is_connected()
    });
    let b = MultiClient::connect(
        &name,
        Arc::new(ClientRecorder::default()),
        MultiClientOptions::default(),
    )
    .unwrap();
    wait_for("клиент B", Duration::from_secs(5), || {
        b.is_connected() && server.client_count() == 2
    });
    let (slot_a, slot_b) = (a.slot_id(), b.slot_id());
    journal.panic_slot.store(slot_a, Ordering::Release);

    a.send(b"boom").unwrap();
    wait_for("on_error слота A", Duration::from_secs(5), || {
        !journal.errors.lock().unwrap().is_empty()
    });
    assert_eq!(
        journal.errors.lock().unwrap()[0],
        (
            Some(slot_a),
            ShmError::WorkerPanicked {
                message: "тестовая паника колбэка"
            }
        )
    );
    wait_for(
        "слот A отключён",
        Duration::from_secs(5),
        || journal.disconnect_of(slot_a).is_some(),
    );
    assert_eq!(
        journal.disconnect_of(slot_a).unwrap().0,
        DisconnectReason::Local
    );

    // Слот B жив.
    for _ in 0..5 {
        b.send(b"still-alive").unwrap();
    }
    wait_for(
        "сообщения слота B",
        Duration::from_secs(5),
        || journal.count(slot_b) == 5,
    );
    assert!(server.is_client_connected(slot_b));
    drop(a);
    drop(b);
    server.stop();
}

/// Fix round 1, п. 3: отказ привязки (инъекция `fail_arms`) повторяется
/// через `REARM_BACKOFF`, а не навсегда оставляет слот глухим: после
/// `disconnect_client` привязка `CONNECT_REQ` свободного слота проваливается
/// один раз, клиент всё равно переподключается; ошибка сообщена один раз.
#[test]
fn failed_rearm_is_retried() {
    let name = unique("REARM");
    let handler = Arc::new(TestHandler::new());
    let server = MultiServer::start(&name, handler.clone(), single_slot(1)).expect("start");
    let recorder = Arc::new(ClientRecorder::default());
    let client = MultiClient::connect(
        &name,
        recorder.clone(),
        MultiClientOptions {
            retry_delay: Duration::from_millis(50),
            ..MultiClientOptions::default()
        },
    )
    .unwrap();
    wait_for("подключение", Duration::from_secs(5), || {
        server.client_count() == 1
    });

    server.core.fail_arms.store(1, Ordering::Release);
    server.disconnect_client(0).unwrap();
    wait_for(
        "переподключение после отказа привязки",
        Duration::from_secs(5),
        || recorder.connects.load(Ordering::Acquire) == 2 && server.client_count() == 1,
    );
    assert_eq!(server.core.fail_arms.load(Ordering::Acquire), 0);
    let errors = handler.errors.lock().unwrap().clone();
    assert_eq!(
        errors,
        vec![(Some(0), ShmError::InvalidConfig("тестовый отказ привязки"))]
    );
    drop(client);
    server.stop();
}

/// Сервер без потока-диспетчера (остановлен), слот 0 захвачен token-ом
/// `claim` и подключён `SharedClient`-ом через диспетчер, ведомый вручную
/// на потоке теста. Возвращает клиента; сервер -- в `server`.
/// Рукопожатие клиента Multi по уже захваченному claim слота
/// (`try_claim_slot` с `token`), как у `MultiClient`: ревизия 3 --
/// `ClaimMode::Held` (`SharedClient::connect` с `Take` получил бы
/// `AlreadyConnected` -- claim уже занят нами же).
fn connect_held(slot_name: &str, token: u32) -> Result<SharedClient> {
    SharedClient::connect_impl(
        None,
        slot_name,
        Duration::from_secs(5),
        true,
        ClaimMode::Held(token),
        None,
    )
}

fn connect_manually(server: &MultiServer, name: &str, claim: u32) -> SharedClient {
    stop_workers(server);
    // Потока нет -- колбэки зовёт тест; `running` нужен доставке сообщений.
    server.running.store(true, Ordering::Release);
    let mut leftover = Vec::new();
    server
        .port
        .wait_batch(&mut leftover, Some(Duration::ZERO))
        .unwrap();
    let slot_name = format!("{name}_0");
    assert!(try_claim_slot(None, &slot_name, claim).unwrap());
    let mut dispatcher = Dispatcher::new(&server.core);
    dispatcher.reconcile(0);
    let connecting = thread::spawn(move || connect_held(&slot_name, claim).unwrap());
    let deadline = Instant::now() + Duration::from_secs(5);
    while !server.is_client_connected(0) {
        assert!(Instant::now() < deadline, "рукопожатие вручную");
        let mut batch = Vec::new();
        server
            .port
            .wait_batch(&mut batch, Some(Duration::from_millis(100)))
            .unwrap();
        for completion in batch {
            dispatcher.complete(completion);
        }
    }
    dispatcher.release_all();
    drop(dispatcher);
    connecting.join().unwrap()
}

/// Fix round 2, п. 1: враждебный пир перематывает `read_pos` назад во время
/// дочитывания (кольцо с огромным `max_messages`). Своя позиция чтения
/// замечает сдвиг -- `Corrupted` (Task 7, решение C: только `on_error`,
/// разрыв -- за путём ухода, вызвавшим дочитывание); ни один кадр не
/// доставлен дважды, дочитывание кончается быстро и не дальше физической
/// ёмкости кольца. (Дочитывание идёт на потоке теста вместо диспетчера:
/// пока оно кончается быстро, диспетчер свободен для остальных слотов --
/// это свойство на живом сервере проверяет
/// `hostile_disconnect_flood_does_not_stall_dispatcher`.)
#[test]
fn hostile_rewind_during_drain_is_corrupted_and_delivers_once() {
    const FRAMES: u32 = 200;
    let name = unique("REWIND");
    let journal = Arc::new(SlotJournal::new());
    let server = MultiServer::start(
        &name,
        journal.clone(),
        MultiOptions {
            max_clients: 1,
            layout: ChannelLayout::symmetric(
                crate::layout::RingConfig::new(64 << 10).max_messages(1_000_000),
            ),
            ..MultiOptions::default()
        },
    )
    .expect("start");
    let client = connect_manually(&server, &name, 0xBAD0_0002);
    for seq in 0..FRAMES {
        client.try_send_to_server(&seq.to_le_bytes()).unwrap();
    }
    // Вид враждебного пира на ту же секцию: заголовок кольца клиент -> сервер.
    let mapping = Mapping::open(ObjectPath::mapping(None, &format!("{name}_0"))).unwrap();
    // SAFETY: отображение живо до конца теста, размер -- его собственный.
    let view = unsafe { SharedView::attach(mapping.as_ptr(), mapping.size()) }.unwrap();
    let c2s = view.header_b();
    assert_eq!(c2s.write_pos.load(Ordering::Acquire), FRAMES * 16);
    let start = c2s.read_pos.load(Ordering::Acquire);
    journal.slow_slot.store(0, Ordering::Release);

    let rewinding = AtomicBool::new(true);
    let took = thread::scope(|scope| {
        scope.spawn(|| {
            while rewinding.load(Ordering::Acquire) {
                c2s.read_pos.store(start, Ordering::Release);
                thread::yield_now();
            }
        });
        let t0 = Instant::now();
        server.core.drain_slot(0, &mut Vec::new());
        let took = t0.elapsed();
        rewinding.store(false, Ordering::Release);
        took
    });

    let seqs: Vec<u32> = journal
        .seqs
        .lock()
        .unwrap()
        .iter()
        .map(|&(_, seq)| seq)
        .collect();
    assert!(
        seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "кадр доставлен повторно: {seqs:?}"
    );
    assert!(seqs.len() <= FRAMES as usize);
    // Физический предел: (64 КиБ / 16) + 1 кадров по 1 мс -- секунды; сдвиг
    // замечается после первой же пачки.
    assert!(took < Duration::from_secs(2), "дочитывание {took:?}");
    assert!(
        journal
            .errors
            .lock()
            .unwrap()
            .contains(&(Some(0), ShmError::Corrupted)),
        "перемотка не замечена"
    );
    // Само дочитывание слот не рвёт (решение C): причину и снятие claim-а
    // решает путь ухода, который его вызвал.
    assert_eq!(journal.disconnect_of(0), None);
    assert!(server.is_client_connected(0));
    drop(client);
}

/// Fix round 2, п. 1: лимит дочитывания -- физическая ёмкость кольца, а не
/// `max_messages`.
#[test]
fn drain_cap_is_physical_not_configured() {
    let name = unique("DRAIN_CAP");
    let server = MultiServer::start(
        &name,
        Arc::new(TestHandler::new()),
        MultiOptions {
            max_clients: 1,
            layout: ChannelLayout::symmetric(
                crate::layout::RingConfig::new(4096).max_messages(u32::MAX),
            ),
            ..MultiOptions::default()
        },
    )
    .expect("start");
    let bound = server.slots.read().unwrap()[0]
        .lock()
        .unwrap()
        .server
        .client_drain_snapshot();
    assert_eq!(bound.left, 4096 / 16 + 1);
    server.stop();
}

/// Fix round 2, п. 3: паника `on_message` при дочитывании уходящего
/// клиента (`DISCONNECT`) -- `on_error(WorkerPanicked)`, остаток
/// дочитывания не доставлен, но причина отключения -- исходная `Graceful`,
/// и claim сессии снят обычным путём (без ожидания `RESERVE_TIMEOUT`).
#[test]
fn panic_while_draining_keeps_reason_and_releases_claim() {
    let name = unique("PANIC_DRAIN");
    let journal = Arc::new(SlotJournal::new());
    let server = MultiServer::start(&name, journal.clone(), single_slot(1)).expect("start");
    let client = connect_manually(&server, &name, 0xBAD0_0003);
    for seq in 0..3u32 {
        client.try_send_to_server(&seq.to_le_bytes()).unwrap();
    }
    client.events().disconnect.set().unwrap();
    journal.panic_slot.store(0, Ordering::Release);

    server.core.handle_slot_disconnect(0, &mut Vec::new());

    assert_eq!(journal.count(0), 0, "остаток дочитывания не доставляется");
    assert_eq!(
        journal.errors.lock().unwrap().as_slice(),
        &[(
            Some(0),
            ShmError::WorkerPanicked {
                message: "тестовая паника колбэка"
            }
        )]
    );
    assert_eq!(
        journal.disconnect_of(0).map(|(reason, _)| reason),
        Some(DisconnectReason::Graceful)
    );
    let claim = server.slots.read().unwrap()[0].lock().unwrap().claim();
    assert_eq!(claim, CLAIM_FREE, "claim сессии не снят");
    drop(client);
}

/// Fix round 2, п. 2: паника `on_error(Corrupted)` отбрасывается: второго
/// `on_error` нет, причина разрыва остаётся `Error`.
#[test]
fn panic_in_on_error_keeps_error_reason() {
    /// Обработчик, чей `on_error` паникует.
    #[derive(Default)]
    struct PanickyErrors {
        errors: AtomicU32,
        reasons: Mutex<Vec<DisconnectReason>>,
    }
    impl MultiHandler for PanickyErrors {
        fn on_client_connect(&self, _client_id: u32) {}
        fn on_client_disconnect(&self, _client_id: u32) {}
        fn on_client_disconnect_reason(&self, _client_id: u32, reason: DisconnectReason) {
            self.reasons.lock().unwrap().push(reason);
        }
        fn on_message(&self, _client_id: u32, _data: &[u8]) {}
        fn on_error(&self, _client_id: Option<u32>, _err: ShmError) {
            self.errors.fetch_add(1, Ordering::AcqRel);
            panic!("тестовая паника on_error");
        }
    }

    let name = unique("PANIC_ON_ERROR");
    let handler = Arc::new(PanickyErrors::default());
    let server = MultiServer::start(&name, handler.clone(), single_slot(1)).expect("start");
    let client = connect_manually(&server, &name, 0xBAD0_0004);
    // Порча кольца клиент -> сервер: `write_pos` не кратен 8.
    let mapping = Mapping::open(ObjectPath::mapping(None, &format!("{name}_0"))).unwrap();
    // SAFETY: отображение живо до конца теста, размер -- его собственный.
    let view = unsafe { SharedView::attach(mapping.as_ptr(), mapping.size()) }.unwrap();
    view.header_b().write_pos.store(3, Ordering::Release);

    let mut dispatcher = Dispatcher::new(&server.core);
    dispatcher.enqueue(0);
    dispatcher.round();

    assert_eq!(handler.errors.load(Ordering::Acquire), 1, "on_error дважды");
    assert_eq!(
        handler.reasons.lock().unwrap().as_slice(),
        &[DisconnectReason::Error]
    );
    dispatcher.release_all();
    drop(dispatcher);
    drop(client);
}

/// Ревизия 3 (§7.1 п. 15): рукопожатие Multi идёт по уже захваченному
/// claim слота (`ClaimMode::Held`) -- в сессии `slot_claim` равен токену
/// клиента и claim сессии сервера; после `stop()` клиента claim свободен.
#[test]
fn multi_handshake_reuses_slot_token() {
    let name = unique("HELD_TOKEN");
    let server =
        MultiServer::start(&name, Arc::new(TestHandler::new()), single_slot(1)).expect("start");
    let client = MultiClient::connect(
        &name,
        Arc::new(ClientRecorder::default()),
        MultiClientOptions::default(),
    )
    .expect("connect");
    wait_for("подключение", Duration::from_secs(5), || {
        server.client_count() == 1 && client.is_connected()
    });
    {
        let slots = server.slots.read().unwrap();
        let slot = slots[0].lock().unwrap();
        let claim = slot.claim();
        assert_ne!(claim, CLAIM_FREE);
        assert_eq!(slot.session_claim, claim);
        assert_eq!(slot.server.session_claim(), claim);
    }
    client.stop();
    wait_for("claim снят", Duration::from_secs(5), || {
        server.slots.read().unwrap()[0].lock().unwrap().claim() == CLAIM_FREE
    });
    server.stop();
}

/// Ревизия 3 (§7.1 п. 10): слот занят чужим claim без заявки, и никто не
/// толкает -- диспетчер не просыпается ни разу (дедлайн назначается только
/// при пробуждении по другой причине).
#[test]
fn idle_server_with_foreign_claim_does_not_wake_up() {
    let name = unique("IDLE_CLAIM");
    let server =
        MultiServer::start(&name, Arc::new(TestHandler::new()), single_slot(2)).expect("start");
    thread::sleep(Duration::from_millis(200));
    let s0 = server.wakeups();
    {
        let slots = server.slots.read().unwrap();
        let slot = slots[0].lock().unwrap();
        slot.server
            .view()
            .control_block()
            .slot_claim
            .store(0x1D1E_C1A1, Ordering::Release);
    }
    thread::sleep(Duration::from_secs(1));
    assert_eq!(server.wakeups() - s0, 0, "диспетчер просыпался в простое");
    server.stop();
}

/// Fix round 1 (Task 5), Minor 2: пакет привязки, который ядро отказывается
/// привязать (подложен уже привязанным -- `STATUS_INVALID_PARAMETER_1`),
/// закрывается и сразу заменяется новым: привязка удаётся, а не
/// повторяется через `REARM_BACKOFF` бесконечно. Привязка закрытого пакета
/// снята -- чужой сигнал цел.
#[test]
fn broken_arm_packet_is_replaced() {
    let port = CompletionPort::new().unwrap();
    let event = EventHandle::create_unnamed(false).unwrap();
    let decoy = EventHandle::create_unnamed(false).unwrap();
    let mut arm = Arm::default();
    arm.arm(&port, event.as_handle(), 1).unwrap();
    assert!(!arm.cancel(), "сигнала не было");
    port.arm(
        arm.packet.as_mut().unwrap(),
        decoy.as_handle().as_raw_handle(),
        99,
        0,
    )
    .unwrap();

    arm.arm(&port, event.as_handle(), 2).unwrap();
    assert!(arm.armed);
    decoy.set().unwrap();
    assert_eq!(port.wait(Some(Duration::from_millis(50))).unwrap(), None);
    assert!(decoy.wait(Some(Duration::ZERO)).unwrap(), "сигнал цел");
    event.set().unwrap();
    assert_eq!(port.wait(Some(Duration::from_secs(5))).unwrap(), Some(2));
    assert!(arm.take(arm.generation));
    // Исправный пакет переиспользуется дальше.
    arm.arm(&port, event.as_handle(), 3).unwrap();
    event.set().unwrap();
    assert_eq!(port.wait(Some(Duration::from_secs(5))).unwrap(), Some(3));
}

/// Task 6 надёжности (fix round 1): сервер ушёл (`server_state = IDLE`,
/// `DISCONNECT`), а worker `MultiClient` ещё пишет очередь -- окно до
/// решения о конце сессии. Ни одной ошибки на сообщение (`NotConnected` --
/// не ошибка канала) и ни одного потерянного: очередь цела, в прежнем
/// порядке, и уйдёт в следующую сессию. Чтение в том же окне тоже молчит.
#[test]
fn flush_after_server_left_keeps_queue_without_errors() {
    #[derive(Default)]
    struct Errors(Mutex<Vec<ShmError>>);
    impl MultiClientHandler for Errors {
        fn on_connect(&self, _slot_id: u32) {}
        fn on_disconnect(&self) {}
        fn on_message(&self, _data: &[u8]) {}
        fn on_error(&self, err: ShmError) {
            self.0.lock().unwrap().push(err);
        }
    }

    let name = format!("XSHM_MULTI_FLUSH_LEFT_{}", std::process::id());
    let mut server = SharedServer::start(&name).unwrap();
    let waiter = thread::spawn(move || {
        server
            .wait_for_client(Some(Duration::from_secs(5)))
            .unwrap();
        server
    });
    let client = SharedClient::connect(&name, Duration::from_secs(5)).unwrap();
    let mut server = waiter.join().unwrap();
    server.disconnect_local();

    let outbox = ClientOutbox::default();
    let queued: Vec<Vec<u8>> = (0..64u32)
        .map(|i| format!("msg-{i:02}").into_bytes())
        .collect();
    outbox.queue.lock_clean().extend(queued.iter().cloned());
    let errors = Arc::new(Errors::default());
    let handler: Arc<dyn MultiClientHandler> = errors.clone();

    assert!(flush_queue(&client, &outbox, &handler));
    let mut buffer = Vec::new();
    let running = AtomicBool::new(true);
    assert!(receive_all(&client, &mut buffer, &handler, &running));
    assert!(
        errors.0.lock().unwrap().is_empty(),
        "шторм ошибок: {:?}",
        errors.0.lock().unwrap()
    );
    let left: Vec<Vec<u8>> = outbox.queue.lock_clean().iter().cloned().collect();
    assert_eq!(left, queued, "очередь потеряна или переставлена");
    assert!(!server.has_unread_from_client(), "запись в закрытую сессию");
}

/// Task 7 (решение C): испорченный кадр при дочитывании штатно ушедшего
/// клиента -- отдельный `on_error(Corrupted)`, а уход остаётся уходом:
/// причина `Graceful`, claim снят клиентом, лишнего `DISCONNECT` сервер не
/// сигналит (до исправления дочитывание рвало слот с `Error` и взводило
/// `DISCONNECT` уже ушедшему клиенту).
#[test]
fn departure_drain_corruption_keeps_graceful_reason() {
    let name = unique("DRAIN_CORRUPT");
    let journal = Arc::new(SlotJournal::new());
    let server = MultiServer::start(&name, journal.clone(), single_slot(1)).expect("start");
    let client = MultiClient::connect(
        &name,
        Arc::new(ClientRecorder::default()),
        MultiClientOptions::default(),
    )
    .expect("connect");
    wait_for("подключение", Duration::from_secs(5), || {
        server.client_count() == 1 && client.is_connected()
    });
    client.send(&7u32.to_le_bytes()).unwrap();
    wait_for(
        "кадр доставлен",
        Duration::from_secs(5),
        || journal.count(0) == 1,
    );
    // Вид пира на ту же секцию: `write_pos` клиент -> сервер не кратен 8,
    // без `DATA` -- порчу заметит только дочитывание при уходе.
    let mapping = Mapping::open(ObjectPath::mapping(None, &format!("{name}_0"))).unwrap();
    // SAFETY: отображение живо до конца теста, размер -- его собственный.
    let view = unsafe { SharedView::attach(mapping.as_ptr(), mapping.size()) }.unwrap();
    let write_pos = &view.header_b().write_pos;
    write_pos.store(
        write_pos.load(Ordering::Acquire).wrapping_add(3),
        Ordering::Release,
    );

    client.stop();
    wait_for("уход разобран", Duration::from_secs(5), || {
        journal.disconnect_of(0).is_some()
    });
    assert_eq!(
        journal.disconnect_of(0).map(|(reason, _)| reason),
        Some(DisconnectReason::Graceful)
    );
    assert_eq!(
        journal.errors.lock().unwrap().as_slice(),
        &[(Some(0), ShmError::Corrupted)]
    );
    let slots = server.slots.read().unwrap();
    let slot = slots[0].lock().unwrap();
    assert!(
        !slot
            .server
            .events()
            .unwrap()
            .disconnect
            .wait(Some(Duration::ZERO))
            .unwrap(),
        "лишний DISCONNECT ушедшему клиенту"
    );
    assert_eq!(slot.claim(), CLAIM_FREE);
    drop(slot);
    drop(slots);
    server.stop();
}

/// Task 7 (решение H): клиент, отключённый `disconnect_client` (`Local`) и
/// умерший, не узнав об этом, освобождает слот сразу по смерти процесса, а
/// не через `RESERVE_TIMEOUT`: сервер держит handle его процесса
/// («задержавшийся владелец») привязанным к порту.
///
/// Живой `MultiClient` по `DISCONNECT` уходит сам, поэтому «умер после
/// отключения» строится подменой: клиент -- `SharedClient` этого процесса,
/// который не ждёт ничего и не уходит (до конца теста), а владелец claim-а
/// (`slot_owner_pid`/`owner_start`) и наблюдаемый процесс сессии подменены
/// на дочерний `cmd /C pause`.
#[test]
fn client_dying_after_local_disconnect_frees_slot_at_once() {
    const TOKEN: u32 = 0x11E6_0001;
    let name = unique("LINGER_LOCAL");
    let server =
        MultiServer::start(&name, Arc::new(TestHandler::new()), single_slot(1)).expect("start");
    let slot_name = format!("{name}_0");
    assert!(try_claim_slot(None, &slot_name, TOKEN).unwrap());
    let mut client = connect_held(&slot_name, TOKEN).unwrap();
    wait_for("подключение", Duration::from_secs(5), || {
        server.client_count() == 1
    });

    let mut child = std::process::Command::new("cmd")
        .args(["/C", "pause"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn cmd");
    let pid = child.id();
    let start = win::process_start_time(pid).expect("время создания");
    let watch = ProcessWatch::open_peer(pid, start).expect("наблюдение");
    server.slots.read().unwrap()[0]
        .lock()
        .unwrap()
        .substitute_owner(watch, pid, start);

    server.disconnect_client(0).unwrap();
    let claim_of_slot0 = || server.slots.read().unwrap()[0].lock().unwrap().claim();
    thread::sleep(Duration::from_millis(100));
    assert_eq!(claim_of_slot0(), TOKEN, "claim живого владельца снят");

    let t0 = Instant::now();
    child.kill().expect("kill");
    child.wait().expect("wait");
    wait_for(
        "claim умершего снят",
        Duration::from_secs(1),
        || claim_of_slot0() == CLAIM_FREE,
    );
    println!("claim снят через {:?}", t0.elapsed());

    // Клиент этого процесса так и не узнал об отключении -- исчезает без L.
    client.mark_disconnected();
    drop(client);
    let next = MultiClient::connect(
        &name,
        Arc::new(ClientRecorder::default()),
        MultiClientOptions::default(),
    )
    .expect("connect");
    wait_for("новый клиент", Duration::from_secs(1), || {
        next.is_connected() && server.client_count() == 1
    });
    assert_eq!(next.slot_id(), 0);
    next.stop();
    server.stop();
}

/// Task 7 (п. 11): отказ S7 (`S2C_CONNECT`) рукопожатия слота доходит до
/// `on_error(Some(slot), ..)` ровно один раз (раньше любая ошибка
/// `accept_pending` молча считалась «толчком»); слот не подключён, клиент
/// сходится (проигранный отзыв -> `DISCONNECT` -> уход) и подключается
/// снова.
#[test]
fn failed_connect_ack_is_reported_on_slot() {
    let name = unique("ACK_FAIL");
    let handler = Arc::new(TestHandler::new());
    let server = MultiServer::start(&name, handler.clone(), single_slot(1)).expect("start");
    crate::server::fail_connect_ack_once(
        server.slots.read().unwrap()[0]
            .lock()
            .unwrap()
            .server
            .view(),
    );
    let client = MultiClient::connect(
        &name,
        Arc::new(ClientRecorder::default()),
        MultiClientOptions {
            slot_timeout: Duration::from_millis(300),
            retry_delay: Duration::from_millis(50),
            ..MultiClientOptions::default()
        },
    )
    .expect("connect");
    wait_for(
        "отказ S7 сообщён",
        Duration::from_secs(5),
        || !handler.errors.lock().unwrap().is_empty(),
    );
    assert_eq!(handler.connects.load(Ordering::Acquire), 0);
    wait_for(
        "клиент сошёлся и подключён",
        Duration::from_secs(10),
        || client.is_connected() && server.client_count() == 1,
    );
    assert_eq!(
        handler.errors.lock().unwrap().as_slice(),
        &[(
            Some(0),
            ShmError::WindowsError {
                code: 0xC000_0001,
                context: "NtSetEvent (injected)",
            }
        )]
    );
    assert_eq!(handler.connects.load(Ordering::Acquire), 1);
    client.stop();
    server.stop();
}

// ─── Task 10: пустой CONNECT_REQ -- O(1), поток не голодит диспетчер ─────

/// Поток-атакующий: взводит `events` по кругу без пауз, пока `run` истинно.
fn flood_events(events: Vec<EventHandle>, run: Arc<AtomicBool>) -> JoinHandle<()> {
    thread::spawn(move || {
        while run.load(Ordering::Acquire) {
            for event in &events {
                let _ = event.set();
            }
        }
    })
}

/// `CONNECT_REQ` слота `slot` сервера `name`.
fn slot_connect_req(name: &str, slot: u32) -> EventHandle {
    crate::events::SharedEvents::open(&format!("{name}_{slot}"))
        .unwrap()
        .connect_req
}

/// П3 (Task 10, решение R): пустой `CONNECT_REQ` одного слота стоит O(1)
/// блокировок слотов, а не обхода всех 256 (полный проход дедлайнов --
/// только на дедлайне или не чаще `LIVENESS_CHECK_INTERVAL`).
#[test]
fn stray_connect_req_work_is_constant() {
    let name = unique("STRAY_O1");
    let server =
        MultiServer::start(&name, Arc::new(TestHandler::new()), single_slot(256)).expect("start");
    thread::sleep(Duration::from_millis(200));
    let run = Arc::new(AtomicBool::new(true));
    // Relaxed: тестовая статистика.
    let (v0, w0) = (server.slot_visits.load(Ordering::Relaxed), server.wakeups());
    let attacker = flood_events(vec![slot_connect_req(&name, 7)], run.clone());
    thread::sleep(Duration::from_millis(500));
    run.store(false, Ordering::Release);
    attacker.join().unwrap();
    thread::sleep(Duration::from_millis(50));
    let visits = server.slot_visits.load(Ordering::Relaxed) - v0;
    let wakeups = server.wakeups() - w0;
    assert!(wakeups >= 10, "поток не дошёл до диспетчера: {wakeups}");
    assert!(
        visits <= 2 * wakeups,
        "блокировок слотов {visits} на {wakeups} пробуждений"
    );
    server.stop();
}

/// Время доставки сообщений: в каждом -- наносекунды от `base` в момент
/// отправки (LE).
struct Latency {
    base: Instant,
    samples: Mutex<Vec<Duration>>,
}

impl MultiHandler for Latency {
    fn on_client_connect(&self, _client_id: u32) {}
    fn on_client_disconnect(&self, _client_id: u32) {}
    fn on_message(&self, _client_id: u32, data: &[u8]) {
        if let Some(sent) = data.first_chunk::<8>() {
            let sent = Duration::from_nanos(u64::from_le_bytes(*sent));
            let latency = self.base.elapsed().saturating_sub(sent);
            self.samples.lock().unwrap().push(latency);
        }
    }
}

/// П1/П3: поток пустых `CONNECT_REQ` на все слоты не голодит диспетчер --
/// подключённый слот обслуживается (p99 доставки < 100 мс), новый клиент
/// подключается, `stop()` возвращается быстро.
#[test]
fn connect_req_flood_does_not_starve_dispatcher() {
    let name = unique("FLOOD_ALL");
    let journal = Arc::new(Latency {
        base: Instant::now(),
        samples: Mutex::new(Vec::new()),
    });
    let server = MultiServer::start(&name, journal.clone(), single_slot(4)).expect("start");
    let honest = MultiClient::connect(
        &name,
        Arc::new(ClientRecorder::default()),
        MultiClientOptions::default(),
    )
    .unwrap();
    wait_for(
        "честный клиент",
        Duration::from_secs(5),
        || honest.is_connected() && honest.slot_id() == 0,
    );
    let run = Arc::new(AtomicBool::new(true));
    let attacker = flood_events(
        (0..4).map(|slot| slot_connect_req(&name, slot)).collect(),
        run.clone(),
    );
    thread::sleep(Duration::from_millis(50));

    for _ in 0..100 {
        let sent = u64::try_from(journal.base.elapsed().as_nanos()).unwrap();
        honest.send(&sent.to_le_bytes()).unwrap();
        thread::sleep(Duration::from_millis(5));
    }
    wait_for(
        "доставка под потоком",
        Duration::from_secs(5),
        || journal.samples.lock().unwrap().len() == 100,
    );
    let mut samples = journal.samples.lock().unwrap().clone();
    samples.sort();
    let p99 = samples[98];
    assert!(p99 < Duration::from_millis(100), "p99 {p99:?}");

    let started = Instant::now();
    let newcomer = MultiClient::connect(
        &name,
        Arc::new(ClientRecorder::default()),
        MultiClientOptions::default(),
    )
    .unwrap();
    wait_for(
        "новый клиент под потоком",
        Duration::from_secs(5),
        || newcomer.is_connected(),
    );
    let connected_in = started.elapsed();
    assert!(
        connected_in <= Duration::from_millis(500),
        "{connected_in:?}"
    );

    let started = Instant::now();
    server.stop();
    let stopped_in = started.elapsed();
    run.store(false, Ordering::Release);
    attacker.join().unwrap();
    assert!(stopped_in <= Duration::from_millis(300), "{stopped_in:?}");
    drop(newcomer);
    drop(honest);
}

/// Fix round 1 (M5): пост пробуждения слота потерян (`wake_lost`) -- handle
/// отключённых клиентов (`retired`) закрываются на ближайшем витке
/// диспетчера по событию ЛЮБОГО слота, а не ждут события своего.
#[test]
fn lost_wake_post_closes_retired_on_next_pass() {
    let name = unique("WAKE_LOST");
    let server =
        MultiServer::start(&name, Arc::new(TestHandler::new()), single_slot(2)).expect("start");
    thread::sleep(Duration::from_millis(100));
    {
        let slots = server.slots.read().unwrap();
        let mut slot = slots[0].lock().unwrap();
        slot.retired
            .push(ProcessWatch::open(std::process::id()).unwrap());
    }
    server.wake_lost.store(true, Ordering::Release);
    slot_connect_req(&name, 1).set().unwrap();
    wait_for(
        "retired слота 0 закрыты",
        Duration::from_secs(2),
        || {
            server.slots.read().unwrap()[0]
                .lock()
                .unwrap()
                .retired
                .is_empty()
        },
    );
    server.stop();
}

/// Обработчик клиента Multi, записывающий ошибки и момент подключения
/// (Task 11 надёжности).
#[derive(Default)]
struct PresenceRecorder {
    connected_at: Mutex<Option<Instant>>,
    errors: Mutex<Vec<ShmError>>,
}

impl MultiClientHandler for PresenceRecorder {
    fn on_connect(&self, _slot_id: u32) {
        *self.connected_at.lock().unwrap() = Some(Instant::now());
    }
    fn on_disconnect(&self) {}
    fn on_message(&self, _data: &[u8]) {}
    fn on_error(&self, err: ShmError) {
        self.errors.lock().unwrap().push(err);
    }
}

/// Task 11 (R19): сервера нет -- клиент Multi спит на маяке базы: ни
/// пробуждений, ни повторных `on_error(NotConnected)` (до Task 11 --
/// попытка раз в `retry_delay`).
#[test]
fn absent_server_multi_client_sleeps() {
    let name = unique("ABSENT");
    let recorder = Arc::new(PresenceRecorder::default());
    let client = MultiClient::connect(&name, recorder.clone(), MultiClientOptions::default())
        .expect("client connect");
    thread::sleep(Duration::from_millis(200));
    let w0 = client.wakeups();
    thread::sleep(Duration::from_secs(1));
    let woke = client.wakeups() - w0;
    assert!(woke <= 1, "пробуждений без сервера: {woke}");
    let errors = recorder.errors.lock().unwrap().clone();
    assert!(errors.len() <= 1, "ошибки без сервера: {errors:?}");
    drop(client);
}

/// Task 11 (R19): клиент Multi стартовал раньше сервера -- подключается
/// сразу после взвода маяка базы, а не через `retry_delay` (10 с).
#[test]
fn multi_client_connects_when_server_appears() {
    let name = unique("PROMPT");
    let recorder = Arc::new(PresenceRecorder::default());
    let client = MultiClient::connect(
        &name,
        recorder.clone(),
        MultiClientOptions {
            retry_delay: Duration::from_secs(10),
            ..MultiClientOptions::default()
        },
    )
    .expect("client connect");
    thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    let server =
        MultiServer::start(&name, Arc::new(TestHandler::new()), single_slot(2)).expect("start");
    wait_for("подключение", Duration::from_secs(15), || {
        recorder.connected_at.lock().unwrap().is_some()
    });
    let took = recorder.connected_at.lock().unwrap().unwrap() - started;
    assert!(
        took <= Duration::from_millis(300),
        "подключение через {took:?}"
    );
    drop(client);
    server.stop();
}

/// M2 ревью Task 11: маяк базы Multi -- `<база>_multi_beacon`, канал с тем
/// же именем держит свой `<имя>_beacon`; остановка одного не гасит маяк
/// другого.
#[test]
fn multi_base_beacon_is_distinct_from_channel_beacon() {
    let name = unique("SHARED_NAME");
    let channel = SharedServer::start(&name).expect("канал");
    let multi =
        MultiServer::start(&name, Arc::new(TestHandler::new()), single_slot(1)).expect("multi");
    assert_eq!(multi.presence(), PresenceStatus::Active);
    let channel_beacon = crate::beacon::Beacon::open(&name).unwrap();
    let multi_beacon = crate::beacon::Beacon::open(&multi_beacon_name(&name)).unwrap();
    assert!(channel_beacon.is_raised().unwrap());
    assert!(multi_beacon.is_raised().unwrap());
    multi.stop();
    assert!(!multi_beacon.is_raised().unwrap(), "маяк базы сброшен");
    assert!(channel_beacon.is_raised().unwrap(), "маяк канала не тронут");
    assert_eq!(multi.presence(), PresenceStatus::Disabled);
    drop(channel);
    assert!(!channel_beacon.is_raised().unwrap());
}

/// W(3): сервер Multi без маяка (база не взводится) -- клиент, пришедший
/// раньше, находит его резервной проверкой `discovery_fallback`.
#[test]
fn multi_client_finds_beaconless_server_by_fallback() {
    let name = unique("NOBEACON");
    // Маяк базы занят событием другого типа: сервер стартует без маяка.
    let sec = ObjectSecurity::default().attributes().unwrap();
    let _squatter = EventHandle::create(
        ObjectPath::scoped(None, &multi_beacon_name(&name), "_beacon"),
        &sec,
    )
    .unwrap();
    let recorder = Arc::new(PresenceRecorder::default());
    let client = MultiClient::connect(
        &name,
        recorder.clone(),
        MultiClientOptions {
            retry_delay: Duration::from_secs(10),
            discovery_fallback: Some(Duration::from_millis(200)),
            ..MultiClientOptions::default()
        },
    )
    .expect("client connect");
    thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    let server =
        MultiServer::start(&name, Arc::new(TestHandler::new()), single_slot(1)).expect("start");
    assert!(matches!(server.presence(), PresenceStatus::Unavailable(_)));
    wait_for("подключение", Duration::from_secs(15), || {
        recorder.connected_at.lock().unwrap().is_some()
    });
    let took = recorder.connected_at.lock().unwrap().unwrap() - started;
    assert!(
        took <= Duration::from_millis(600),
        "подключение через {took:?}"
    );
    drop(client);
    server.stop();
}
