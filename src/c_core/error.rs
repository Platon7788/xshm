//! Ошибки Rust-границы и C-ядра, без преобразования чужого enum через transmute.

/// Результат безопасного интерфейса.
pub type Result<T> = std::result::Result<T, Error>;

/// Почему завершилась сессия.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DisconnectReason {
    /// Сессия отсутствует или причина пока неизвестна.
    Unknown,
    /// Пир штатно завершил сессию.
    Graceful,
    /// Процесс пира завершился.
    PeerDied,
    /// Сессию завершила локальная сторона.
    Local,
    /// Повреждение протокола или ошибка ожидания.
    Error,
}
impl From<i32> for DisconnectReason {
    fn from(value: i32) -> Self {
        match value {
            1 => Self::Graceful,
            2 => Self::PeerDied,
            3 => Self::Local,
            4 => Self::Error,
            _ => Self::Unknown,
        }
    }
}

/// Ошибка операции. Неизвестный код C сохраняется для диагностики.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Admission-обработчик запаниковал при panic=unwind; сервер отказывает
    /// в регистрации и больше не вызывает этот обработчик. При panic=abort
    /// действует обычная политика завершения процесса, перехват невозможен.
    #[error("паника обработчика допуска клиентов")]
    AdmissionPanicked,
    /// Неверный аргумент C API.
    #[error("неверный аргумент")]
    InvalidArgument,
    /// Недопустимая конфигурация канала.
    #[error("недопустимая конфигурация канала")]
    InvalidConfig,
    /// Имя превышает предел NT-объекта.
    #[error("имя слишком длинное")]
    NameTooLong,
    /// Канал ещё не существует.
    #[error("канал не найден")]
    NotFound,
    /// Имя уже занято другим сервером.
    #[error("имя уже занято")]
    NameCollision,
    /// Доступ запрещён политикой безопасности.
    #[error("доступ запрещён")]
    AccessDenied,
    /// Создатель ещё не опубликовал канал.
    #[error("канал ещё не опубликован")]
    NotReady,
    /// Несовместимая версия wire-протокола.
    #[error("несовместимая версия канала")]
    VersionMismatch,
    /// Нарушен протокол или повреждена память канала.
    #[error("повреждён протокол канала")]
    Corrupted,
    /// Заявка или сессия занята другим клиентом.
    #[error("канал занят")]
    Busy,
    /// Рукопожатие не завершилось успешно.
    #[error("ошибка рукопожатия")]
    HandshakeFailed,
    /// Сообщение меньше минимально допустимого.
    #[error("сообщение слишком мало")]
    MessageTooSmall,
    /// Сообщение превышает лимит направления.
    #[error("сообщение слишком велико")]
    MessageTooLarge,
    /// Нет доступного слота.
    #[error("нет свободного слота")]
    NoFreeSlot,
    /// Сервер отказал в регистрации.
    #[error("сервер отказал в регистрации")]
    Rejected,
    /// Именованный объект имеет другой тип.
    #[error("несовпадение типа объекта")]
    ObjectTypeMismatch,
    /// Не классифицированная ядром ошибка NT-вызова.
    #[error("ошибка NT-вызова в C-ядре")]
    KernelCall,
    /// Истёк срок ожидания.
    #[error("истёк срок ожидания")]
    Timeout,
    /// Сработал токен отмены.
    #[error("операция отменена")]
    Cancelled,
    /// В очереди пока нет сообщений.
    #[error("очередь пуста")]
    Empty,
    /// Для отправки без перезаписи нет места.
    #[error("очередь заполнена")]
    Full,
    /// Сессия завершилась.
    #[error("сессия завершилась: {0:?}")]
    Disconnected(DisconnectReason),
    /// Кадр сохранён; можно повторить чтение в больший буфер.
    #[error("буфер мал: необходимо {required} байт")]
    BufferTooSmall {
        /// Необходимая ёмкость буфера.
        required: usize,
    },
    /// Имя содержит нулевой байт, недопустимый в C-строке.
    #[error("имя содержит нулевой байт")]
    NameContainsNul(#[from] std::ffi::NulError),
    /// Конечный таймаут не помещается в миллисекунды C ABI.
    #[error("конечный таймаут превышает диапазон C ABI")]
    TimeoutTooLarge,
    /// Размер буфера или сообщения не помещается в C ABI.
    #[error("размер превышает u32")]
    LengthTooLarge,
    /// C-аллокатор не смог создать соединение.
    #[error("не удалось выделить соединение")]
    Allocation,
    /// Ошибка локального объекта отмены.
    #[error("локальное событие отмены: {0}")]
    Os(#[from] std::io::Error),
    /// C вернул недопустимый размер результата.
    #[error("нарушен контракт размера результата C ABI")]
    InvalidCoreResult,
    /// Прочий стабильный код xipc_status.
    #[error("ошибка C-ядра xipc: код {0}")]
    Core(i32),
}

pub(super) const fn check(status: i32) -> Result<()> {
    match status {
        0 => Ok(()),
        1 => Err(Error::InvalidArgument),
        2 => Err(Error::InvalidConfig),
        3 => Err(Error::NameTooLong),
        4 => Err(Error::Timeout),
        5 => Err(Error::Cancelled),
        6 => Err(Error::NotFound),
        7 => Err(Error::NameCollision),
        8 => Err(Error::AccessDenied),
        9 => Err(Error::NotReady),
        10 => Err(Error::VersionMismatch),
        11 => Err(Error::Corrupted),
        12 => Err(Error::Busy),
        13 => Err(Error::HandshakeFailed),
        16 => Err(Error::Full),
        17 => Err(Error::Empty),
        18 => Err(Error::MessageTooSmall),
        19 => Err(Error::MessageTooLarge),
        21 => Err(Error::NoFreeSlot),
        22 => Err(Error::Rejected),
        23 => Err(Error::ObjectTypeMismatch),
        24 => Err(Error::KernelCall),
        25 => Err(Error::Allocation),
        code => Err(Error::Core(code)),
    }
}
