/// Общее «магическое» значение для сегмента.
pub const SHARED_MAGIC: u32 = 0x5853_484d; // 'XSHM'
/// Версия формата разделяемой памяти.
///
/// `0x0002_0000` (1.0): самоописываемая раскладка -- `LayoutBlock` с
/// параметрами каждого направления, кадр `u32 len + u32 flags`, кадры
/// выровнены на 8 байт. Пиры формата `0x0001_0000` (xshm <= 0.9) получают
/// `VersionMismatch` и сами отвергают сегмент по версии -- молча разойтись в
/// смещениях стороны не могут.
pub const SHARED_VERSION: u32 = 0x0002_0000;

/// Ёмкость кольца по умолчанию (байты, на каждое направление).
pub const DEFAULT_RING_CAPACITY: usize = 2 * 1024 * 1024;
/// Лимит непрочитанных сообщений в кольце по умолчанию.
pub const DEFAULT_MAX_MESSAGES: u32 = 500;
/// Максимальный размер payload по умолчанию.
pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 65_535;

/// Минимальная ёмкость кольца: одна страница.
pub const MIN_RING_CAPACITY: usize = 4 * 1024;
/// Максимальная ёмкость кольца: 1 ГиБ. Позиции кольца -- `u32` с
/// переполнением по модулю 2^32, ёмкость обязана делить 2^32 (степень двойки)
/// и быть не больше 2^31, чтобы `write - read` однозначно давало занятость.
pub const MAX_RING_CAPACITY: usize = 1024 * 1024 * 1024;

/// Минимальный размер сообщения (пустые сообщения не передаются).
pub const MIN_MESSAGE_SIZE: usize = 1;

/// Размер служебного заголовка кадра: `u32 len LE` + `u32 flags` (всегда 0).
pub const FRAME_HEADER_SIZE: usize = 8;
/// Выравнивание кадров в кольце: позиции кадров кратны 8, поэтому заголовок
/// кадра никогда не разрывается границей кольца, а payload выровнен на 8.
pub const FRAME_ALIGN: usize = 8;

/// Размер `ControlBlock` + `LayoutBlock` -- первые байты сегмента, которые
/// читаются до разбора раскладки.
pub const SEGMENT_HEADER_SIZE: usize = 128;

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

/// Специальное значение: нет свободных слотов.
pub const SLOT_ID_NO_SLOT: u32 = 0xFFFF_FFFF;

/// Значение «слот свободен» для `ControlBlock::slot_claim`.
pub const CLAIM_FREE: u32 = 0;
