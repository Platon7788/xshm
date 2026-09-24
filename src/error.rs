/// Удобный тип результата для библиотеки.
pub type Result<T> = std::result::Result<T, ShmError>;

/// Ошибки, которые может возвращать библиотека.
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum ShmError {
    /// Запрошенная операция недоступна, так как соединение отсутствует.
    #[error("endpoint is not connected")]
    NotConnected,
    /// Ресурс ожидает завершения другой операции (например, handshake).
    #[error("endpoint is not ready yet")]
    NotReady,
    /// Половина соединения уже активна; повторное подключение невозможно.
    #[error("endpoint is already connected")]
    AlreadyConnected,
    /// Ожидаемое событие не произошло в отведённое время.
    #[error("operation timed out")]
    Timeout,
    /// Очередь сообщений пуста.
    #[error("no messages available")]
    QueueEmpty,
    /// Очередь сообщений переполнена и не может принять данные без перезаписи.
    #[error("message queue is full")]
    QueueFull,
    /// Сообщение слишком маленькое (минимум 2 байта).
    #[error("message is too small")]
    MessageTooSmall,
    /// Сообщение превышает допустимый размер.
    #[error("message is too large")]
    MessageTooLarge,
    /// Формат данных в буфере повреждён или некорректен.
    #[error("shared ring buffer is corrupted")]
    Corrupted,
    /// Не удалось выполнить handshake между участниками.
    #[error("handshake failed")]
    HandshakeFailed,
    /// Системная ошибка Windows (NTSTATUS или Win32 код).
    #[error("windows error {code:#x} while {context}")]
    WindowsError {
        /// Код ошибки (NTSTATUS или Win32).
        code: u32,
        /// Контекст операции.
        context: &'static str,
    },
    /// Нет свободных слотов на мультиклиентном сервере.
    #[error("no free slots available on multi-client server")]
    NoFreeSlot,
    /// Некорректная конфигурация (например, недопустимое число клиентов).
    #[error("invalid configuration: {0}")]
    InvalidConfig(&'static str),
    /// Процесс пира завершился без штатного отключения (крах, kill).
    /// Возвращается блокирующими операциями (`wait_for_space`, `poll_*`),
    /// когда пир наблюдается через удерживаемый handle процесса (оба пира
    /// 0.8+ и handle удалось открыть).
    #[error("peer process died")]
    PeerDied,
}

/// Почему соединение разорвано (для `on_disconnect_reason` и аналогов).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DisconnectReason {
    /// Пир отключился штатно (Drop/`stop()` -> событие DISCONNECT).
    Graceful,
    /// Процесс пира завершился без штатного отключения (крах, kill,
    /// `TerminateProcess`) -- замечено по удерживаемому handle процесса.
    /// Всё, что пир успел записать в кольцо до смерти, к этому моменту
    /// уже доставлено.
    PeerDied,
    /// Соединение разорвала локальная сторона (`disconnect_client`, `stop`).
    Local,
    /// Ошибка протокола или ожидания (`Corrupted`, ошибка NT-вызова).
    Error,
}
