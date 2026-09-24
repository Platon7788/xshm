/// Общее «магическое» значение для сегмента.
pub const SHARED_MAGIC: u32 = 0x5853_484d; // 'XSHM'
/// Текущая версия протокола.
pub const SHARED_VERSION: u32 = 0x0001_0000;

/// Размер каждого кольцевого буфера (байты).
pub const RING_CAPACITY: usize = 2 * 1024 * 1024;
/// Маска размера (так как это степень двойки).
pub const RING_MASK: u32 = (RING_CAPACITY as u32) - 1;

/// Максимальное количество сообщений в очереди.
pub const MAX_MESSAGES: u32 = 500;
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

/// Специальное значение: нет свободных слотов.
pub const SLOT_ID_NO_SLOT: u32 = 0xFFFF_FFFF;

/// Индекс в reserved[] СЕГМЕНТА СЛОТА для атомарного захвата слота multi-клиентом.
/// Хранит токен захватившего клиента; `CLAIM_FREE` (0) = слот свободен.
/// Захват выполняется через `compare_exchange(CLAIM_FREE -> token)` — это даёт
/// конкурентное, lock-free распределение слотов без централизованного lobby.
pub const RESERVED_CLAIM_INDEX: usize = 0;
/// Значение «слот свободен» для claim.
pub const CLAIM_FREE: u32 = 0;

/// Индекс в reserved[] СЕГМЕНТА СЛОТА для PID процесса, захватившего claim.
/// Записывается клиентом сразу после успешного CAS в `try_claim_slot`.
/// Сервер использует его для liveness-проверки connected-слотов: клиент,
/// упавший ПОСЛЕ завершения handshake (но не освободивший claim), иначе
/// навсегда лишает сервер слота — событий от мёртвого процесса не будет.
pub const RESERVED_OWNER_PID_INDEX: usize = 1;

/// Индекс в reserved[] ControlBlock для PID процесса-сервера (0.8+).
///
/// Пишется сервером при создании сегмента (`SharedServer::start*`); клиент
/// читает его после SERVER_READY и открывает удерживаемый handle процесса
/// сервера (`ProcessWatch`) для детекции его смерти. `0` -- сервер старой
/// версии, PID неизвестен (детекции смерти нет, поведение как в 0.7).
pub const RESERVED_SERVER_PID_INDEX: usize = 2;

/// Индекс в reserved[] ControlBlock для PID процесса-клиента (0.8+).
///
/// Клиент пишет его ДО `CLIENT_HELLO` (Release-публикация через
/// `client_state`); сервер в `complete_handshake` забирает его `swap(0)` --
/// значение одноразовое, поэтому клиент старой версии, пришедший следом, не
/// унаследует чужой PID. Откат handshake на клиенте тоже его обнуляет.
pub const RESERVED_CLIENT_PID_INDEX: usize = 3;
