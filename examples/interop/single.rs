use super::*;
#[derive(Debug)]
enum Link<'a> {
    Client(Client),
    Server(Session<'a>),
}
#[derive(Debug)]
struct Side<'a> {
    link: Link<'a>,
    layout: Layout,
    server: bool,
}
impl Side<'_> {
    fn connect(name: &str, args: &Args) -> TestResult<Self> {
        let started = Instant::now();
        let options = ClientConnectOptions {
            namespace: namespace(args, false)?,
            discovery_fallback: None,
            ..ClientConnectOptions::default()
        };
        let mut client = checked(Client::connect_when_available(
            name,
            options,
            ConnectBudget::For(Duration::from_secs(25)),
            None,
        ))?;
        let layout = client.layout().ok_or("missing layout")?;
        if args.busy {
            out(&format!("INFO busy_ms={}", started.elapsed().as_millis()));
        }
        out("READY");
        out(&format!(
            "INFO layout={}:{}:{}/{}:{}:{}",
            layout.s2c.capacity,
            layout.s2c.max_messages,
            layout.s2c.max_message_size,
            layout.c2s.capacity,
            layout.c2s.max_messages,
            layout.c2s.max_message_size
        ));
        Ok(Self {
            link: Link::Client(client),
            layout,
            server: false,
        })
    }
    const fn tx_max(&self) -> usize {
        (if self.server {
            self.layout.s2c.max_message_size
        } else {
            self.layout.c2s.max_message_size
        }) as usize
    }
    const fn rx_max(&self) -> usize {
        (if self.server {
            self.layout.c2s.max_message_size
        } else {
            self.layout.s2c.max_message_size
        }) as usize
    }
    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        match &mut self.link {
            Link::Client(c) => c.try_send(bytes),
            Link::Server(s) => s.try_send(bytes),
        }
    }
    fn space(&mut self, len: usize) -> Result<()> {
        match &mut self.link {
            Link::Client(c) => c.wait_for_space(len, finite(), None),
            Link::Server(s) => s.wait_for_space(len, finite(), None),
        }
    }
    fn lossless(&mut self, bytes: &[u8]) -> TestResult<u32> {
        let mut waits = 0;
        loop {
            match self.send(bytes) {
                Err(Error::Full) => {
                    waits += 1;
                    checked(self.space(bytes.len()))?;
                }
                other => {
                    checked(other)?;
                    return Ok(waits);
                }
            }
        }
    }
    fn recv(&mut self, buf: &mut [u8]) -> Result<usize> {
        loop {
            let result = match &mut self.link {
                Link::Client(c) => c.receive_into(buf),
                Link::Server(s) => s.receive_into(buf),
            };
            if !matches!(result, Err(Error::Empty)) {
                return result;
            }
            match &mut self.link {
                Link::Client(c) => c.wait_readable(finite(), None)?,
                Link::Server(s) => s.wait_readable(finite(), None)?,
            }
        }
    }
}
fn exercise(side: &mut Side<'_>, scenario: &str, args: &Args) -> TestResult {
    let mut buf = vec![0; side.rx_max()];
    let mut lengths = payload::Lengths::new(args.seed, args.size.unwrap_or(65535));
    if scenario.ends_with("stall-leave") {
        thread::sleep(Duration::from_millis(args.delay));
        out("INFO leaving=1");
        return Ok(());
    }
    if scenario.ends_with("die") {
        for k in 0..2 {
            checked(side.send(&payload::frame(k, 7)))?;
        }
        out("INFO sent=2");
        std::process::exit(9);
    }
    if scenario.ends_with("fill-wait") {
        let frame = payload::frame(0, args.size.unwrap_or(1024) as usize);
        let mut count = 0;
        loop {
            match side.send(&frame) {
                Ok(()) => count += 1,
                Err(Error::Full) => break,
                other => {
                    checked(other)?;
                }
            }
        }
        out(&format!("INFO full={count}"));
        let result = side.space(frame.len());
        let got = match &result {
            Ok(()) => "Space",
            Err(e) => err_name(e),
        };
        out(&format!("INFO err={got}"));
        return if got == args.expect {
            Ok(())
        } else {
            Err(format!("expected {} got {got}", args.expect))
        };
    }
    if scenario == "client-ping" {
        for k in 0..args.n {
            let frame = payload::frame(k, lengths.next_len());
            checked(side.send(&frame))?;
            let n = checked(side.recv(&mut buf))?;
            if buf[..n] != xor(&frame, side.rx_max()) {
                return Err("wrong echo".into());
            }
        }
        out(&format!("INFO pinged={}", args.n));
        return Ok(());
    }
    if scenario.ends_with("farewell") && args.mode == "send" {
        for k in 0..3 {
            checked(side.send(&payload::frame(k, 7 + k as usize)))?;
        }
        return Ok(());
    }
    if scenario == "client-expect-death" {
        checked(side.send(&[1]))?;
    }
    if scenario.ends_with("lossless-source") {
        let mut waits = 0;
        for k in 0..args.n {
            waits += side.lossless(&payload::frame(k, args.size.unwrap_or(30720) as usize))?;
        }
        out(&format!("INFO waits={waits}"));
        out(&format!("INFO sent={}", args.n));
    }
    let auto = scenario.starts_with("auto-");
    let mut count = 0;
    let reason = loop {
        match side.recv(&mut buf) {
            Ok(n) => {
                let bytes = &buf[..n];
                if auto {
                    let response = xor(bytes, side.tx_max());
                    checked(side.send(&response))?;
                } else if scenario.ends_with("echo") {
                    if count < args.n && n != lengths.next_len() {
                        return Err("wrong length".into());
                    }
                    if !payload::is_frame(count, bytes) {
                        return Err("wrong payload".into());
                    }
                    let response = xor(bytes, side.tx_max());
                    checked(side.send(&response))?;
                } else if scenario.ends_with("lossless-sink") {
                    if n != args.size.unwrap_or(30720) as usize || !payload::is_frame(count, bytes)
                    {
                        return Err("lost/reordered payload".into());
                    }
                    if count % 16 == 15 {
                        thread::sleep(Duration::from_millis(1));
                    }
                } else if scenario.ends_with("expect-death") || scenario.ends_with("farewell") {
                    let size = if scenario.ends_with("expect-death") {
                        7
                    } else {
                        7 + count as usize
                    };
                    if n != size || !payload::is_frame(count, bytes) {
                        return Err("wrong final frame".into());
                    }
                }
                count += 1;
            }
            Err(Error::Disconnected(reason)) => break reason,
            Err(error) => {
                out(&format!("INFO err={}", err_name(&error)));
                if auto && args.expect == "Error" {
                    break DisconnectReason::Error;
                }
                return Err(error.to_string());
            }
        }
    };
    out(&format!("INFO frames={count}"));
    out(&format!("INFO reason={}", reason_name(reason)));
    let expected = if scenario.ends_with("expect-death") {
        "PeerDied"
    } else if auto && !args.expect.is_empty() {
        &args.expect
    } else {
        "Graceful"
    };
    if reason_name(reason) != expected {
        return Err(format!("reason != {expected}"));
    }
    let frames = if scenario.ends_with("echo") && !auto || scenario.ends_with("lossless-sink") {
        Some(args.n)
    } else if scenario.ends_with("farewell") {
        Some(3)
    } else if scenario.ends_with("expect-death") {
        Some(2)
    } else {
        None
    };
    if let Some(expected) = frames
        && count < expected
    {
        return Err(format!("only {count}/{expected} frames"));
    }
    Ok(())
}
pub(super) fn run(scenario: &str, name: &str, args: &Args) -> TestResult {
    if scenario == "client-hello-die" {
        let name = name.to_owned();
        thread::spawn(move || {
            let _attempt = Client::connect(&name, Wait::For(Duration::from_secs(30)), None);
        });
        thread::sleep(Duration::from_millis(500));
        out("INFO hello=1");
        std::process::exit(9);
    }
    if scenario == "client-expect-busy" || scenario == "client-expect-err" {
        let expected = if scenario == "client-expect-busy" {
            "AlreadyConnected"
        } else {
            &args.expect
        };
        match Client::connect(name, Wait::For(Duration::from_secs(2)), None) {
            Ok(_) => Err("unexpected connection".into()),
            Err(error) => {
                let got = err_name(&error);
                out(&format!("INFO err={got}"));
                if got == expected {
                    Ok(())
                } else {
                    Err(format!("expected {expected}, got {got}"))
                }
            }
        }
    } else if scenario.starts_with("server-")
        || scenario == "ns-server"
        || scenario == "auto-server-echo"
    {
        let mut server = checked(Server::bind(
            name,
            ChannelConfig {
                layout: args.layout,
                namespace: namespace(args, true)?,
                ..ChannelConfig::default()
            },
        ))?;
        out("READY");
        let count = if scenario == "server-sessions" {
            args.sessions
        } else {
            1
        };
        for index in 1..=count {
            let mut session = loop {
                match server.accept(Wait::Infinite, None) {
                    Ok(session) => break session,
                    Err(Error::HandshakeFailed) => out("INFO handshake=HandshakeFailed"),
                    Err(error) => return Err(error.to_string()),
                }
            };
            let layout = session.layout().ok_or("missing layout")?;
            out("INFO connected=1");
            if scenario == "server-sessions" {
                out(&format!("INFO session={index}"));
            }
            exercise(
                &mut Side {
                    link: Link::Server(session),
                    layout,
                    server: true,
                },
                if scenario == "server-sessions" || scenario == "ns-server" {
                    "server-echo"
                } else {
                    scenario
                },
                args,
            )?;
        }
        Ok(())
    } else {
        exercise(
            &mut Side::connect(name, args)?,
            if scenario == "ns-client" {
                "client-echo"
            } else {
                scenario
            },
            args,
        )
    }
}
