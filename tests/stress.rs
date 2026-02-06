use std::convert::TryInto;
use std::thread;
use std::time::{Duration, Instant};

use xshm::{SharedClient, SharedServer, ShmError};

const MIN_SLEEP: Duration = Duration::from_micros(50);

fn unique_name(tag: &str) -> String {
    use std::time::SystemTime;
    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(
        "XSHM_STRESS_{}_{}_{}",
        tag,
        std::process::id(),
        ts % 1_000_000
    )
}

#[test]
fn stress_server_to_client() {
    const TOTAL: usize = 4000;
    let name = unique_name("S2C");

    let server_thread = thread::spawn({
        let name = name.clone();
        move || -> xshm::Result<(usize, usize)> {
            let mut server = SharedServer::start(&name)?;
            server.wait_for_client(Some(Duration::from_secs(5)))?;

            let mut overwritten_total = 0usize;
            for i in 0..TOTAL {
                let mut msg = [0u8; 8];
                msg[..4].copy_from_slice(&(i as u32).to_le_bytes());
                loop {
                    match server.send_to_client(&msg) {
                        Ok(outcome) => {
                            overwritten_total += outcome.overwritten as usize;
                            break;
                        }
                        Err(ShmError::QueueFull) => {
                            thread::sleep(MIN_SLEEP);
                        }
                        Err(err) => return Err(err),
                    }
                }
                thread::yield_now();
            }

            server.send_to_client(b"END")?;

            let mut recv_buf = Vec::new();
            let start = Instant::now();
            loop {
                if start.elapsed() > Duration::from_secs(5) {
                    return Err(ShmError::Timeout);
                }
                if server.poll_client(Some(Duration::from_millis(10)))? {
                    let len = server.receive_from_client(&mut recv_buf)?;
                    if &recv_buf[..len] == b"ACK" {
                        break;
                    }
                }
            }

            Ok((TOTAL, overwritten_total))
        }
    });

    thread::sleep(Duration::from_millis(50));

    let client_result = (|| -> xshm::Result<usize> {
        let client = SharedClient::connect(&name, Duration::from_secs(5))?;
        let mut recv_buf = Vec::new();
        let mut expected = 0u32;
        let start = Instant::now();

        loop {
            if start.elapsed() > Duration::from_secs(5) {
                return Err(ShmError::Timeout);
            }
            if client.poll_server(Some(Duration::from_millis(10)))? {
                match client.receive_from_server(&mut recv_buf) {
                    Ok(len) => {
                        if &recv_buf[..len] == b"END" {
                            break;
                        }
                        assert!(len >= 4, "message too short: {len}");
                        let value = u32::from_le_bytes(recv_buf[..4].try_into().unwrap());
                        assert_eq!(
                            value, expected,
                            "out of order message: got {value}, expected {expected}"
                        );
                        expected += 1;
                    }
                    Err(ShmError::QueueEmpty) => {}
                    Err(err) => return Err(err),
                }
            } else {
                thread::sleep(MIN_SLEEP);
            }
        }

        assert_eq!(expected as usize, TOTAL);
        client.send_to_server(b"ACK")?;
        Ok(expected as usize)
    })();

    if let Err(err) = client_result {
        panic!("client error: {err:?}");
    }

    match server_thread.join() {
        Ok(Ok((sent, overwritten))) => {
            assert_eq!(sent, TOTAL);
            assert_eq!(overwritten, 0, "server overwrote {overwritten} messages");
        }
        Ok(Err(err)) => panic!("server error: {err:?}"),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

#[test]
fn stress_client_to_server() {
    const TOTAL: usize = 4000;
    let name = unique_name("C2S");

    let server_thread = thread::spawn({
        let name = name.clone();
        move || -> xshm::Result<usize> {
            let mut server = SharedServer::start(&name)?;
            server.wait_for_client(Some(Duration::from_secs(5)))?;
            let mut recv_buf = Vec::new();
            let mut expected = 0u32;
            let start = Instant::now();

            loop {
                if start.elapsed() > Duration::from_secs(5) {
                    return Err(ShmError::Timeout);
                }
                if server.poll_client(Some(Duration::from_millis(10)))? {
                    match server.receive_from_client(&mut recv_buf) {
                        Ok(len) => {
                            if &recv_buf[..len] == b"END" {
                                break;
                            }
                            assert!(len >= 4);
                            let value = u32::from_le_bytes(recv_buf[..4].try_into().unwrap());
                            assert_eq!(
                                value, expected,
                                "out of order message: got {value}, expected {expected}"
                            );
                            expected += 1;
                        }
                        Err(ShmError::QueueEmpty) => {}
                        Err(err) => return Err(err),
                    }
                } else {
                    thread::sleep(MIN_SLEEP);
                }
            }

            assert_eq!(expected as usize, TOTAL);
            server.send_to_client(b"ACK")?;
            Ok(expected as usize)
        }
    });

    thread::sleep(Duration::from_millis(50));

    let client_result = (|| -> xshm::Result<(usize, usize)> {
        let client = SharedClient::connect(&name, Duration::from_secs(5))?;
        let mut overwritten_total = 0usize;
        for i in 0..TOTAL {
            let mut msg = [0u8; 8];
            msg[..4].copy_from_slice(&(i as u32).to_le_bytes());
            loop {
                match client.send_to_server(&msg) {
                    Ok(outcome) => {
                        overwritten_total += outcome.overwritten as usize;
                        break;
                    }
                    Err(ShmError::QueueFull) => {
                        thread::sleep(MIN_SLEEP);
                    }
                    Err(err) => return Err(err),
                }
            }
            thread::yield_now();
        }

        client.send_to_server(b"END")?;

        let mut recv_buf = Vec::new();
        let start = Instant::now();
        loop {
            if start.elapsed() > Duration::from_secs(5) {
                return Err(ShmError::Timeout);
            }
            if client.poll_server(Some(Duration::from_millis(10)))? {
                match client.receive_from_server(&mut recv_buf) {
                    Ok(len) => {
                        if &recv_buf[..len] == b"ACK" {
                            break;
                        }
                    }
                    Err(ShmError::QueueEmpty) => {}
                    Err(err) => return Err(err),
                }
            }
        }

        Ok((TOTAL, overwritten_total))
    })();

    match server_thread.join() {
        Ok(Ok(received)) => {
            assert_eq!(received, TOTAL);
        }
        Ok(Err(err)) => panic!("server error: {err:?}"),
        Err(panic) => std::panic::resume_unwind(panic),
    }

    match client_result {
        Ok((sent, overwritten)) => {
            assert_eq!(sent, TOTAL);
            assert_eq!(overwritten, 0, "client overwrote {overwritten} messages");
        }
        Err(err) => panic!("client error: {err:?}"),
    }
}

#[test]
fn stress_bidirectional() {
    const TOTAL: usize = 2000;
    let name = unique_name("BI");

    let server_thread = thread::spawn({
        let name = name.clone();
        move || -> xshm::Result<()> {
            let mut server = SharedServer::start(&name)?;
            server.wait_for_client(Some(Duration::from_secs(5)))?;

            let mut recv_buf = Vec::new();
            let mut received = 0u32;
            let mut sent = 0u32;
            let start = Instant::now();

            while sent < TOTAL as u32 || received < TOTAL as u32 {
                if sent < TOTAL as u32 {
                    let mut msg = [0u8; 8];
                    msg[..4].copy_from_slice(&sent.to_le_bytes());
                    loop {
                        match server.send_to_client(&msg) {
                            Ok(_) => break,
                            Err(ShmError::QueueFull) => thread::sleep(MIN_SLEEP),
                            Err(err) => return Err(err),
                        }
                    }
                    sent += 1;
                }

                if server.poll_client(Some(Duration::from_millis(1)))? {
                    match server.receive_from_client(&mut recv_buf) {
                        Ok(len) => {
                            assert!(len >= 4);
                            let value = u32::from_le_bytes(recv_buf[..4].try_into().unwrap());
                            assert_eq!(value, received, "server received out of order");
                            received += 1;
                        }
                        Err(ShmError::QueueEmpty) => {}
                        Err(err) => return Err(err),
                    }
                } else {
                    thread::sleep(MIN_SLEEP);
                }

                if start.elapsed() > Duration::from_secs(10) {
                    return Err(ShmError::Timeout);
                }
            }

            server.send_to_client(b"DONE")?;
            Ok(())
        }
    });

    thread::sleep(Duration::from_millis(50));

    let client_result = (|| -> xshm::Result<()> {
        let client = SharedClient::connect(&name, Duration::from_secs(5))?;
        let mut recv_buf = Vec::new();
        let mut received = 0u32;
        let mut sent = 0u32;
        let start = Instant::now();

        while sent < TOTAL as u32 || received < TOTAL as u32 {
            if sent < TOTAL as u32 {
                let mut msg = [0u8; 8];
                msg[..4].copy_from_slice(&sent.to_le_bytes());
                loop {
                    match client.send_to_server(&msg) {
                        Ok(_) => break,
                        Err(ShmError::QueueFull) => thread::sleep(MIN_SLEEP),
                        Err(err) => return Err(err),
                    }
                }
                sent += 1;
            }

            if client.poll_server(Some(Duration::from_millis(1)))? {
                match client.receive_from_server(&mut recv_buf) {
                    Ok(len) => {
                        if &recv_buf[..len] == b"DONE" {
                            break;
                        }
                        assert!(len >= 4);
                        let value = u32::from_le_bytes(recv_buf[..4].try_into().unwrap());
                        assert_eq!(value, received, "client received out of order");
                        received += 1;
                    }
                    Err(ShmError::QueueEmpty) => {}
                    Err(err) => return Err(err),
                }
            } else {
                thread::sleep(MIN_SLEEP);
            }

            if start.elapsed() > Duration::from_secs(10) {
                return Err(ShmError::Timeout);
            }
        }

        Ok(())
    })();

    if let Err(err) = client_result {
        panic!("client error: {err:?}");
    }

    match server_thread.join() {
        Ok(Ok(())) => {}
        Ok(Err(err)) => panic!("server error: {err:?}"),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}
