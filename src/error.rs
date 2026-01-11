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
}
