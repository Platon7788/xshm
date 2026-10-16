//! Обмен PID в handshake (протокол 0.8+) и совместимость со старыми пирами.
//!
//! «Старый» пир моделируется так же, как он выглядит в shared memory:
//! клиент 0.7/0.8.0 не пишет `ControlBlock::client_pid`
//! (`connect_impl(.., announce_pid = false)`), сервер 0.7/0.8.0 не пишет
//! `ControlBlock::server_pid` (обнуляем после `start`).

use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use crate::{SharedClient, SharedServer, ShmError};

fn unique(tag: &str) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("XSHM_PEER_{tag}_{}_{}", std::process::id(), ts % 1_000_000)
}

/// Поднимает сервер в отдельном потоке и подключает клиента.
fn connect_pair(name: &str, old_server: bool, announce_pid: bool) -> (SharedServer, SharedClient) {
    let mut server = SharedServer::start(name).unwrap();
    if old_server {
        server
            .view()
            .control_block()
            .server_pid
            .store(0, Ordering::Release);
    }
    let server_thread = thread::spawn(move || {
        server
            .wait_for_client(Some(Duration::from_secs(5)))
            .unwrap();
        server
    });
    thread::sleep(Duration::from_millis(30));
    let client =
        SharedClient::connect_impl(name, Duration::from_secs(5), announce_pid, None).unwrap();
    (server_thread.join().unwrap(), client)
}

fn roundtrip(server: &SharedServer, client: &SharedClient) {
    let mut buf = Vec::new();
    server.try_send_to_client(b"ping").unwrap();
    assert!(client.poll_server(Some(Duration::from_secs(1))).unwrap());
    client.receive_from_server(&mut buf).unwrap();
    assert_eq!(buf, b"ping");
    client.send_to_server(b"pong").unwrap();
    assert!(server.poll_client(Some(Duration::from_secs(1))).unwrap());
    server.receive_from_client(&mut buf).unwrap();
    assert_eq!(buf, b"pong");
}

#[test]
fn new_peers_exchange_pids() {
    let (server, client) = connect_pair(&unique("NEW"), false, true);
    let me = std::process::id();
    assert_eq!(server.peer_pid(), Some(me));
    assert_eq!(client.peer_pid(), Some(me));
    assert_eq!(server.is_peer_alive(), Some(true));
    assert_eq!(client.is_peer_alive(), Some(true));
    // PID клиента одноразовый: сервер забрал его swap-ом.
    assert_eq!(
        server
            .view()
            .control_block()
            .client_pid
            .load(Ordering::Acquire),
        0
    );
    roundtrip(&server, &client);
}

/// Клиент 0.7/0.8.0 (без PID) + новый сервер: наблюдения за клиентом нет,
/// всё остальное работает как раньше.
#[test]
fn old_client_interoperates_without_peer_watch() {
    let (server, client) = connect_pair(&unique("OLDC"), false, false);
    assert_eq!(server.peer_pid(), None);
    assert_eq!(server.is_peer_alive(), None);
    assert_eq!(client.peer_pid(), Some(std::process::id()));
    // Без наблюдения poll_* ведёт себя как в 0.7: просто таймаут.
    assert_eq!(
        server.poll_client(Some(Duration::from_millis(10))),
        Ok(false)
    );
    roundtrip(&server, &client);
}

/// Сервер 0.7/0.8.0 (PID не опубликован) + новый клиент.
#[test]
fn old_server_interoperates_without_peer_watch() {
    let (server, client) = connect_pair(&unique("OLDS"), true, true);
    assert_eq!(client.peer_pid(), None);
    assert_eq!(client.is_peer_alive(), None);
    assert_eq!(server.peer_pid(), Some(std::process::id()));
    assert_eq!(
        client.poll_server(Some(Duration::from_millis(10))),
        Ok(false)
    );
    roundtrip(&server, &client);
}

/// Устаревший PID (клиент упал между записью PID и HELLO, а следом пришёл
/// клиент старой версии) указывает на мёртвый процесс -- его нельзя
/// принимать за «пир умер»: наблюдения просто нет.
#[test]
fn stale_dead_pid_is_not_reported_as_peer_death() {
    let mut child = std::process::Command::new("cmd")
        .args(["/C", "exit", "0"])
        .spawn()
        .unwrap();
    let dead_pid = child.id();
    child.wait().unwrap(); // `child` держит handle -> объект-процесс жив, но сигнален

    let name = unique("STALE");
    let mut server = SharedServer::start(&name).unwrap();
    server
        .view()
        .control_block()
        .client_pid
        .store(dead_pid, Ordering::Release);
    let server_thread = thread::spawn(move || {
        server
            .wait_for_client(Some(Duration::from_secs(5)))
            .unwrap();
        server
    });
    thread::sleep(Duration::from_millis(30));
    let client = SharedClient::connect_impl(&name, Duration::from_secs(5), false, None).unwrap();
    let server = server_thread.join().unwrap();
    assert_eq!(server.peer_pid(), None);
    assert_eq!(server.is_peer_alive(), None);
    roundtrip(&server, &client);
    drop(child);
}

/// Откат handshake (сервер не ответил) отзывает заявленный PID.
#[test]
fn rollback_clears_announced_pid() {
    let name = unique("RB");
    let server = SharedServer::start(&name).unwrap();
    // Никто не вызывает wait_for_client -> клиент упрётся в таймаут.
    let err = SharedClient::connect(&name, Duration::from_millis(50)).unwrap_err();
    assert_eq!(err, ShmError::Timeout);
    assert_eq!(
        server
            .view()
            .control_block()
            .client_pid
            .load(Ordering::Acquire),
        0
    );
}

/// Отключение закрывает handle пира (RAII) и обнуляет наблюдение.
#[test]
fn disconnect_drops_peer_watch() {
    let (mut server, client) = connect_pair(&unique("DROP"), false, true);
    assert!(server.peer_pid().is_some());
    server.mark_disconnected();
    assert_eq!(server.peer_pid(), None);
    assert_eq!(server.is_peer_alive(), None);
    drop(client);
}

/// Находка 23: клиент сообщил чужое время создания процесса (подмена PID
/// или PID, переиспользованный после смерти пира) -- сервер не принимает
/// процесс с этим PID за живого клиента.
#[test]
fn client_with_forged_start_time_is_reported_dead() {
    let name = unique("FORGE");
    let mut server = SharedServer::start(&name).unwrap();
    let server_thread = thread::spawn(move || {
        server
            .wait_for_client(Some(Duration::from_secs(5)))
            .unwrap();
        server
    });
    thread::sleep(Duration::from_millis(30));
    let forged = crate::win::current_process_start_time().wrapping_add(1);
    crate::client::FORGED_START_TIME.with(|cell| cell.set(Some(forged)));
    let client = SharedClient::connect_impl(&name, Duration::from_secs(5), true, None);
    crate::client::FORGED_START_TIME.with(|cell| cell.set(None));
    let client = client.unwrap();
    let server = server_thread.join().unwrap();
    assert_ne!(server.is_peer_alive(), Some(true));
    assert_eq!(server.peer_pid(), None);
    // Время одноразовое, как и PID: сервер забрал его парой.
    let control = server.view().control_block();
    assert_eq!(control.client_start_lo.load(Ordering::Acquire), 0);
    assert_eq!(control.client_start_hi.load(Ordering::Acquire), 0);
    roundtrip(&server, &client);
}

/// Тест-порча (Review Focus 1/4): мусор в старшей половине времени сервера
/// не даёт клиенту принять процесс с этим PID за живой сервер.
#[test]
fn corrupted_server_start_hi_is_not_a_live_server() {
    let name = unique("CORRUPT");
    let mut server = SharedServer::start(&name).unwrap();
    let control = server.view().control_block();
    let hi = control.server_start_hi.load(Ordering::Acquire);
    control
        .server_start_hi
        .store(hi ^ 0xDEAD_BEEF, Ordering::Release);
    let server_thread = thread::spawn(move || {
        server
            .wait_for_client(Some(Duration::from_secs(5)))
            .unwrap();
        server
    });
    thread::sleep(Duration::from_millis(30));
    let client = SharedClient::connect(&name, Duration::from_secs(5)).unwrap();
    let server = server_thread.join().unwrap();
    assert_ne!(client.is_peer_alive(), Some(true));
    assert_eq!(client.peer_pid(), None);
    // Честное время клиента сервер принимает.
    assert_eq!(server.is_peer_alive(), Some(true));
    roundtrip(&server, &client);
}
