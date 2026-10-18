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
    /// Максимум байт payload в ожидающих командах.
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
}
impl Command {
    const fn bytes(&self) -> usize {
        match &self.operation {
            Operation::Send { data, .. } => data.len(),
            Operation::Disconnect => 0,
        }
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
    fn submit(
        &self,
        peer: &DispatchPeer,
        data: Option<(&[u8], bool)>,
    ) -> Result<CommandCompletion> {
        let bytes = data.map_or(0, |(data, _)| data.len());
        u32::try_from(bytes).map_err(|_| Error::LengthTooLarge)?;
        let mut queue = self
            .0
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if queue.stop || queue.closed {
            return Err(Error::WorkerStopped);
        }
        let total = queue.bytes.checked_add(bytes).ok_or(Error::Full)?;
        if queue.commands.len() >= self.0.limits.commands || total > self.0.limits.bytes {
            return Err(Error::Full);
        }
        let (reply, receiver) = mpsc::channel();
        let operation = match data {
            Some((data, overwrite)) => Operation::Send {
                data: data.to_vec(),
                overwrite,
            },
            None => Operation::Disconnect,
        };
        queue.commands.push_back(Command {
            peer: peer.clone(),
            operation,
            reply,
        });
        // Worker не заберёт команду под этим lock. Неудачный wake откатывает
        // только что добавленную команду: Err не скрывает будущую отправку.
        if let Err(error) = self.0.wake.wake() {
            let _removed = queue.commands.pop_back();
            return Err(error);
        }
        queue.bytes = total;
        Ok(CommandCompletion {
            receiver,
            worker: self.0.worker,
        })
    }
    /// Поставить отправку без вытеснения. Full здесь означает очередь команд;
    /// Full в подтверждении означает заполненное кольцо. Автоповтора нет.
    pub fn submit_try_send(&self, peer: &DispatchPeer, data: &[u8]) -> Result<CommandCompletion> {
        self.submit(peer, Some((data, false)))
    }
    /// Явно разрешить C вытеснить старые сообщения транспортного кольца.
    pub fn submit_overwriting(
        &self,
        peer: &DispatchPeer,
        data: &[u8],
    ) -> Result<CommandCompletion> {
        self.submit(peer, Some((data, true)))
    }
    /// Отключить конкретную сессию; повторно занятый id не адресуется.
    pub fn submit_disconnect(&self, peer: &DispatchPeer) -> Result<CommandCompletion> {
        self.submit(peer, None)
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
/// use xshm::c_core::{DispatchEvent, DispatchOptions, DispatchWorker, QueueLimits};
/// let mut worker = DispatchWorker::start("my_lobby", DispatchOptions::default(),
///     QueueLimits::default(), |_, event| {
///         if let DispatchEvent::Connected { peer, registration } = event {
///             println!("{}: {}", peer.client_id(), registration.name);
///         }
///     })?;
/// let sender = worker.sender();
/// sender.request_stop()?;
/// worker.stop()?;
/// # Ok::<(), xshm::c_core::Error>(())
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
