//! Ограниченная локальная очередь одной клиентской сессии, без протокола IPC.
use super::{Beacon, CommandCompletion, Error, QueueLimits, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, ThreadId};

#[derive(Debug, Clone, Copy)]
pub(super) enum SendMode {
    Try,
    Lossless,
    Overwrite,
}
#[derive(Debug)]
pub(super) struct ClientCommand {
    pub data: Vec<u8>,
    pub mode: SendMode,
    reply: mpsc::Sender<Result<()>>,
}
#[derive(Debug, Default)]
struct Queue {
    commands: VecDeque<ClientCommand>,
    count: usize,
    bytes: usize,
    closed: bool,
    wake_pending: bool,
}
#[derive(Debug)]
struct Shared {
    queue: Mutex<Queue>,
    wake: Beacon,
    limits: QueueLimits,
    worker: ThreadId,
    stop: AtomicBool,
}

/// Отправитель одной клиентской сессии. Старый экземпляр не адресует новое
/// подключение. Ограничения очереди учитывают и сообщение, ожидающее места.
#[derive(Debug, Clone)]
pub struct ClientSender(Arc<Shared>);
impl ClientSender {
    pub(super) fn new(limits: QueueLimits) -> Result<Self> {
        Ok(Self(Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            wake: Beacon::unnamed()?,
            limits,
            worker: thread::current().id(),
            stop: AtomicBool::new(false),
        })))
    }
    fn submit(&self, data: &[u8], mode: SendMode) -> Result<CommandCompletion> {
        u32::try_from(data.len()).map_err(|_| Error::LengthTooLarge)?;
        let mut queue = self
            .0
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.0.stop.load(Ordering::Acquire) || queue.closed {
            return Err(Error::WorkerStopped);
        }
        let bytes = queue.bytes.checked_add(data.len()).ok_or(Error::Full)?;
        if queue.count >= self.0.limits.commands || bytes > self.0.limits.bytes {
            return Err(Error::Full);
        }
        let (reply, receiver) = mpsc::channel();
        queue.commands.push_back(ClientCommand {
            data: data.to_vec(),
            mode,
            reply,
        });
        // Под тем же lock worker сбрасывает сигнал после наблюдения очереди:
        // новая команда не теряется между проверкой и переходом в C wait.
        if !queue.wake_pending {
            if let Err(error) = self.0.wake.raise() {
                let _removed = queue.commands.pop_back();
                return Err(error);
            }
            queue.wake_pending = true;
        }
        queue.count += 1;
        queue.bytes = bytes;
        Ok(CommandCompletion::new(receiver, self.0.worker))
    }
    /// Поставить одну попытку C try_send. Подтверждение может вернуть Full;
    /// событие Writable сообщит о последующем появлении места.
    pub fn submit_try_send(&self, data: &[u8]) -> Result<CommandCompletion> {
        self.submit(data, SendMode::Try)
    }
    /// Дождаться места и записать без вытеснения. Пока место не освободилось,
    /// worker продолжает принимать данные и команды остановки.
    pub fn submit_lossless(&self, data: &[u8]) -> Result<CommandCompletion> {
        self.submit(data, SendMode::Lossless)
    }
    /// Явно разрешить вытеснение старых сообщений кольца C.
    pub fn submit_overwriting(&self, data: &[u8]) -> Result<CommandCompletion> {
        self.submit(data, SendMode::Overwrite)
    }
    /// Запросить остановку без ожидания; разрешено из собственного callback.
    pub fn request_stop(&self) -> Result<()> {
        let mut queue = self
            .0
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.0.stop.store(true, Ordering::Release);
        if !queue.wake_pending {
            self.0.wake.raise()?;
            queue.wake_pending = true;
        }
        Ok(())
    }
    /// Остановка запрошена либо очередь уже закрыта для новых отправок.
    pub fn is_stopped(&self) -> bool {
        let queue = self
            .0
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.0.stop.load(Ordering::Acquire) || queue.closed
    }
    pub(super) fn next(&self, take: bool) -> Result<Option<ClientCommand>> {
        let mut queue = self
            .0
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.0.stop.load(Ordering::Acquire) {
            return Err(Error::WorkerStopped);
        }
        if queue.wake_pending {
            self.0.wake.lower()?;
            queue.wake_pending = false;
        }
        Ok(if take {
            queue.commands.pop_front()
        } else {
            None
        })
    }
    pub(super) fn has_commands(&self) -> bool {
        !self
            .0
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .commands
            .is_empty()
    }
    pub(super) fn stop_requested(&self) -> bool {
        self.0.stop.load(Ordering::Acquire)
    }
    pub(super) fn wake(&self) -> &Beacon {
        &self.0.wake
    }
    pub(super) fn finish(&self, command: ClientCommand, result: Result<()>) {
        {
            let mut queue = self
                .0
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            queue.count -= 1;
            queue.bytes -= command.data.len();
        }
        let _delivered = command.reply.send(result);
    }
    pub(super) fn close(&self) {
        let commands = {
            let mut queue = self
                .0
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            queue.closed = true;
            queue.count = 0;
            queue.bytes = 0;
            std::mem::take(&mut queue.commands)
        };
        for command in commands {
            let _delivered = command.reply.send(Err(Error::WorkerStopped));
        }
    }
}
pub(super) struct CloseClientQueue(pub ClientSender);
impl Drop for CloseClientQueue {
    fn drop(&mut self) {
        self.0.close();
    }
}
