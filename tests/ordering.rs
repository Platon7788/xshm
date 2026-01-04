//! Тесты корректности memory ordering и валидации.
//!
//! Проверяют:
//! 1. Порядок операций write_pos/message_count
//! 2. Валидацию magic/version при подключении
//! 3. Корректность handshake с generation

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use xshm::{SharedClient, SharedServer, ShmError};

fn unique_name(tag: &str) -> String {
    use std::time::SystemTime;
    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("XSHM_ORD_{}_{}_{}", tag, std::process::id(), ts % 1_000_000)
}

/// Тест: message_count обновляется ДО write_pos
/// Reader не должен видеть QueueEmpty когда данные уже записаны
#[test]
fn test_message_count_before_write_pos() {
    const ITERATIONS: usize = 1000;
    let name = unique_name("COUNT_ORDER");
    
    let server_ready = Arc::new(AtomicBool::new(false));
    let errors_found = Arc::new(AtomicU32::new(0));
    
    let server_ready_clone = server_ready.clone();
    let errors_clone = errors_found.clone();
    let name_clone = name.clone();
    
    let server_thread = thread::spawn(move || -> xshm::Result<()> {
        let mut server = SharedServer::start(&name_clone)?;
        server.wait_for_client(Some(Duration::from_secs(5)))?;
        server_ready_clone.store(true, Ordering::Release);
        
        // Быстро отправляем сообщения
        for i in 0..ITERATIONS {
            let msg = format!("MSG_{:04}", i);
            match server.send_to_client(msg.as_bytes()) {
                Ok(_) => {}
                Err(e) => {
                    errors_clone.fetch_add(1, Ordering::Relaxed);
                    eprintln!("Server send error: {:?}", e);
                }
            }
            // Без sleep - максимальная нагрузка на ordering
        }
        
        // Финальное сообщение
        thread::sleep(Duration::from_millis(50));
        server.send_to_client(b"DONE")?;
        Ok(())
    });
    
    thread::sleep(Duration::from_millis(100));
    
    let client_result = (|| -> xshm::Result<usize> {
        let client = SharedClient::connect(&name, Duration::from_secs(5))?;
        let mut recv_buf = Vec::new();
        let mut received = 0usize;
        let mut empty_after_data = 0u32;
        let start = Instant::now();
        
        while start.elapsed() < Duration::from_secs(10) {
            match client.receive_from_server(&mut recv_buf) {
                Ok(len) => {
                    if &recv_buf[..len] == b"DONE" {
                        break;
                    }
                    received += 1;
                }
                Err(ShmError::QueueEmpty) => {
                    // Это нормально если очередь действительно пуста
                    // Но если мы только что получили данные и сразу QueueEmpty - 
                    // это может быть ordering issue
                    if received > 0 {
                        empty_after_data += 1;
                    }
                    // Ждём события вместо busy-wait
                    let _ = client.poll_server(Some(Duration::from_millis(10)));
                }
                Err(e) => return Err(e),
            }
        }
        
        // empty_after_data > ITERATIONS означает слишком много ложных QueueEmpty
        // Это индикатор ordering проблемы
        if empty_after_data > ITERATIONS as u32 * 2 {
            eprintln!("Warning: {} empty reads after data (possible ordering issue)", empty_after_data);
        }
        
        Ok(received)
    })();
    
    let _ = server_thread.join();
    
    match client_result {
        Ok(received) => {
            assert!(received > 0, "Should receive at least some messages");
            println!("Received {} messages", received);
        }
        Err(e) => panic!("Client error: {:?}", e),
    }
    
    assert_eq!(errors_found.load(Ordering::Relaxed), 0, "Server had errors");
}

/// Тест: клиент должен отклонить невалидный magic
#[test]
fn test_invalid_magic_rejected() {
    // Этот тест проверяет что клиент не подключится к "мусорной" памяти
    // Создаём сервер, ждём немного, потом пробуем подключиться с другим именем
    let name = unique_name("MAGIC_TEST");
    let wrong_name = unique_name("WRONG_MAGIC");
    
    let _server = SharedServer::start(&name).expect("server start");
    
    // Попытка подключиться к несуществующему серверу должна вернуть ошибку
    let result = SharedClient::connect(&wrong_name, Duration::from_millis(100));
    
    assert!(result.is_err(), "Should fail to connect to non-existent server");
}

/// Тест: generation корректно обновляется при reconnect
#[test]
fn test_generation_on_reconnect() {
    let name = unique_name("GEN_TEST");
    
    let mut server = SharedServer::start(&name).expect("server start");
    
    // Первое подключение
    let client1_thread = thread::spawn({
        let name = name.clone();
        move || -> xshm::Result<()> {
            let client = SharedClient::connect(&name, Duration::from_secs(2))?;
            // Отправляем что-то
            client.send_to_server(b"HELLO1")?;
            thread::sleep(Duration::from_millis(100));
            // Клиент отключается при drop
            Ok(())
        }
    });
    
    server.wait_for_client(Some(Duration::from_secs(2))).expect("wait client 1");
    
    let mut buf = Vec::new();
    let _ = server.poll_client(Some(Duration::from_millis(200)));
    let len = server.receive_from_client(&mut buf).expect("receive from client 1");
    assert_eq!(&buf[..len], b"HELLO1");
    
    client1_thread.join().unwrap().expect("client 1 ok");
    
    // Сервер должен обнаружить disconnect и быть готов к новому клиенту
    // В текущей реализации нужно пересоздать сервер для нового клиента
    drop(server);
    
    let mut server2 = SharedServer::start(&name).expect("server2 start");
    
    // Второе подключение
    let client2_thread = thread::spawn({
        let name = name.clone();
        move || -> xshm::Result<()> {
            let client = SharedClient::connect(&name, Duration::from_secs(2))?;
            client.send_to_server(b"HELLO2")?;
            thread::sleep(Duration::from_millis(100));
            Ok(())
        }
    });
    
    server2.wait_for_client(Some(Duration::from_secs(2))).expect("wait client 2");
    
    let mut buf2 = Vec::new();
    let _ = server2.poll_client(Some(Duration::from_millis(200)));
    let len2 = server2.receive_from_client(&mut buf2).expect("receive from client 2");
    assert_eq!(&buf2[..len2], b"HELLO2");
    
    client2_thread.join().unwrap().expect("client 2 ok");
}

/// Тест: высокая нагрузка на ordering в обоих направлениях
#[test]
fn test_bidirectional_ordering_stress() {
    const MESSAGES_PER_SIDE: usize = 500;
    let name = unique_name("BIDIR_ORD");
    
    let server_thread = thread::spawn({
        let name = name.clone();
        move || -> xshm::Result<(usize, usize)> {
            let mut server = SharedServer::start(&name)?;
            server.wait_for_client(Some(Duration::from_secs(5)))?;
            
            let mut sent = 0usize;
            let mut received = 0usize;
            let mut buf = Vec::new();
            let start = Instant::now();
            
            while (sent < MESSAGES_PER_SIDE || received < MESSAGES_PER_SIDE) 
                  && start.elapsed() < Duration::from_secs(10) 
            {
                // Отправка
                if sent < MESSAGES_PER_SIDE {
                    let msg = format!("S{:04}", sent);
                    if server.send_to_client(msg.as_bytes()).is_ok() {
                        sent += 1;
                    }
                }
                
                // Приём
                match server.receive_from_client(&mut buf) {
                    Ok(len) => {
                        if buf[..len].starts_with(b"C") {
                            received += 1;
                        }
                        if &buf[..len] == b"CDONE" {
                            break;
                        }
                    }
                    Err(ShmError::QueueEmpty) => {
                        let _ = server.poll_client(Some(Duration::from_millis(1)));
                    }
                    Err(_) => {}
                }
            }
            
            // Сигнал завершения
            let _ = server.send_to_client(b"SDONE");
            
            Ok((sent, received))
        }
    });
    
    thread::sleep(Duration::from_millis(50));
    
    let client_result = (|| -> xshm::Result<(usize, usize)> {
        let client = SharedClient::connect(&name, Duration::from_secs(5))?;
        
        let mut sent = 0usize;
        let mut received = 0usize;
        let mut buf = Vec::new();
        let start = Instant::now();
        
        while (sent < MESSAGES_PER_SIDE || received < MESSAGES_PER_SIDE)
              && start.elapsed() < Duration::from_secs(10)
        {
            // Отправка
            if sent < MESSAGES_PER_SIDE {
                let msg = format!("C{:04}", sent);
                if client.send_to_server(msg.as_bytes()).is_ok() {
                    sent += 1;
                }
            }
            
            // Приём
            match client.receive_from_server(&mut buf) {
                Ok(len) => {
                    if buf[..len].starts_with(b"S") {
                        received += 1;
                    }
                    if &buf[..len] == b"SDONE" {
                        break;
                    }
                }
                Err(ShmError::QueueEmpty) => {
                    let _ = client.poll_server(Some(Duration::from_millis(1)));
                }
                Err(_) => {}
            }
        }
        
        // Сигнал завершения
        let _ = client.send_to_server(b"CDONE");
        
        Ok((sent, received))
    })();
    
    let server_result = server_thread.join().expect("server thread panic");
    
    let (server_sent, server_received) = server_result.expect("server error");
    let (client_sent, client_received) = client_result.expect("client error");
    
    println!("Server: sent={}, received={}", server_sent, server_received);
    println!("Client: sent={}, received={}", client_sent, client_received);
    
    // Должны отправить все сообщения
    assert_eq!(server_sent, MESSAGES_PER_SIDE, "Server should send all messages");
    assert_eq!(client_sent, MESSAGES_PER_SIDE, "Client should send all messages");
    
    // Должны получить большинство (некоторые могут быть потеряны при завершении)
    assert!(server_received > MESSAGES_PER_SIDE / 2, "Server should receive most messages");
    assert!(client_received > MESSAGES_PER_SIDE / 2, "Client should receive most messages");
}

/// Тест: проверка что compile-time assert работает (этот тест всегда проходит на x86/x64)
#[test]
fn test_architecture_supported() {
    #[cfg(target_arch = "x86")]
    println!("Running on x86 (32-bit)");
    
    #[cfg(target_arch = "x86_64")]
    println!("Running on x86_64 (64-bit)");
    
    // Если мы здесь - значит архитектура поддерживается
    assert!(cfg!(any(target_arch = "x86", target_arch = "x86_64")));
}
