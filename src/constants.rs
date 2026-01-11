/// Общее «магическое» значение для сегмента.
pub const SHARED_MAGIC: u32 = 0x5853_484d; // 'XSHM'
/// Текущая версия протокола.
pub const SHARED_VERSION: u32 = 0x0001_0000;

/// Размер каждого кольцевого буфера (байты).
pub const RING_CAPACITY: usize = 2 * 1024 * 1024;
/// Маска размера (так как это степень двойки).
pub const RING_MASK: u32 = (RING_CAPACITY as u32) - 1;

/// Максимальное количество сообщений в очереди.
pub const MAX_MESSAGES: u32 = 250;
/// Максимальный размер одного сообщения.
pub const MAX_MESSAGE_SIZE: usize = 65_535;
/// Минимальный размер сообщения.
pub const MIN_MESSAGE_SIZE: usize = 2;

/// Размер служебного заголовка сообщения (байты).
pub const MESSAGE_HEADER_SIZE: usize = 4; // u16 length + u16 flags/reserved

/// Имя события для данных, поступающих от сервера к клиенту.
pub const EVENT_DATA_SUFFIX: &str = "DATA";
/// Имя события для уведомления о свободном месте.
pub const EVENT_SPACE_SUFFIX: &str = "SPACE";
/// Имя события о подключении (ответ сервера).
pub const EVENT_CONNECT_SUFFIX: &str = "CONNECT";
/// Имя события запроса подключения (от клиента).
pub const EVENT_CONNECT_REQ_SUFFIX: &str = "CONNECT_REQ";
/// Имя события об отключении.
pub const EVENT_DISCONNECT_SUFFIX: &str = "DISCONNECT";

/// Состояния handshake.
pub const HANDSHAKE_IDLE: u32 = 0;
pub const HANDSHAKE_CLIENT_HELLO: u32 = 1;
pub const HANDSHAKE_SERVER_READY: u32 = 2;

/// Индекс в reserved[] для передачи slot_id при multi-client handshake.
pub const RESERVED_SLOT_ID_INDEX: usize = 0;
/// Специальное значение: нет свободных слотов.
pub const SLOT_ID_NO_SLOT: u32 = 0xFFFF_FFFF;
