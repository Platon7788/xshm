use super::*;
fn recv(client: &mut Client, buffer: &mut [u8]) -> TestResult<usize> {
    loop {
        match client.receive_into(buffer) {
            Err(Error::Empty) => checked(client.wait_readable(finite(), None))?,
            other => return checked(other),
        }
    }
}
fn client_dialog(
    mut client: Client,
    args: &Args,
    ping: bool,
    cancel: &CancellationToken,
    ended: &std::sync::atomic::AtomicU32,
) -> TestResult<Client> {
    let layout = client.layout().ok_or("missing layout")?;
    let mut buffer = vec![0; layout.s2c.max_message_size as usize];
    if ping {
        for k in 0..args.frames {
            let frame = payload::frame(k, 16);
            checked(client.try_send(&frame))?;
            let n = recv(&mut client, &mut buffer)?;
            if buffer[..n] != xor(&frame, buffer.len()) {
                return Err("incorrect group echo".into());
            }
        }
    } else {
        loop {
            match client.receive_into(&mut buffer) {
                Ok(n) => {
                    let reply = xor(&buffer[..n], layout.c2s.max_message_size as usize);
                    checked(client.try_send(&reply))?;
                }
                Err(Error::Empty) => match client.wait_readable(finite(), Some(cancel)) {
                    Ok(()) => {}
                    Err(Error::Disconnected(reason)) => {
                        out(&format!("INFO reason={}", reason_name(reason)));
                        if ended.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
                            >= args.ends.unwrap_or(args.count)
                        {
                            checked(cancel.cancel())?;
                        }
                        break;
                    }
                    Err(Error::Cancelled) => break,
                    other => {
                        checked(other)?;
                    }
                },
                Err(Error::Disconnected(reason)) => {
                    out(&format!("INFO reason={}", reason_name(reason)));
                    if ended.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
                        >= args.ends.unwrap_or(args.count)
                    {
                        checked(cancel.cancel())?;
                    }
                    break;
                }
                Err(error) => return Err(error.to_string()),
            }
        }
    }
    Ok(client)
}
fn clients(name: &str, args: &Args, multi: bool) -> TestResult {
    let ping = if multi {
        args.mode == "ping"
    } else {
        args.mode != "echo"
    };
    let ns = namespace(args, false)?;
    let mut threads = Vec::new();
    let cancel = checked(CancellationToken::new())?;
    let ended = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    for _ in 0..args.count {
        let (name, args, ns) = (name.to_owned(), args.clone(), ns.clone());
        let (cancel, ended) = (cancel.clone(), ended.clone());
        threads.push(thread::spawn(move || -> TestResult<Client> {
            let client = if multi {
                let (client, slot) = checked(Client::connect_multi(
                    &name,
                    MultiClientOptions {
                        namespace: ns,
                        discovery_fallback: None,
                        ..MultiClientOptions::default()
                    },
                    ConnectBudget::For(Duration::from_secs(20)),
                    None,
                ))?;
                out(&format!("INFO slot={slot}"));
                client
            } else {
                let result = Client::connect_dispatch(
                    &name,
                    1,
                    &args.cname,
                    DispatchClientOptions {
                        namespace: ns,
                        ..DispatchClientOptions::default()
                    },
                    None,
                );
                if !args.expect.is_empty() {
                    return match result {
                        Ok(_) => Err("unexpected registration".into()),
                        Err(error) => {
                            let got = err_name(&error);
                            out(&format!("INFO err={got}"));
                            if got == args.expect {
                                Err("expected-registration-error".into())
                            } else {
                                Err(error.to_string())
                            }
                        }
                    };
                }
                let (client, info) = checked(result)?;
                out(&format!("INFO channel={}", info.channel_name));
                client
            };
            client_dialog(client, &args, ping, &cancel, &ended)
        }));
    }
    let mut clients = Vec::new();
    for handle in threads {
        let result = handle
            .join()
            .map_err(|_| "client thread panic".to_owned())?;
        if !args.expect.is_empty()
            && matches!(&result,Err(error) if error=="expected-registration-error")
        {
            continue;
        }
        clients.push(result?);
    }
    if ping && args.expect.is_empty() {
        out(&format!("INFO pinged={}", args.count));
        if multi {
            if args.release.is_empty() {
                thread::sleep(Duration::from_millis(args.hold));
            } else {
                checked(checked(Beacon::open(&args.release))?.wait(finite(), None))?;
            }
        }
    }
    drop(clients);
    Ok(())
}
pub(super) fn run(scenario: &str, name: &str, args: &Args) -> TestResult {
    if scenario == "beacon-client" {
        let beacon = checked(Beacon::open(name))?;
        out("INFO waiting=1");
        checked(beacon.wait(finite(), None))?;
        out(&format!(
            "INFO lobby={}",
            u8::from(checked(lobby_exists(name, None))?)
        ));
        out(&format!(
            "INFO cleared={}",
            checked(beacon.clear_if_stale())?
        ));
        out(&format!(
            "INFO raised={}",
            u8::from(checked(beacon.is_raised())?)
        ));
        return Ok(());
    }
    if scenario.ends_with("client") {
        return clients(name, args, scenario == "multi-client");
    }
    let ns = namespace(args, true)?;
    if scenario.contains("multi") {
        let mut server = checked(MultiServer::bind(
            name,
            args.n,
            MultiOptions {
                channel: ChannelConfig {
                    layout: args.layout,
                    namespace: ns,
                    ..ChannelConfig::default()
                },
                ..MultiOptions::default()
            },
        ))?;
        out("READY");
        let mut buffer = vec![0; args.layout.c2s.max_message_size as usize];
        let mut ended = 0;
        let mut kicked = false;
        while ended < args.ends.unwrap_or(args.n) {
            match checked(server.next_event(&mut buffer, Wait::Infinite))? {
                MultiEvent::Connected(peer) => out(&format!("INFO connect={}", peer.slot())),
                MultiEvent::Message { peer, data } => {
                    checked(
                        server
                            .try_send(&peer, &xor(data, args.layout.s2c.max_message_size as usize)),
                    )?;
                    if !kicked && args.kick == Some(peer.slot()) {
                        checked(server.disconnect(&peer))?;
                        kicked = true;
                    }
                }
                MultiEvent::Disconnected { peer, reason, .. } => {
                    out(&format!(
                        "INFO disconnect={}:{}",
                        peer.slot(),
                        reason_name(reason)
                    ));
                    ended += 1;
                }
                MultiEvent::Fault { error, .. } => out(&format!("INFO err={}", err_name(&error))),
                _ => {}
            }
        }
        return Ok(());
    }
    let options = DispatchOptions {
        max_clients: args.max_clients,
        channel: ChannelConfig {
            namespace: ns,
            ..ChannelConfig::default()
        },
        ..DispatchOptions::default()
    };
    let max = options.channel.layout.s2c.max_message_size as usize;
    let size = options.channel.layout.c2s.max_message_size as usize;
    let mut server = checked(DispatchServer::bind(name, options))?;
    let beacon = if scenario.starts_with("beacon-server") {
        let beacon = checked(Beacon::open(name))?;
        checked(beacon.raise())?;
        Some(beacon)
    } else {
        None
    };
    out("READY");
    if scenario == "beacon-server-crash" {
        std::process::exit(9);
    }
    let mut buffer = vec![0; size];
    let mut ended = 0;
    while ended < args.ends.unwrap_or(1) {
        match checked(server.next_event(&mut buffer, Wait::Infinite))? {
            DispatchEvent::Connected { peer, registration } => out(&format!(
                "INFO connect={}:{}:{}",
                peer.client_id(),
                registration.name,
                registration.pid
            )),
            DispatchEvent::Message { peer, data, .. } => {
                checked(server.try_send(&peer, &xor(data, max)))?;
            }
            DispatchEvent::Disconnected { peer, reason, .. } => {
                out(&format!(
                    "INFO disconnect={}:{}",
                    peer.client_id(),
                    reason_name(reason)
                ));
                ended += 1;
            }
            DispatchEvent::Fault { error, .. } => out(&format!("INFO err={}", err_name(&error))),
            _ => {}
        }
    }
    if let Some(beacon) = beacon {
        checked(beacon.lower())?;
    }
    Ok(())
}
