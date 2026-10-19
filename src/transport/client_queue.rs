//! Ограниченная локальная очередь одной клиентской сессии, без протокола IPC.
use super::{Beacon, CommandCompletion, Error, OwnedSendError, QueueLimits, Result};
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
    bytes: usize,
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
    fn submit<T>(
        &self,
        data: T,
        retained_bytes: usize,
        mode: SendMode,
    ) -> std::result::Result<CommandCompletion, (Error, T)>
    where
        T: AsRef<[u8]> + Into<Vec<u8>>,
    {
        if u32::try_from(data.as_ref().len()).is_err() {
            return Err((Error::LengthTooLarge, data));
        }
        let mut queue = self
            .0
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.0.stop.load(Ordering::Acquire) || queue.closed {
            return Err((Error::WorkerStopped, data));
        }
        let Some(bytes) = queue.bytes.checked_add(retained_bytes) else {
            return Err((Error::Full, data));
        };
        if queue.count >= self.0.limits.commands || bytes > self.0.limits.bytes {
            return Err((Error::Full, data));
        }
        // Сигнал и публикация команды под одним lock: worker не сможет
        // проверить очередь до вставки. Ошибка wake возвращает исходный буфер.
        if !queue.wake_pending {
            if let Err(error) = self.0.wake.raise() {
                return Err((error, data));
            }
            queue.wake_pending = true;
        }
        let (reply, receiver) = mpsc::channel();
        queue.commands.push_back(ClientCommand {
            data: data.into(),
            mode,
            bytes: retained_bytes,
            reply,
        });
        queue.count += 1;
        queue.bytes = bytes;
        Ok(CommandCompletion::new(receiver, self.0.worker))
    }
    /// Поставить одну попытку C try_send. Подтверждение может вернуть Full;
    /// событие Writable сообщит о последующем появлении места.
    pub fn submit_try_send(&self, data: &[u8]) -> Result<CommandCompletion> {
        self.submit(data, data.len(), SendMode::Try)
            .map_err(|(error, _)| error)
    }
    /// Дождаться места и записать без вытеснения. Пока место не освободилось,
    /// worker продолжает принимать данные и команды остановки.
    pub fn submit_lossless(&self, data: &[u8]) -> Result<CommandCompletion> {
        self.submit(data, data.len(), SendMode::Lossless)
            .map_err(|(error, _)| error)
    }
    /// Явно разрешить вытеснение старых сообщений кольца C.
    pub fn submit_overwriting(&self, data: &[u8]) -> Result<CommandCompletion> {
        self.submit(data, data.len(), SendMode::Overwrite)
            .map_err(|(error, _)| error)
    }
    /// Одна попытка C try_send без копии при постановке в очередь.
    /// В лимите очереди учитывается capacity буфера, включая запас памяти.
    /// При отказе возвращается исходный Vec; при успехе им владеет worker.
    /// Подтверждение относится к C-операции, а не чтению пиром.
    pub fn submit_try_send_owned(
        &self,
        data: Vec<u8>,
    ) -> std::result::Result<CommandCompletion, OwnedSendError> {
        let bytes = data.capacity();
        self.submit(data, bytes, SendMode::Try)
            .map_err(|(error, data)| OwnedSendError { error, data })
    }
    /// Ожидание места в C-кольце без копии при постановке в очередь.
    /// В лимите очереди учитывается capacity буфера, включая запас памяти.
    /// При отказе возвращается исходный Vec; при успехе им владеет worker.
    /// Подтверждение относится к C-операции, а не чтению пиром.
    pub fn submit_lossless_owned(
        &self,
        data: Vec<u8>,
    ) -> std::result::Result<CommandCompletion, OwnedSendError> {
        let bytes = data.capacity();
        self.submit(data, bytes, SendMode::Lossless)
            .map_err(|(error, data)| OwnedSendError { error, data })
    }
    /// Вытесняющая отправка без копии при постановке в очередь.
    /// В лимите очереди учитывается capacity буфера, включая запас памяти.
    /// При отказе возвращается исходный Vec; при успехе им владеет worker.
    /// Подтверждение относится к C-операции, а не чтению пиром.
    pub fn submit_overwriting_owned(
        &self,
        data: Vec<u8>,
    ) -> std::result::Result<CommandCompletion, OwnedSendError> {
        let bytes = data.capacity();
        self.submit(data, bytes, SendMode::Overwrite)
            .map_err(|(error, data)| OwnedSendError { error, data })
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
            queue.bytes -= command.bytes;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_buffers_keep_allocation_and_bound_capacity_until_completion() {
        let sender = ClientSender::new(QueueLimits {
            commands: 2,
            bytes: 64,
        })
        .unwrap();
        for mode in [SendMode::Try, SendMode::Lossless, SendMode::Overwrite] {
            let mut data = Vec::with_capacity(64);
            data.extend_from_slice(b"move");
            let pointer = data.as_ptr();
            let capacity = data.capacity();
            let completion = match mode {
                SendMode::Try => sender.submit_try_send_owned(data),
                SendMode::Lossless => sender.submit_lossless_owned(data),
                SendMode::Overwrite => sender.submit_overwriting_owned(data),
            }
            .unwrap();
            let command = sender.next(true).unwrap().unwrap();
            assert_eq!(command.data.as_ptr(), pointer);
            assert_eq!(command.data.capacity(), capacity);
            assert_eq!(command.data, b"move");
            assert!(matches!(
                (command.mode, mode),
                (SendMode::Try, SendMode::Try)
                    | (SendMode::Lossless, SendMode::Lossless)
                    | (SendMode::Overwrite, SendMode::Overwrite)
            ));
            // Извлечение команды worker-ом не освобождает бюджет до finish.
            let data = vec![9];
            let pointer = data.as_ptr();
            let rejected = sender.submit_lossless_owned(data).unwrap_err();
            assert!(matches!(rejected.error, Error::Full));
            assert_eq!(rejected.data.as_ptr(), pointer);
            assert_eq!(rejected.data, [9]);
            sender.finish(command, Ok(()));
            assert_eq!(completion.try_result().unwrap(), Some(()));
            sender.submit_lossless(b"copy").unwrap();
            let command = sender.next(true).unwrap().unwrap();
            sender.finish(command, Ok(()));
        }
        let mut oversized = Vec::with_capacity(65);
        oversized.push(1);
        let pointer = oversized.as_ptr();
        let capacity = oversized.capacity();
        let rejected = sender.submit_try_send_owned(oversized).unwrap_err();
        assert!(matches!(rejected.error, Error::Full));
        assert_eq!(rejected.data.as_ptr(), pointer);
        assert_eq!(rejected.data.capacity(), capacity);
        sender.request_stop().unwrap();
        let rejected = sender.submit_overwriting_owned(rejected.data).unwrap_err();
        assert!(matches!(rejected.error, Error::WorkerStopped));
        assert_eq!(rejected.data.as_ptr(), pointer);
    }

    #[test]
    fn close_cancels_owned_commands_and_returns_later_buffers() {
        let sender = ClientSender::new(QueueLimits::default()).unwrap();
        let pending = sender.submit_lossless_owned(vec![1, 2, 3]).unwrap();
        sender.close();
        assert!(matches!(pending.try_result(), Err(Error::WorkerStopped)));
        let data = vec![4, 5];
        let pointer = data.as_ptr();
        let error = sender.submit_try_send_owned(data).unwrap_err();
        assert!(matches!(error.error, Error::WorkerStopped));
        assert_eq!(error.data.as_ptr(), pointer);
        assert_eq!(error.data, [4, 5]);
    }
}
