//! Один поток владеет C-диспетчером; команды будят его, не опрашивая таймер.
use super::{
    DispatchEvent, DispatchOptions, DispatchPeer, DispatchServer, DispatchWake, Error,
    Registration, Result, Wait,
};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle, ThreadId};

/// Ограничения очереди команд, отдельно от ограничений транспортного кольца.
#[derive(Debug, Clone, Copy)]
pub struct QueueLimits {
    /// Максимум команд, ожидающих выполнения.
    pub commands: usize,
    /// Максимум байт буферов: длина для `&[u8]`, capacity для `Vec<u8>`.
    pub bytes: usize,
}
impl Default for QueueLimits {
    fn default() -> Self {
        Self {
            commands: 1024,
            bytes: 8 * 1024 * 1024,
        }
    }
}

/// Отказ постановки owned-буфера в очередь. Команда не принята и не будет
/// выполнена; исходный Vec возвращён без копирования или изменения capacity.
/// После успешной постановки ошибки C приходят через CommandCompletion:
/// в этом случае буфер уже принадлежит worker и не возвращается.
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct OwnedSendError {
    /// Причина отказа, например Full или WorkerStopped.
    #[source]
    pub error: Error,
    /// Исходный буфер для повторной попытки или повторного использования.
    pub data: Vec<u8>,
}

/// Однократное подтверждение фактической операции C, а не постановки в очередь.
/// Drop подтверждения не отменяет команду; stop отменяет невыполненные команды.
#[derive(Debug)]
pub struct CommandCompletion {
    receiver: mpsc::Receiver<Result<()>>,
    worker: ThreadId,
}
impl CommandCompletion {
    pub(super) const fn new(receiver: mpsc::Receiver<Result<()>>, worker: ThreadId) -> Self {
        Self { receiver, worker }
    }
    /// Дождаться исполнения. В callback своего worker вернёт WouldDeadlock;
    /// команда останется в очереди и будет исполнена после callback.
    pub fn wait(self) -> Result<()> {
        if self.worker == thread::current().id() {
            return Err(Error::WouldDeadlock);
        }
        self.receiver.recv().map_err(|_| Error::WorkerStopped)?
    }
    /// Ждать подтверждение не дольше указанного срока. Timeout не отменяет
    /// команду; для отмены очереди остановите worker. В его callback ожидание
    /// запрещено так же, как для wait. Результат подтверждает C-операцию,
    /// но не чтение сообщения пиром.
    pub fn wait_timeout(self, duration: std::time::Duration) -> Result<()> {
        if self.worker == thread::current().id() {
            return Err(Error::WouldDeadlock);
        }
        match self.receiver.recv_timeout(duration) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(Error::Timeout),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(Error::WorkerStopped),
        }
    }
    /// Проверить подтверждение без ожидания. Some означает успешное завершение;
    /// после результата подтверждение повторно не используется.
    pub fn try_result(&self) -> Result<Option<()>> {
        match self.receiver.try_recv() {
            Ok(result) => result.map(Some),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err(Error::WorkerStopped),
        }
    }
}

#[derive(Debug)]
enum Operation {
    Send { data: Vec<u8>, overwrite: bool },
    Disconnect,
}
#[derive(Debug)]
struct Command {
    peer: DispatchPeer,
    operation: Operation,
    reply: mpsc::Sender<Result<()>>,
    bytes: usize,
}
impl Command {
    const fn bytes(&self) -> usize {
        self.bytes
    }
}
#[derive(Debug, Default)]
struct Queue {
    commands: VecDeque<Command>,
    bytes: usize,
    stop: bool,
    closed: bool,
}
#[derive(Debug)]
struct Shared {
    queue: Mutex<Queue>,
    wake: DispatchWake,
    limits: QueueLimits,
    worker: ThreadId,
}
impl Shared {
    fn stop(&self) -> Result<()> {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if queue.closed {
            return Ok(());
        }
        queue.stop = true;
        self.wake.wake()
    }
    fn stopped(&self) -> bool {
        let queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.stop || queue.closed
    }
}
struct CloseQueue(Arc<Shared>);
impl Drop for CloseQueue {
    fn drop(&mut self) {
        let commands = {
            let mut queue = self
                .0
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            queue.closed = true;
            queue.bytes = 0;
            std::mem::take(&mut queue.commands)
        };
        for command in commands {
            let _delivered = command.reply.send(Err(Error::WorkerStopped));
        }
    }
}

/// Разделяемый отправитель команд. Не владеет C-сервером и не продлевает его работу.
#[derive(Debug, Clone)]
pub struct DispatchSender(Arc<Shared>);
impl DispatchSender {
    fn submit<T>(
        &self,
        peer: &DispatchPeer,
        len: usize,
        bytes: usize,
        input: T,
        operation: impl FnOnce(T) -> Operation,
    ) -> std::result::Result<CommandCompletion, (Error, T)> {
        if u32::try_from(len).is_err() {
            return Err((Error::LengthTooLarge, input));
        }
        let mut queue = self
            .0
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if queue.stop || queue.closed {
            return Err((Error::WorkerStopped, input));
        }
        let Some(total) = queue.bytes.checked_add(bytes) else {
            return Err((Error::Full, input));
        };
        if queue.commands.len() >= self.0.limits.commands || total > self.0.limits.bytes {
            return Err((Error::Full, input));
        }
        // Worker берёт команды под этим же lock. Сигнал до вставки безопасен:
        // он не увидит пустую очередь, а при ошибке исходный input не потреблён.
        if let Err(error) = self.0.wake.wake() {
            return Err((error, input));
        }
        let (reply, receiver) = mpsc::channel();
        queue.commands.push_back(Command {
            peer: peer.clone(),
            operation: operation(input),
            reply,
            bytes,
        });
        queue.bytes = total;
        Ok(CommandCompletion {
            receiver,
            worker: self.0.worker,
        })
    }
    /// Поставить отправку без вытеснения. Full здесь означает очередь команд;
    /// Full в подтверждении означает заполненное кольцо. Автоповтора нет.
    pub fn submit_try_send(&self, peer: &DispatchPeer, data: &[u8]) -> Result<CommandCompletion> {
        self.submit(peer, data.len(), data.len(), data, |data| Operation::Send {
            data: data.to_vec(),
            overwrite: false,
        })
        .map_err(|(error, _)| error)
    }
    /// Явно разрешить C вытеснить старые сообщения транспортного кольца.
    pub fn submit_overwriting(
        &self,
        peer: &DispatchPeer,
        data: &[u8],
    ) -> Result<CommandCompletion> {
        self.submit(peer, data.len(), data.len(), data, |data| Operation::Send {
            data: data.to_vec(),
            overwrite: true,
        })
        .map_err(|(error, _)| error)
    }
    /// Одна попытка C try_send с передачей Vec без копирования в очередь.
    /// В лимите учитывается capacity; при отказе исходный Vec возвращается.
    /// Ошибки исполнения C приходят через CommandCompletion без возврата Vec.
    pub fn submit_try_send_owned(
        &self,
        peer: &DispatchPeer,
        data: Vec<u8>,
    ) -> std::result::Result<CommandCompletion, OwnedSendError> {
        self.submit(peer, data.len(), data.capacity(), data, |data| {
            Operation::Send {
                data,
                overwrite: false,
            }
        })
        .map_err(|(error, data)| OwnedSendError { error, data })
    }
    /// Вытесняющая отправка с передачей Vec без копирования в очередь.
    /// В лимите учитывается capacity; при отказе исходный Vec возвращается.
    /// Ошибки исполнения C приходят через CommandCompletion без возврата Vec.
    pub fn submit_overwriting_owned(
        &self,
        peer: &DispatchPeer,
        data: Vec<u8>,
    ) -> std::result::Result<CommandCompletion, OwnedSendError> {
        self.submit(peer, data.len(), data.capacity(), data, |data| {
            Operation::Send {
                data,
                overwrite: true,
            }
        })
        .map_err(|(error, data)| OwnedSendError { error, data })
    }
    /// Отключить конкретную сессию; повторно занятый id не адресуется.
    pub fn submit_disconnect(&self, peer: &DispatchPeer) -> Result<CommandCompletion> {
        self.submit(peer, 0, 0, (), |()| Operation::Disconnect)
            .map_err(|(error, ())| error)
    }
    /// Попросить остановку, в том числе из callback; не ждёт завершения потока.
    pub fn request_stop(&self) -> Result<()> {
        self.0.stop()
    }
}

/// Владелец фонового Dispatch. Drop из другого потока запрашивает остановку
/// и ждёт worker: после возврата callbacks больше не вызываются.
/// Из собственного callback используйте request_stop отправителя.
/// Если владелец всё же удалён на своём worker, self-join не производится;
/// поток завершится после возврата текущего callback.
///
/// ```no_run
/// use xshm::{DispatchEvent, DispatchOptions, DispatchWorker, QueueLimits};
/// let mut worker = DispatchWorker::start("my_lobby", DispatchOptions::default(),
///     QueueLimits::default(), |_, event| {
///         if let DispatchEvent::Connected { peer, registration } = event {
///             println!("{}: {}", peer.client_id(), registration.name);
///         }
///     })?;
/// let sender = worker.sender();
/// sender.request_stop()?;
/// worker.stop()?;
/// # Ok::<(), xshm::Error>(())
/// ```
#[derive(Debug)]
pub struct DispatchWorker {
    sender: DispatchSender,
    thread: Option<JoinHandle<Result<()>>>,
    failure: Option<Arc<Error>>,
}
type Admission = dyn FnMut(&Registration) -> bool + Send;
impl DispatchWorker {
    /// Запустить C-сервер в его потоке-владельце. Ошибка bind возвращается
    /// синхронно. Handler работает без внутренних блокировок очереди и сразу
    /// получает отправителя, без внешней инициализации через OnceLock.
    pub fn start<F>(
        lobby: &str,
        options: DispatchOptions,
        limits: QueueLimits,
        handler: F,
    ) -> Result<Self>
    where
        F: for<'a> FnMut(&DispatchSender, DispatchEvent<'a>) + Send + 'static,
    {
        Self::start_inner(lobby, options, limits, None, handler)
    }
    /// Как start, с обработчиком допуска, вызываемым внутри C next.
    pub fn start_with_admission<A, F>(
        lobby: &str,
        options: DispatchOptions,
        limits: QueueLimits,
        admission: A,
        handler: F,
    ) -> Result<Self>
    where
        A: FnMut(&Registration) -> bool + Send + 'static,
        F: for<'a> FnMut(&DispatchSender, DispatchEvent<'a>) + Send + 'static,
    {
        Self::start_inner(lobby, options, limits, Some(Box::new(admission)), handler)
    }
    fn start_inner<F>(
        lobby: &str,
        options: DispatchOptions,
        limits: QueueLimits,
        admission: Option<Box<Admission>>,
        mut handler: F,
    ) -> Result<Self>
    where
        F: for<'a> FnMut(&DispatchSender, DispatchEvent<'a>) + Send + 'static,
    {
        if limits.commands == 0 || limits.bytes == 0 {
            return Err(Error::InvalidConfig);
        }
        let capacity = options.channel.layout.c2s.max_message_size as usize;
        let lobby = lobby.to_owned();
        let (ready, receiver) = mpsc::channel();
        let thread = crate::thread_hook::spawn(
            thread::Builder::new().name("xshm-dispatch".to_owned()),
            move || {
                let server = match admission {
                    Some(admission) => {
                        DispatchServer::bind_with_admission(&lobby, options, admission)
                    }
                    None => DispatchServer::bind(&lobby, options),
                };
                let mut server = match server {
                    Ok(server) => server,
                    Err(error) => {
                        let _delivered = ready.send(Err(error));
                        return Ok(());
                    }
                };
                let shared = Arc::new(Shared {
                    queue: Mutex::new(Queue::default()),
                    wake: server.wake_handle(),
                    limits,
                    worker: thread::current().id(),
                });
                // Очередь закрывается перед C free, также при unwind worker-а.
                let _close = CloseQueue(shared.clone());
                if ready.send(Ok(shared.clone())).is_err() {
                    return Ok(());
                }
                let sender = DispatchSender(shared.clone());
                let mut buffer = vec![0; capacity];
                loop {
                    // Ограниченная пачка команд: входящие события не голодают
                    // при непрерывной отправке другими потоками.
                    for _ in 0..32 {
                        let command = {
                            let mut queue = shared
                                .queue
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            if queue.stop {
                                return Ok(());
                            }
                            let command = queue.commands.pop_front();
                            if let Some(command) = &command {
                                queue.bytes -= command.bytes();
                            }
                            command
                        };
                        let Some(command) = command else {
                            break;
                        };
                        let result = match command.operation {
                            Operation::Send {
                                data,
                                overwrite: false,
                            } => server.try_send(&command.peer, &data),
                            Operation::Send {
                                data,
                                overwrite: true,
                            } => server.send_overwriting(&command.peer, &data),
                            Operation::Disconnect => server.disconnect(&command.peer),
                        };
                        let _delivered = command.reply.send(result);
                    }
                    let wait = {
                        let queue = shared
                            .queue
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if queue.stop {
                            return Ok(());
                        }
                        if queue.commands.is_empty() {
                            Wait::Infinite
                        } else {
                            Wait::Immediate
                        }
                    };
                    match server.next_event(&mut buffer, wait)? {
                        DispatchEvent::Wake | DispatchEvent::Timeout => {}
                        event => handler(&sender, event),
                    }
                }
            },
        )?;
        let shared = match receiver.recv() {
            Ok(Ok(shared)) => shared,
            Ok(Err(error)) => {
                thread.join().map_err(|_| Error::WorkerPanicked)??;
                return Err(error);
            }
            Err(_) => {
                thread.join().map_err(|_| Error::WorkerPanicked)??;
                return Err(Error::WorkerStopped);
            }
        };
        Ok(Self {
            sender: DispatchSender(shared),
            thread: Some(thread),
            failure: None,
        })
    }
    /// Клон отправителя остаётся безопасен после stop/Drop, возвращая WorkerStopped.
    pub fn sender(&self) -> DispatchSender {
        self.sender.clone()
    }
    /// Остановить и дождаться worker. Отменяет ещё не выполненные команды.
    /// Повторный вызов сохраняет ошибку предыдущего завершения.
    pub fn stop(&mut self) -> Result<()> {
        if let Some(thread) = &self.thread {
            if thread.thread().id() == thread::current().id() {
                self.sender.request_stop()?;
                return Err(Error::WouldDeadlock);
            }
            self.sender.request_stop()?;
        }
        if let Some(thread) = self.thread.take() {
            let result = match thread.join() {
                Ok(result) => result,
                Err(_) => Err(Error::WorkerPanicked),
            };
            if let Err(error) = result {
                self.failure = Some(Arc::new(error));
            }
        }
        match &self.failure {
            Some(error) => Err(Error::WorkerFailed(error.clone())),
            None => Ok(()),
        }
    }
    /// Остановка уже запрошена или worker завершился.
    pub fn is_stopped(&self) -> bool {
        self.sender.0.stopped()
    }
}
impl Drop for DispatchWorker {
    fn drop(&mut self) {
        let _stopped = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Client, DispatchClientOptions};
    use std::time::Duration;

    #[test]
    fn dispatch_owned_buffers_preserve_allocation_on_accept_full_and_close() {
        let lobby = format!("xshm_owned_dispatch_{}", std::process::id());
        let mut server = DispatchServer::bind(&lobby, DispatchOptions::default()).unwrap();
        let connecting = thread::spawn(move || {
            Client::connect_dispatch(&lobby, 1, "owned", DispatchClientOptions::default(), None)
        });
        let mut buffer = vec![0; 65535];
        let peer = loop {
            if let DispatchEvent::Connected { peer, .. } = server
                .next_event(&mut buffer, Wait::For(Duration::from_secs(5)))
                .unwrap()
            {
                break peer;
            }
        };
        let (_client, _) = connecting.join().unwrap().unwrap();
        let sender = DispatchSender(Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            wake: server.wake_handle(),
            limits: QueueLimits {
                commands: 1,
                bytes: 64,
            },
            worker: thread::current().id(),
        }));
        for overwrite in [false, true] {
            let mut data = Vec::with_capacity(64);
            data.extend_from_slice(b"move");
            let pointer = data.as_ptr();
            let completion = if overwrite {
                sender.submit_overwriting_owned(&peer, data)
            } else {
                sender.submit_try_send_owned(&peer, data)
            }
            .unwrap();
            let mut oversized = Vec::with_capacity(65);
            oversized.push(9);
            let rejected_pointer = oversized.as_ptr();
            let error = sender.submit_try_send_owned(&peer, oversized).unwrap_err();
            assert!(matches!(error.error, Error::Full));
            assert_eq!(error.data.as_ptr(), rejected_pointer);
            let command = {
                let mut queue = sender.0.queue.lock().unwrap();
                assert_eq!(queue.bytes, 64);
                let command = queue.commands.pop_front().unwrap();
                queue.bytes -= command.bytes();
                command
            };
            match command.operation {
                Operation::Send {
                    data,
                    overwrite: actual,
                } => {
                    assert_eq!(data.as_ptr(), pointer);
                    assert_eq!(data, b"move");
                    assert_eq!(actual, overwrite);
                }
                Operation::Disconnect => panic!("expected send"),
            }
            command.reply.send(Ok(())).unwrap();
            assert_eq!(completion.try_result().unwrap(), Some(()));
            // Даже пустая очередь отказывает из-за capacity, а не len.
            let error = sender
                .submit_overwriting_owned(&peer, error.data)
                .unwrap_err();
            assert!(matches!(error.error, Error::Full));
            assert_eq!(error.data.as_ptr(), rejected_pointer);
        }
        let pending = sender.submit_try_send_owned(&peer, vec![1]).unwrap();
        drop(CloseQueue(sender.0.clone()));
        assert!(matches!(pending.try_result(), Err(Error::WorkerStopped)));
        let data = vec![2];
        let pointer = data.as_ptr();
        let error = sender.submit_overwriting_owned(&peer, data).unwrap_err();
        assert!(matches!(error.error, Error::WorkerStopped));
        assert_eq!(error.data.as_ptr(), pointer);
        // Отказ wake после закрытия C-сервера тоже возвращает исходный Vec,
        // не оставляя скрытой команды или занятого бюджета.
        let sender = DispatchSender(Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            wake: server.wake_handle(),
            limits: QueueLimits::default(),
            worker: thread::current().id(),
        }));
        drop(server);
        let error = sender.submit_try_send_owned(&peer, error.data).unwrap_err();
        assert!(matches!(
            error.error,
            Error::Disconnected(super::super::DisconnectReason::Local)
        ));
        assert_eq!(error.data.as_ptr(), pointer);
        let queue = sender.0.queue.lock().unwrap();
        assert!(queue.commands.is_empty());
        assert_eq!(queue.bytes, 0);
    }
}
