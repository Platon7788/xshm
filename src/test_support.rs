//! Общие помощники модульных тестов (только `cfg(test)`).

use std::process::{Child, Command};

/// PID и время создания процесса, который уже завершился (дочерний `cmd`,
/// handle закрыт): «упавший сервер». Даже если PID успеют переиспользовать,
/// время создания не совпадёт.
pub(crate) fn dead_process() -> (u32, u64) {
    let mut child = Command::new("cmd")
        .args(["/C", "exit", "0"])
        .spawn()
        .expect("cmd");
    let pid = child.id();
    let start = crate::win::process_start_time(pid).expect("время создания");
    drop(child.wait());
    drop(child);
    (pid, start)
}

/// Живой дочерний процесс (`cmd /C pause` с закрытым stdin не выходит сам,
/// пока его не убьют) и его время создания. Убивает его `Drop` [`LiveChild`].
pub(crate) struct LiveChild {
    child: Child,
    pub(crate) pid: u32,
    pub(crate) start: u64,
}

impl LiveChild {
    pub(crate) fn spawn() -> Self {
        // `timeout` без консоли завершается сразу -- держим процесс
        // `ping` на петлевой адрес (около минуты) с потолком на случай, если
        // тест упал и не убил его.
        let child = Command::new("cmd")
            .args(["/C", "ping", "-n", "60", "127.0.0.1", ">", "NUL"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("cmd");
        let pid = child.id();
        let start = crate::win::process_start_time(pid).expect("время создания");
        Self { child, pid, start }
    }

    /// Убить процесс и дождаться выхода.
    pub(crate) fn kill(&mut self) {
        drop(self.child.kill());
        drop(self.child.wait());
    }
}

impl Drop for LiveChild {
    fn drop(&mut self) {
        self.kill();
    }
}
