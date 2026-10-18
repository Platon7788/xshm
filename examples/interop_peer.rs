//! Межъязыковые проверки нового Rust API и нативных C-пиров xhook/ipc.
//! Стабильные имена CLI-сценариев принадлежат тестовому оркестратору,
//! а не API библиотеки. Никакого протокола разделяемой памяти здесь нет.
use std::{
    io::Write,
    process::ExitCode,
    thread,
    time::{Duration, Instant},
};
use xshm::*;
#[path = "interop/data.rs"]
mod data;
#[path = "interop/groups.rs"]
mod groups;
#[path = "interop/single.rs"]
mod single;
use data::*;
type TestResult<T = ()> = std::result::Result<T, String>;
fn checked<T>(result: Result<T>) -> TestResult<T> {
    result.map_err(|e| format!("{}: {e}", err_name(&e)))
}
fn out(line: &str) {
    let mut stdout = std::io::stdout().lock();
    let _written = writeln!(stdout, "{line}");
    let _flushed = stdout.flush();
}
const fn finite() -> Wait {
    Wait::For(Duration::from_secs(20))
}
const fn err_name(error: &Error) -> &str {
    match error {
        Error::Disconnected(_) => "NotConnected",
        Error::Busy => "AlreadyConnected",
        Error::NotReady => "NotReady",
        Error::Timeout => "Timeout",
        Error::Empty => "QueueEmpty",
        Error::Full => "QueueFull",
        Error::Corrupted => "Corrupted",
        Error::HandshakeFailed | Error::Rejected => "HandshakeFailed",
        Error::VersionMismatch => "VersionMismatch",
        Error::AccessDenied => "AccessDenied",
        Error::NoFreeSlot => "NoFreeSlot",
        Error::InvalidConfig => "InvalidConfig",
        _ => "Other",
    }
}
const fn reason_name(reason: DisconnectReason) -> &'static str {
    match reason {
        DisconnectReason::Graceful => "Graceful",
        DisconnectReason::PeerDied => "PeerDied",
        DisconnectReason::Local => "Local",
        DisconnectReason::Error => "Error",
        _ => "Unknown",
    }
}
fn namespace(args: &Args, server: bool) -> TestResult<Option<PrivateNamespace>> {
    if args.boundary.is_empty() {
        return Ok(None);
    }
    let security = if args.low {
        Security::CurrentUserAllowLow
    } else {
        Security::CurrentUser
    };
    let ns = checked(PrivateNamespace::open(&args.boundary, security))?;
    // C API намеренно не обещает различать create и open: важен живой handle.
    let _role = server;
    out("INFO ns=ready");
    Ok(Some(ns))
}
fn xor(data: &[u8], max: usize) -> Vec<u8> {
    data.iter().take(max).map(|b| b ^ 0x5A).collect()
}
fn main() -> ExitCode {
    // Ограничение всего сценария; ядро блокируется на событиях, без опроса.
    thread::spawn(|| {
        thread::sleep(Duration::from_secs(600));
        out("FAIL timeout");
        std::process::exit(1);
    });
    let argv: Vec<_> = std::env::args().skip(1).collect();
    let [scenario, name, rest @ ..] = argv.as_slice() else {
        out("FAIL usage");
        return ExitCode::from(2);
    };
    let result = parse_args(rest).and_then(|args| {
        if scenario.contains("multi")
            || scenario.contains("dispatch")
            || scenario.starts_with("beacon-")
        {
            groups::run(scenario, name, &args)
        } else {
            single::run(scenario, name, &args)
        }
    });
    match result {
        Ok(()) => {
            out("PASS");
            ExitCode::SUCCESS
        }
        Err(error) => {
            out(&format!("FAIL {error}"));
            ExitCode::FAILURE
        }
    }
}
