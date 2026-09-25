use super::*;

struct TestHandler {
    connects: AtomicU32,
    disconnects: AtomicU32,
    messages: AtomicU32,
    last_reason: Mutex<Option<DisconnectReason>>,
}

impl TestHandler {
    fn new() -> Self {
        Self {
            connects: AtomicU32::new(0),
            disconnects: AtomicU32::new(0),
            messages: AtomicU32::new(0),
            last_reason: Mutex::new(None),
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

/// Регрессия (аудит API 2026-07-10): раньше `MultiClient` не имел
/// `max_send_queue` вообще -- внутренняя send-очередь росла неограниченно,
/// если пир завис/тормозит. Теперь при достижении лимита самое старое
/// сообщение вытесняется (overwrite-семантика, как у `AutoOptions`).
#[test]
fn push_with_cap_evicts_oldest_and_reports_overflow() {
    let mut queue: VecDeque<Vec<u8>> = VecDeque::new();

    assert!(!push_with_cap(&mut queue, b"a".to_vec(), 2));
    assert!(!push_with_cap(&mut queue, b"b".to_vec(), 2));
    assert_eq!(queue.len(), 2);

    // Очередь на пределе -- третья вставка вытесняет самую старую ("a").
    let overflowed = push_with_cap(&mut queue, b"c".to_vec(), 2);
    assert!(overflowed);
    assert_eq!(queue.len(), 2);
    assert_eq!(
        queue.iter().map(|v| v.as_slice()).collect::<Vec<_>>(),
        [b"b".as_slice(), b"c".as_slice()]
    );
}

/// Регрессия (аудит 2026-07-10): токены захвата не должны предсказуемо
/// повторяться/коллизировать (старая схема `pid<<8 ^ n` гарантированно
/// коллизировала между процессами при n >= 256) и никогда не должны
/// совпадать с CLAIM_FREE.
#[test]
fn claim_tokens_are_never_free_and_practically_unique() {
    use std::collections::HashSet;

    let tokens: HashSet<u32> = (0..1000).map(|_| next_claim_token()).collect();

    assert!(
        !tokens.contains(&CLAIM_FREE),
        "next_claim_token() никогда не должен вернуть CLAIM_FREE"
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
        control.reserved[RESERVED_CLAIM_INDEX].store(STALE_TOKEN, Ordering::Release);
        control.reserved[RESERVED_OWNER_PID_INDEX].store(dead_pid, Ordering::Release);
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
        control.reserved[RESERVED_CLAIM_INDEX].store(0xABCD_0001, Ordering::Release);
        control.reserved[RESERVED_OWNER_PID_INDEX].store(std::process::id(), Ordering::Release);
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
        slot0.server.view().control_block().reserved[RESERVED_CLAIM_INDEX]
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
        let claim_field = &slot0.server.view().control_block().reserved[RESERVED_CLAIM_INDEX];
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
        slot0.server.view().control_block().reserved[RESERVED_CLAIM_INDEX]
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
    assert!(try_claim_slot(&slot_name, 0xC1A1_0001).unwrap());
    thread::sleep(Duration::from_millis(100));
    let w0 = server.wakeups();
    assert_eq!(w0, 0, "захват сам по себе сервер не будит");

    let t0 = Instant::now();
    nudge_stale_claims(&name);
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
    assert!(try_claim_slot(&format!("{name}_0"), 0xC1A1_0002).unwrap());

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

/// `stop()` сервера (два worker-а, есть подключённый клиент) и Drop клиента
/// (подключённого и ждущего сервер в паузе `retry_delay`) возвращаются сразу:
/// их будит событие, а не тик.
#[test]
fn stop_is_prompt() {
    let name = unique("PROMPT");
    let server = MultiServer::start(
        &name,
        Arc::new(TestHandler::new()),
        single_slot(MAX_MULTI_CLIENTS),
    )
    .expect("start");
    assert_eq!(server.wakes.len(), 2, "31 слот -- два worker-а");
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

/// Слоты второй группы (второй worker) обслуживаются так же: 24 клиента на
/// 31 слоте, каждый обменивается сообщением.
#[test]
fn clients_across_two_worker_groups() {
    let name = unique("GROUPS");
    let handler = Arc::new(TestHandler::new());
    let server =
        MultiServer::start(&name, handler.clone(), single_slot(MAX_MULTI_CLIENTS)).expect("start");
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
            try_claim_slot(&slot_name, TOKEN_A).unwrap(),
            "раунд {round}"
        );
        let mut a = SharedClient::connect(&slot_name, Duration::from_secs(5)).unwrap();
        wait_for("подключение A", Duration::from_secs(5), || {
            server.client_count() == 1
        });

        server.disconnect_client(0).unwrap();
        // Гонка из дефекта: B пытается захватить слот сразу после отключения.
        assert!(
            !try_claim_slot(&slot_name, TOKEN_B).unwrap(),
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
        release_claim(&slot_name, TOKEN_A);

        assert!(
            try_claim_slot(&slot_name, TOKEN_B).unwrap(),
            "раунд {round}"
        );
        let b = SharedClient::connect(&slot_name, Duration::from_secs(5)).unwrap();
        wait_for("подключение B", Duration::from_secs(5), || {
            server.client_count() == 1
        });
        assert!(b.is_session_current());
        drop(b); // штатный уход B: DISCONNECT -> сервер снимает claim CAS-ом
        release_claim(&slot_name, TOKEN_B);
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
    assert!(try_claim_slot(&slot_name, 0xDEAD_0001).unwrap());
    let client = SharedClient::connect(&slot_name, Duration::from_secs(5)).unwrap();
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
            control.reserved[RESERVED_CLAIM_INDEX].load(Ordering::Acquire),
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
