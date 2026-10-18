use super::*;
pub(super) mod payload {
    /// Поток длин кадров эхо-сценария: `1 + xorshift32 % max`.
    #[derive(Debug)]
    pub struct Lengths {
        state: u32,
        max: u32,
    }

    impl Lengths {
        pub const fn new(seed: u32, max: u32) -> Self {
            Self {
                state: if seed == 0 { 1 } else { seed },
                max: if max == 0 { 1 } else { max },
            }
        }

        /// Длина следующего кадра.
        pub const fn next_len(&mut self) -> usize {
            let mut x = self.state;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.state = x;
            (1 + x % self.max) as usize
        }
    }

    /// Байт `i` кадра `k`.
    const fn byte(k: u32, i: usize) -> u8 {
        (k.wrapping_mul(31).wrapping_add(i as u32) & 0xFF) as u8
    }

    /// Кадр `k` длины `len`.
    pub fn frame(k: u32, len: usize) -> Vec<u8> {
        (0..len).map(|i| byte(k, i)).collect()
    }

    /// Кадр `k` -- узор генератора.
    pub fn is_frame(k: u32, data: &[u8]) -> bool {
        data.iter().enumerate().all(|(i, &b)| b == byte(k, i))
    }
}

#[derive(Debug, Clone)]
pub(super) struct Args {
    pub(super) n: u32,
    pub(super) size: Option<u32>,
    pub(super) seed: u32,
    pub(super) layout: Layout,
    pub(super) mode: String,
    pub(super) expect: String,
    pub(super) sessions: u32,
    pub(super) delay: u64,
    pub(super) busy: bool,
    pub(super) count: u32,
    pub(super) ends: Option<u32>,
    pub(super) kick: Option<u32>,
    pub(super) frames: u32,
    pub(super) hold: u64,
    pub(super) release: String,
    pub(super) max_clients: u32,
    pub(super) cname: String,
    pub(super) boundary: String,
    pub(super) low: bool,
}

/// `<ёмкость>:<сообщений>:<макс. размер>`.
fn parse_ring(text: &str) -> Option<RingConfig> {
    let mut parts = text.split(':');
    let capacity = parts.next()?.parse().ok()?;
    let max_messages = parts.next()?.parse().ok()?;
    let max_message_size = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(RingConfig {
        capacity,
        max_messages,
        max_message_size,
    })
}

pub(super) fn parse_args(pairs: &[String]) -> TestResult<Args> {
    let mut args = Args {
        n: 3000,
        size: None,
        seed: 1,
        layout: ChannelConfig::default().layout,
        mode: String::new(),
        expect: String::new(),
        sessions: 1,
        delay: 500,
        busy: false,
        count: 1,
        ends: None,
        kick: None,
        frames: 10,
        hold: 0,
        release: String::new(),
        max_clients: DispatchOptions::default().max_clients,
        cname: "rust".to_owned(),
        boundary: String::new(),
        low: false,
    };
    for pair in pairs {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| format!("bad argument {pair}"))?;
        let bad = || format!("bad value of {key}: {value}");
        match key {
            "n" => args.n = value.parse().map_err(|_| bad())?,
            "size" => args.size = Some(value.parse().map_err(|_| bad())?),
            "seed" => args.seed = value.parse().map_err(|_| bad())?,
            "s2c" => args.layout.s2c = parse_ring(value).ok_or_else(bad)?,
            "c2s" => args.layout.c2s = parse_ring(value).ok_or_else(bad)?,
            "mode" => value.clone_into(&mut args.mode),
            "expect" => value.clone_into(&mut args.expect),
            "sessions" => args.sessions = value.parse().map_err(|_| bad())?,
            "delay" => args.delay = value.parse().map_err(|_| bad())?,
            "busy" => args.busy = value == "1",
            "count" => args.count = value.parse().map_err(|_| bad())?,
            "ends" => args.ends = Some(value.parse().map_err(|_| bad())?),
            "kick" => args.kick = Some(value.parse().map_err(|_| bad())?),
            "frames" => args.frames = value.parse().map_err(|_| bad())?,
            "hold" => args.hold = value.parse().map_err(|_| bad())?,
            "release" => value.clone_into(&mut args.release),
            "max_clients" => args.max_clients = value.parse().map_err(|_| bad())?,
            "name" => value.clone_into(&mut args.cname),
            "boundary" => value.clone_into(&mut args.boundary),
            "low" => {
                args.low = match value {
                    "0" => false,
                    "1" => true,
                    _ => return Err(bad()),
                }
            }
            _ => return Err(format!("unknown key {key}")),
        }
    }
    Ok(args)
}
