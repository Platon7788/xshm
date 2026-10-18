//! Эксклюзивное владение C-соединениями и заимствованная серверная сессия.
use super::error::check;
use super::{CancellationToken, ChannelConfig, Error, PrivateNamespace, Result, Wait};
use std::cell::Cell;
use std::ffi::CString;
use std::marker::PhantomData;
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
        let security = options.security.raw();
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
        let raw_config = config.raw();
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
