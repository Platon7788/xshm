//! Фоновое обслуживание уже подключённого C-клиента одним владельцем.
use super::client_queue::{ClientCommand, CloseClientQueue, SendMode};
use super::{Client, ClientSender, DisconnectReason, Error, Layout, QueueLimits, Result, Wait};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};

/// Событие одной сессии. Данные принадлежат буферу worker до возврата callback.
#[derive(Debug)]
#[non_exhaustive]
pub enum ClientEvent<'buffer> {
    /// Соединение передано worker; это не аутентификация PID.
    Connected {
        peer_pid: Option<u32>,
        layout: Layout,
    },
    /// Копия сообщения, полученная C recv.
    Message { data: &'buffer [u8] },
    /// Команда завершена, в локальной очереди освобождён её объём.
    QueueAvailable,
    /// После неудачного try_send C подтвердил место для указанной длины.
    Writable { payload_len: usize },
    /// Пир отключился; последние входящие кадры прочитаны. Локальный stop
    /// завершает worker без вызова новых обработчиков.
    Disconnected { reason: DisconnectReason },
    /// Неустранимая ошибка сессии; тот же исход доступен через stop.
    Failed { error: Arc<Error> },
}

/// Владелец одного фонового C-клиента без автоматической перепривязки очереди
/// к новой сессии. Сначала выполните Client::connect/connect_dispatch либо
/// connect_when_available, затем передайте единственного клиента в spawn.
/// Drop/stop из другого потока ждут callbacks; self-join из callback отвергается.
#[derive(Debug)]
pub struct ClientWorker {
    sender: ClientSender,
    thread: Option<JoinHandle<Result<()>>>,
    failure: Option<Arc<Error>>,
}
impl ClientWorker {
    /// Перенести подключённый Client в выделенный поток. Handler сразу
    /// получает отправителя: внешняя инициализация через OnceLock не нужна.
    pub fn spawn<F>(mut client: Client, limits: QueueLimits, mut handler: F) -> Result<Self>
    where
        F: for<'a> FnMut(&ClientSender, ClientEvent<'a>) + Send + 'static,
    {
        if limits.commands == 0 || limits.bytes == 0 {
            return Err(Error::InvalidConfig);
        }
        let layout = client
            .layout()
            .ok_or(Error::Disconnected(DisconnectReason::Unknown))?;
        let peer_pid = client.peer_pid();
        let (ready, receiver) = mpsc::channel();
        let thread = crate::thread_hook::spawn(
            thread::Builder::new().name("xshm-client".to_owned()),
            move || {
                let sender = match ClientSender::new(limits) {
                    Ok(sender) => sender,
                    Err(error) => {
                        let _delivered = ready.send(Err(error));
                        return Ok(());
                    }
                };
                let _close = CloseClientQueue(sender.clone());
                if ready.send(Ok(sender.clone())).is_err() {
                    return Ok(());
                }
                handler(&sender, ClientEvent::Connected { peer_pid, layout });
                let result = service(
                    &mut client,
                    &sender,
                    layout.s2c.max_message_size as usize,
                    &mut handler,
                );
                sender.close();
                drop(client);
                match result {
                    Ok(DisconnectReason::Local) => Ok(()),
                    Ok(reason) => {
                        handler(&sender, ClientEvent::Disconnected { reason });
                        Ok(())
                    }
                    Err(error) => {
                        let error = match error {
                            Error::WorkerFailed(error) => error,
                            error => Arc::new(error),
                        };
                        handler(
                            &sender,
                            ClientEvent::Failed {
                                error: error.clone(),
                            },
                        );
                        Err(Error::WorkerFailed(error))
                    }
                }
            },
        )?;
        let sender = match receiver.recv() {
            Ok(Ok(sender)) => sender,
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
            sender,
            thread: Some(thread),
            failure: None,
        })
    }
    /// Безопасный клон отправителя, связанного только с этой сессией.
    pub fn sender(&self) -> ClientSender {
        self.sender.clone()
    }
    /// Остановить, дождаться worker и вернуть сохранённый исход завершения.
    /// При вызове на самом worker запрашивает stop и возвращает WouldDeadlock.
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
                self.failure = Some(match error {
                    Error::WorkerFailed(error) => error,
                    error => Arc::new(error),
                });
            }
        }
        match &self.failure {
            Some(error) => Err(Error::WorkerFailed(error.clone())),
            None => Ok(()),
        }
    }
    /// Остановка запрошена либо сессия закрывает очередь отправки.
    pub fn is_stopped(&self) -> bool {
        self.sender.is_stopped()
    }
}
impl Drop for ClientWorker {
    fn drop(&mut self) {
        let _stopped = self.stop();
    }
}

fn service<F>(
    client: &mut Client,
    sender: &ClientSender,
    capacity: usize,
    handler: &mut F,
) -> Result<DisconnectReason>
where
    F: for<'a> FnMut(&ClientSender, ClientEvent<'a>),
{
    let mut buffer = vec![0; capacity];
    let mut pending: Option<ClientCommand> = None;
    let mut closing = false;
    let mut space_interest = None;
    loop {
        // Под lock только наблюдаем/сбрасываем локальное уведомление.
        // Ни C-соединение, ни callback под этим lock не вызываются.
        match sender.next(pending.is_none() && !closing) {
            Ok(command) => {
                if command.is_some() {
                    pending = command;
                }
            }
            Err(Error::WorkerStopped) => return Ok(DisconnectReason::Local),
            Err(error) => return Err(error),
        }
        if let Some(command) = pending.take() {
            let result = match command.mode {
                SendMode::Overwrite => client.send_overwriting(&command.data).map(|_| ()),
                SendMode::Try | SendMode::Lossless => client.try_send(&command.data),
            };
            match result {
                Err(Error::Full) if matches!(command.mode, SendMode::Lossless) => {
                    pending = Some(command);
                }
                Err(error @ (Error::Corrupted | Error::KernelCall)) => {
                    let error = Arc::new(error);
                    sender.finish(command, Err(Error::WorkerFailed(error.clone())));
                    return Err(Error::WorkerFailed(error));
                }
                result => {
                    if matches!(&result, Err(Error::Full)) {
                        space_interest = Some(command.data.len());
                    }
                    if matches!(&result, Err(Error::Disconnected(_))) {
                        closing = true;
                    }
                    sender.finish(command, result);
                    // При отказе TX сначала доставляем остаток RX. Публикация
                    // WorkerStopped раньше последнего callback позволяла
                    // потребителю остановить worker и потерять финальный BYE.
                    // Новые команды пока могут войти в bounded очередь; после
                    // drain они отменяются общим close, исполнения не обещано.
                    if !closing {
                        handler(sender, ClientEvent::QueueAvailable);
                    }
                }
            }
        }
        // Ограничение пачки не позволяет входящим кадрам голодать команды.
        for _ in 0..32 {
            if sender.stop_requested() {
                return Ok(DisconnectReason::Local);
            }
            match client.receive_into(&mut buffer) {
                Ok(len) => handler(
                    sender,
                    ClientEvent::Message {
                        data: &buffer[..len],
                    },
                ),
                Err(Error::Empty) => break,
                Err(Error::Disconnected(reason)) => return Ok(reason),
                Err(error) => return Err(error),
            }
        }
        if pending.is_none() && !closing && sender.has_commands() {
            continue;
        }
        let wanted = if closing {
            None
        } else {
            pending.as_ref().map(|c| c.data.len()).or(space_interest)
        };
        match client.wait_activity_with_wake(wanted, Wait::Infinite, sender.wake()) {
            Ok(ready) => {
                if ready.writable()
                    && pending.is_none()
                    && let Some(payload_len) = space_interest.take()
                {
                    handler(sender, ClientEvent::Writable { payload_len });
                }
            }
            Err(Error::Cancelled) => {} // новая команда или stop; проверяется под lock
            Err(Error::Disconnected(reason)) => return Ok(reason),
            Err(error) => return Err(error),
        }
    }
}
