//! Эксклюзивное владение C-соединениями и заимствованная серверная сессия.
use super::error::check;
use super::{CancellationToken, ChannelConfig, Error, PrivateNamespace, Result, Wait};
use std::cell::Cell;
use std::ffi::CString;
use std::marker::PhantomData;
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle};
use std::ptr::NonNull;

#[derive(Debug)]
struct Owned {
    ptr: NonNull<xipc_sys::Conn>,
    _not_sync: PhantomData<Cell<()>>,
    namespace: Option<PrivateNamespace>,
}
// SAFETY: перемещаем только эксклюзивного владельца. C conn не привязан к
// потоку; mutex лобби принадлежит только синхронному connect, не сессии.
unsafe impl Send for Owned {}
impl Owned {
    fn activity(
        &mut self,
        pending: Option<usize>,
        wait: Wait,
        signal: Option<BorrowedHandle<'_>>,
    ) -> Result<super::Readiness> {
        let pending = match pending {
            Some(0) => return Err(Error::MessageTooSmall),
            Some(len) => u32::try_from(len).map_err(|_| Error::LengthTooLarge)?,
            None => 0,
        };
        let timeout = wait.milliseconds()?;
        let mut ready = 0;
        let mut reason = 0;
        // SAFETY: уникальный conn, живой заимствованный HANDLE и доступные
        // выходные числа. Проверки протокола/arm/recheck выполняет только C.
        let code = unsafe {
            xipc_sys::xipc_wait_activity(
                self.ptr.as_ptr(),
                pending,
                timeout,
                signal.map_or(std::ptr::null_mut(), |h| h.as_raw_handle()),
                &raw mut ready,
                &raw mut reason,
            )
        };
        if code == 14 {
            return Err(Error::Disconnected(reason.into()));
        }
        self.status(code)?;
        if ready == 0 || ready & !3 != 0 || (pending == 0 && ready & 2 != 0) {
            return Err(Error::InvalidCoreResult);
        }
        Ok(super::Readiness {
            readable: ready & 1 != 0,
            writable: ready & 2 != 0,
        })
    }
    fn new(namespace: Option<PrivateNamespace>) -> Result<Self> {
        // SAFETY: C-аллокатор не имеет предусловий.
        let ptr = NonNull::new(unsafe { xipc_sys::xipc_conn_alloc() }).ok_or(Error::Allocation)?;
        Ok(Self {
            ptr,
            _not_sync: PhantomData,
            namespace,
        })
    }
    fn status(&mut self, code: i32) -> Result<()> {
        if code == 14 || code == 15 {
            if code == 15 {
                return Err(Error::Disconnected(super::DisconnectReason::PeerDied));
            }
            // SAFETY: живое эксклюзивно принадлежащее нам C-соединение.
            let reason = unsafe { xipc_sys::xipc_conn_end_reason(self.ptr.as_ptr()) };
            return Err(Error::Disconnected(reason.into()));
        }
        check(code)
    }
    fn try_send(&mut self, message: &[u8]) -> Result<()> {
        let len = u32::try_from(message.len()).map_err(|_| Error::LengthTooLarge)?;
        // SAFETY: &mut self исключает параллельные вызовы; message живёт до возврата.
        let code =
            unsafe { xipc_sys::xipc_try_send(self.ptr.as_ptr(), message.as_ptr().cast(), len) };
        self.status(code)
    }
    fn overwrite(&mut self, message: &[u8]) -> Result<u32> {
        let len = u32::try_from(message.len()).map_err(|_| Error::LengthTooLarge)?;
        let mut overwritten = 0;
        // SAFETY: эксклюзивный conn, читаемый message и записываемый счётчик.
        let code = unsafe {
            xipc_sys::xipc_send(
                self.ptr.as_ptr(),
                message.as_ptr().cast(),
                len,
                &raw mut overwritten,
            )
        };
        self.status(code)?;
        Ok(overwritten)
    }
    fn receive(&mut self, buffer: &mut [u8]) -> Result<usize> {
        let capacity = u32::try_from(buffer.len()).map_err(|_| Error::LengthTooLarge)?;
        let mut len = 0;
        // SAFETY: уникальный conn; buffer доступен для записи capacity байт,
        // C не сохраняет его адрес; len доступен для записи до возврата.
        let code = unsafe {
            xipc_sys::xipc_recv(
                self.ptr.as_ptr(),
                buffer.as_mut_ptr().cast(),
                capacity,
                &raw mut len,
            )
        };
        if code == 20 {
            return Err(Error::BufferTooSmall {
                required: len as usize,
            });
        }
        self.status(code)?;
        let len = len as usize;
        if len > buffer.len() {
            return Err(Error::InvalidCoreResult);
        }
        Ok(len)
    }
    fn wait(&mut self, wait: Wait, cancel: Option<&CancellationToken>) -> Result<()> {
        let ms = wait.milliseconds()?;
        let mut reason = 0;
        // SAFETY: conn эксклюзивен; cancel заимствован и держит HANDLE живым
        // всё ожидание; C не сохраняет указатель на локальный reason.
        let code = unsafe {
            xipc_sys::xipc_wait_readable(
                self.ptr.as_ptr(),
                ms,
                CancellationToken::handle(cancel),
                &raw mut reason,
            )
        };
        if code == 14 {
            return Err(Error::Disconnected(reason.into()));
        }
        self.status(code)
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: указатель получен C-аллокатором, единственный владелец;
        // &mut исключает незавершённые вызовы. free завершает нужную роль.
        unsafe { xipc_sys::xipc_conn_free(self.ptr.as_ptr()) };
    }
}

/// Клиентская сторона: перемещаема между потоками, но не разделяется без lock.
///
/// ```compile_fail
/// fn requires_sync<T: Sync>() {}
/// requires_sync::<xshm::c_core::Client>();
/// ```
#[derive(Debug)]
pub struct Client(Owned);
impl Client {
    pub(super) fn wait_activity_with_wake(
        &mut self,
        pending: Option<usize>,
        wait: Wait,
        wake: &super::Beacon,
    ) -> Result<super::Readiness> {
        self.0.activity(pending, wait, Some(wake.as_handle()))
    }
    /// Дождаться доступного канала через C discovery/claim-free события.
    /// При исчерпании бюджета возвращается последняя ошибка попытки C.
    pub fn connect_when_available(
        name: &str,
        options: super::ClientConnectOptions,
        budget: super::ConnectBudget,
        cancel: Option<&CancellationToken>,
    ) -> Result<Self> {
        let name = CString::new(name)?;
        let timeout = budget.milliseconds()?;
        let security = options.security.raw()?;
        let fallback = match options.discovery_fallback {
            None => u32::MAX,
            Some(duration) if duration.is_zero() => return Err(Error::InvalidConfig),
            Some(duration) => Wait::For(duration).milliseconds()?,
        };
        let raw = xipc_sys::ClientWaitOptions {
            handshake_timeout_ms: Wait::For(options.handshake_timeout).milliseconds()?,
            retry_delay_ms: Wait::For(options.retry_delay).milliseconds()?,
            discovery_fallback_ms: fallback,
            security: &raw const security,
            ns: options
                .namespace
                .as_ref()
                .map_or(std::ptr::null(), PrivateNamespace::raw),
        };
        let mut conn = Owned::new(options.namespace)?;
        // SAFETY: нулевой уникальный conn, параметры/строка/токен живы весь
        // вызов, namespace удерживается conn; discovery выполняет только C.
        let code = unsafe {
            xipc_sys::xipc_client_connect_wait(
                conn.ptr.as_ptr(),
                name.as_ptr(),
                &raw const raw,
                timeout,
                CancellationToken::handle(cancel),
            )
        };
        conn.status(code)?;
        Ok(Self(conn))
    }

    /// Зарегистрироваться в Dispatch и получить выделенное соединение.
    /// Имя клиента C сокращает до 64 байт на границе символа UTF-8.
    pub fn connect_dispatch(
        lobby: &str,
        revision: u16,
        client_name: &str,
        options: super::DispatchClientOptions,
        cancel: Option<&CancellationToken>,
    ) -> Result<(Self, super::DispatchConnectionInfo)> {
        let lobby = CString::new(lobby)?;
        let client_name = CString::new(client_name)?;
        let raw = xipc_sys::DispatchClientOptions {
            lobby_timeout_ms: options.lobby_timeout.milliseconds()?,
            response_timeout_ms: options.response_timeout.milliseconds()?,
            channel_timeout_ms: options.channel_timeout.milliseconds()?,
            security: options.security.raw()?,
            ns: options
                .namespace
                .as_ref()
                .map_or(std::ptr::null(), PrivateNamespace::raw),
        };
        let mut conn = Owned::new(options.namespace)?;
        let mut id = 0;
        let mut channel_name = [0_u8; 65];
        // SAFETY: нулевой уникальный conn, живые C-строки/параметры/токен;
        // выходной массив имеет требуемые C 65 байт. C не сохраняет указатели.
        let code = unsafe {
            xipc_sys::xipc_dispatch_connect(
                lobby.as_ptr(),
                revision,
                client_name.as_ptr(),
                &raw const raw,
                CancellationToken::handle(cancel),
                conn.ptr.as_ptr(),
                &raw mut id,
                channel_name.as_mut_ptr().cast(),
            )
        };
        conn.status(code)?;
        let end = channel_name
            .iter()
            .position(|&b| b == 0)
            .ok_or(Error::InvalidCoreResult)?;
        if id == 0 || end == 0 {
            return Err(Error::InvalidCoreResult);
        }
        let channel_name = std::str::from_utf8(&channel_name[..end])
            .map_err(|_| Error::InvalidCoreResult)?
            .to_owned();
        Ok((
            Self(conn),
            super::DispatchConnectionInfo {
                client_id: id,
                channel_name,
            },
        ))
    }

    /// Занять свободный слот Multi. Полученный клиент владеет заявкой до Drop.
    /// Возвращённый номер слота служит диагностике, а не идентификатором сессии.
    pub fn connect_multi(
        base: &str,
        options: super::MultiClientOptions,
        budget: super::ConnectBudget,
        cancel: Option<&CancellationToken>,
    ) -> Result<(Self, u32)> {
        let base = CString::new(base)?;
        let ms = budget.milliseconds()?;
        let security = options.security.raw()?;
        let discovery_fallback_ms = match options.discovery_fallback {
            None => u32::MAX,
            Some(duration) if duration.is_zero() => return Err(Error::InvalidConfig),
            Some(duration) => Wait::For(duration).milliseconds()?,
        };
        let raw = xipc_sys::MultiClientOptions {
            slot_timeout_ms: Wait::For(options.handshake_timeout).milliseconds()?,
            retry_delay_ms: Wait::For(options.retry_delay).milliseconds()?,
            ns: options
                .namespace
                .as_ref()
                .map_or(std::ptr::null(), PrivateNamespace::raw),
            discovery_fallback_ms,
            security: &raw const security,
        };
        let mut conn = Owned::new(options.namespace)?;
        let mut slot = u32::MAX;
        // SAFETY: уникальный нулевой conn, строки/параметры/токен живы весь
        // вызов. Namespace удерживается conn, claim освобождается C free.
        let code = unsafe {
            xipc_sys::xipc_multi_client_connect(
                conn.ptr.as_ptr(),
                base.as_ptr(),
                &raw const raw,
                ms,
                CancellationToken::handle(cancel),
                &raw mut slot,
            )
        };
        conn.status(code)?;
        if slot >= 256 {
            return Err(Error::InvalidCoreResult);
        }
        Ok((Self(conn), slot))
    }

    /// Одна попытка соединения. Имя проверяет C-ядро; NUL отвергается Rust.
    pub fn connect(name: &str, wait: Wait, cancel: Option<&CancellationToken>) -> Result<Self> {
        Self::connect_with_namespace(name, None, wait, cancel)
    }
    /// Соединиться в пространстве имён и удерживать его до закрытия канала.
    pub fn connect_in(
        name: &str,
        namespace: &PrivateNamespace,
        wait: Wait,
        cancel: Option<&CancellationToken>,
    ) -> Result<Self> {
        Self::connect_with_namespace(name, Some(namespace.clone()), wait, cancel)
    }
    fn connect_with_namespace(
        name: &str,
        namespace: Option<PrivateNamespace>,
        wait: Wait,
        cancel: Option<&CancellationToken>,
    ) -> Result<Self> {
        let name = CString::new(name)?;
        let ms = wait.milliseconds()?;
        let mut conn = Owned::new(namespace)?;
        let ns = conn
            .namespace
            .as_ref()
            .map_or(std::ptr::null(), PrivateNamespace::raw);
        // SAFETY: C-строка, уникальный нулевой conn; namespace удерживается conn;
        // заимствованный cancel держит событие живым всё рукопожатие.
        let code = unsafe {
            xipc_sys::xipc_client_connect(
                conn.ptr.as_ptr(),
                name.as_ptr(),
                ns,
                ms,
                CancellationToken::handle(cancel),
            )
        };
        conn.status(code)?;
        Ok(Self(conn))
    }
}

/// Слушатель одного канала. `accept` выдаёт эксклюзивную заимствованную сессию.
///
/// ```compile_fail
/// use xshm::c_core::{ChannelConfig, Server, Wait};
/// let mut server = Server::bind("borrow_example", ChannelConfig::default()).unwrap();
/// let mut session = server.accept(Wait::Infinite, None).unwrap();
/// drop(server); // слушатель ещё заимствован сессией
/// session.try_send(b"still borrowing").unwrap();
/// ```
#[derive(Debug)]
pub struct Server(Owned);
impl Server {
    /// Создать именованный канал. Политика по умолчанию ограничена пользователем.
    pub fn bind(name: &str, config: ChannelConfig) -> Result<Self> {
        let name = CString::new(name)?;
        let raw_config = config.raw()?;
        let mut conn = Owned::new(config.namespace)?;
        // SAFETY: C-строка и уникальный conn живут весь вызов; namespace
        // удерживается conn до закрытия всех заимствующих его C-объектов.
        let code = unsafe {
            xipc_sys::xipc_server_start(conn.ptr.as_ptr(), name.as_ptr(), &raw const raw_config)
        };
        conn.status(code)?;
        Ok(Self(conn))
    }
    /// Принять клиента. Пока Session жива, повторный accept и Drop сервера недоступны.
    /// Уже зафиксированное рукопожатие может выиграть гонку с отменой.
    pub fn accept(
        &mut self,
        wait: Wait,
        cancel: Option<&CancellationToken>,
    ) -> Result<Session<'_>> {
        let ms = wait.milliseconds()?;
        // SAFETY: эксклюзивный живой сервер; заимствованный cancel держит HANDLE.
        let code = unsafe {
            xipc_sys::xipc_server_accept(self.0.ptr.as_ptr(), ms, CancellationToken::handle(cancel))
        };
        self.0.status(code)?;
        Ok(Session(&mut self.0))
    }
}

/// Принятая сессия. Drop отключает клиента, сохраняя слушатель для нового accept.
#[derive(Debug)]
pub struct Session<'server>(&'server mut Owned);
impl Drop for Session<'_> {
    fn drop(&mut self) {
        // SAFETY: заимствование удерживает сервер и эксклюзивно владеет сессией.
        // Уже завершившаяся сессия даёт NOT_CONNECTED; повторный close безопасен.
        unsafe { xipc_sys::xipc_server_disconnect(self.0.ptr.as_ptr()) };
    }
}

macro_rules! channel_methods {
    () => {
        /// Одним ожиданием дождаться входящих данных либо места для pending.
        /// None отключает ожидание записи; Some(0) отвергается. Оба флага
        /// могут быть готовы вместе; проверка и уведомления реализованы в C.
        pub fn wait_activity(
            &mut self,
            pending: Option<usize>,
            wait: Wait,
            cancel: Option<&CancellationToken>,
        ) -> Result<super::Readiness> {
            self.0
                .activity(pending, wait, cancel.map(AsHandle::as_handle))
        }
        /// Известное C-ядру состояние текущей сессии.
        pub fn is_connected(&mut self) -> bool {
            // SAFETY: эксклюзивное обращение к живому C-объекту.
            unsafe { xipc_sys::xipc_is_connected(self.0.ptr.as_ptr()) }
        }
        /// Собственная копия раскладки; None, когда C уже освободил канал.
        pub fn layout(&mut self) -> Option<super::Layout> {
            // SAFETY: C возвращает свой layout либо NULL; &mut не допускает
            // изменения conn до завершения копирования простой repr(C)-структуры.
            let ptr = unsafe { xipc_sys::xipc_conn_layout(self.0.ptr.as_ptr()) };
            // SAFETY: непустой указатель C валиден весь этот эксклюзивный вызов.
            unsafe { ptr.as_ref() }.copied()
        }
        /// PID пира из C-ядра (сам по себе не является аутентификацией).
        pub fn peer_pid(&mut self) -> Option<u32> {
            // SAFETY: эксклюзивный живой C-объект; возвращается число по значению.
            let pid = unsafe { xipc_sys::xipc_peer_pid(self.0.ptr.as_ptr()) };
            (pid != 0).then_some(pid)
        }
        /// Дождаться места для конкретного сообщения без таймерного опроса.
        /// При уходе пира его последние входящие кадры ещё можно прочитать.
        pub fn wait_for_space(
            &mut self,
            payload_len: usize,
            wait: Wait,
            cancel: Option<&CancellationToken>,
        ) -> Result<()> {
            let len = u32::try_from(payload_len).map_err(|_| Error::LengthTooLarge)?;
            let timeout = wait.milliseconds()?;
            // SAFETY: уникальный conn; заимствованный token удерживает HANDLE
            // всё ожидание. Arm/recheck/Dekker выполняет единое C-ядро.
            let code = unsafe {
                xipc_sys::xipc_wait_for_space(
                    self.0.ptr.as_ptr(),
                    len,
                    timeout,
                    CancellationToken::handle(cancel),
                )
            };
            self.0.status(code)
        }
        /// Отправить без потери старых сообщений; Full означает отсутствие места.
        pub fn try_send(&mut self, message: &[u8]) -> Result<()> {
            self.0.try_send(message)
        }
        /// Явно разрешить вытеснение старых сообщений; вернуть их количество.
        pub fn send_overwriting(&mut self, message: &[u8]) -> Result<u32> {
            self.0.overwrite(message)
        }
        /// Прочитать кадр в буфер. BufferTooSmall сохраняет кадр для повтора.
        pub fn receive_into(&mut self, buffer: &mut [u8]) -> Result<usize> {
            self.0.receive(buffer)
        }
        /// Дождаться данных; после ухода пира сначала доступны его последние кадры.
        pub fn wait_readable(
            &mut self,
            wait: Wait,
            cancel: Option<&CancellationToken>,
        ) -> Result<()> {
            self.0.wait(wait, cancel)
        }
    };
}
impl Client {
    channel_methods!();
}
impl Session<'_> {
    channel_methods!();
}
