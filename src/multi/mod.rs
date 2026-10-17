//! Мультиклиентный сервер для xShm.
//!
//! Позволяет одному серверу обслуживать до N клиентов одновременно.
//! Клиент САМ захватывает свободный слот (lock-free claim) — без
//! централизованного lobby.
//!
//! # Архитектура (конкурентный захват слотов)
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                      MultiServer                            │
//! ├─────────────────────────────────────────────────────────────┤
//! │  Нет lobby. N независимых сегментов-слотов BaseName_0..N-1.  │
//! │  Клиент пробегает слоты и атомарно захватывает свободный:    │
//! │    CAS slot_claim: CLAIM_FREE -> token (на сегменте слота)   │
//! │  Победитель CAS делает обычный handshake SharedClient.       │
//! ├─────────────────────────────────────────────────────────────┤
//! │  Slot 0: SharedServer "BaseName_0" ←→ Client A  (claim=tA)  │
//! │  Slot 1: SharedServer "BaseName_1" ←→ Client B  (claim=tB)  │
//! │  Slot 2: (свободен, claim=CLAIM_FREE)                       │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! N клиентов подключаются ПОЛНОСТЬЮ КОНКУРЕНТНО: CAS на разной памяти,
//! без общего состояния, без coalescing событий, без коллизий слотов.
//!
//! # Событийная модель (0.9; 0.10 -- один поток-диспетчер)
//!
//! Ни сервер, ни клиент не просыпаются в простое (`poll_timeout = None` по
//! умолчанию). Сервер (0.10) обслуживает ВСЕ слоты одним потоком-диспетчером
//! на своём порту завершения (`reactor::CompletionPort`, не общий реактор):
//! каждый объект ожидания слота привязан к порту пакетом ожидания с ключом
//! (слот, вид) и меткой-поколением -- у свободного слота `C2S_CONNECT_REQ`,
//! у подключённого `S2C_DISCONNECT`, `C2S_DATA` и handle процесса клиента.
//! `disconnect_client`/`stop` будят диспетчер постом в порт
//! (`NtSetIoCompletion`, ключ `WAKE_KEY`). Таймаут ожидания порта -- ровно
//! до ближайшего ДЕДЛАЙНА (протухший захват слота, разовая проверка
//! брошенного рукопожатия), а не тик. Колбэки всех слотов выполняются
//! последовательно на этом потоке, данные слотов читаются по кругу (не
//! больше `recv_batch` сообщений слота за круг). Клиент спит по
//! `[S2C_DISCONNECT, S2C_DATA, процесс сервера, wake]`; `send`/`stop` будят
//! его своим событием.
//!
//! # Привязки одноразовые: порядок перепривязки
//!
//! Привязка пакета -- ожидание: сработав, она потребляет сигнал
//! автосбросного события и снимается. Диспетчер -- единственный ждущий
//! `C2S_CONNECT_REQ` и `C2S_DATA` на стороне сервера, поэтому сигнал,
//! пришедший между срабатыванием и перепривязкой, остаётся в событии и
//! будит новую привязку сразу (`AlreadySignaled`). `C2S_DATA` перепривязывается
//! ДО чтения кольца, а «пусто» решается только по позициям
//! (`read_message` -> `confirm_empty`): писатель, опубликовавший кадр после
//! этой проверки, сигналит `DATA` в уже привязанный пакет. Снятие привязки
//! (`WaitPacket::cancel`), потребившей сигнал (`FiredNotDelivered` или
//! извлечённой, но ещё не обработанной -- `AlreadyDelivered`), возвращает
//! сигнал (`NtSetEvent`) для `CONNECT_REQ`/`DISCONNECT` и перечитывает кольцо
//! для `DATA`; лишний сигнал безопасен, потерянный -- нет. Пакет, чья
//! метка-поколение устарела (снят или перепривязан), отбрасывается:
//! завершение прежней сессии не попадает в новую.
//!
//! Пакет у каждой привязки свой и переиспользуется, пока исправен (до
//! «грязного» исхода, см. ниже): привязок фиксированное число (по видам
//! ожидания слота), поэтому общий пул, как у реактора (`reactor`, «Пул
//! пакетов»), не нужен -- в установившемся режиме пакеты и так не
//! создаются. Правило чистоты общее с реактором: отмена с неожиданным
//! кодом (`WaitPacket::cancel_for_reuse`) или отказ привязки, в котором не
//! доказана вина объекта (`WaitPacket::survives_arm_failure`), закрывает
//! пакет; следующая привязка (при отказе привязки -- сразу же, один раз)
//! создаёт новый, а не повторяет отказ через `REARM_BACKOFF` бесконечно.

use std::collections::VecDeque;
use std::ops::Range;
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::auto::{join_unless_self, map_spawn_error};
use crate::client::{ClaimMode, Interrupt, SharedClient};
use crate::constants::{
    CLAIM_FREE, EVENT_CONNECT_REQ_SUFFIX, HANDSHAKE_SERVER_READY, MAX_RING_CAPACITY,
    MIN_MESSAGE_SIZE, RESERVE_SAFETY_MARGIN, RESERVE_TIMEOUT, SLOT_ID_NO_SLOT,
};
use crate::error::{DisconnectReason, Result, ShmError};
use crate::layout::{ChannelLayout, max_payload_for};
use crate::namespace::PrivateNamespace;
use crate::naming::{Direction, ObjectPath};
use crate::ntapi::STATUS_SUCCESS;
use crate::presence::{
    self, Failure, Presence, PresenceStatus, PresenceWait, PresenceWatch, Reported, ServerPresence,
    multi_beacon_name,
};
use crate::reactor::{CancelOutcome, Completion, CompletionPort, WaitPacket};
use crate::ring::{
    DrainBound, FreeSpace, SpinPolicy, UnreadProbe, rest_after_stale_disconnect, spin_until,
};
use crate::security::ObjectSecurity;
use crate::server::{Lingering, SharedServer, release_if_holder_dead};
use crate::shared::SharedView;
use crate::sync_ext::{LockExt, RwLockExt, join_worker, panic_error};
use crate::wait_delay_or;
use crate::win::{self, EventHandle, Mapping, ProcessWatch, status_to_error};

/// Максимальное количество клиентов по умолчанию
pub const DEFAULT_MAX_CLIENTS: u32 = 20;

/// Жёсткий предел числа слотов. Его задаёт протокол перебора, а не лимит
/// 64 handle у `NtWaitForMultipleObjects`: клиент перебирает слоты
/// `0..MAX_MULTI_CLIENTS` до первого отсутствующего -- предел ограничивает
/// цену перебора. Сервер публикует число слотов в `ControlBlock::slot_count`
/// каждого слота -- это подсказка (диагностика), клиенты по ней перебор не
/// ограничивают. Все слоты (до 256) обслуживает один поток-диспетчер на
/// порту завершения (0.10; до 0.10 -- worker на каждые 21 слот).
pub const MAX_MULTI_CLIENTS: u32 = 256;

/// Видов ожидания у слота (`WaitKind`): ключ пакета -- `slot * WAIT_KINDS +
/// вид`, для 256 слотов -- меньше 1024.
const WAIT_KINDS: usize = 4;

/// Ключ поста «пробуждение» (`NtSetIoCompletion`): `disconnect_client`,
/// `stop`, `force_disconnect`. Вне диапазона ключей слотов. Метка поста --
/// номер слота, чей набор ожиданий изменился, или `WAKE_ANY`.
const WAKE_KEY: usize = usize::MAX;

/// Метка поста `WAKE_KEY` без конкретного слота (`stop`/Drop).
const WAKE_ANY: usize = usize::MAX;

/// Пауза перед повтором неудавшейся привязки объекта слота к порту
/// (`Dispatcher::reconcile`): отказ ядра редок (нехватка ресурсов), повтор
/// нужен, чтобы слот не остался глухим, а пауза -- чтобы стойкий отказ не
/// стал горячим циклом.
const REARM_BACKOFF: Duration = Duration::from_millis(100);

/// Разовая (не периодическая) проверка «брошенного рукопожатия» после
/// handshake: клиент, чьё ожидание `S2C_CONNECT` истекло ровно в момент
/// ответа сервера, откатывает заявку и снимает claim, не сигналя
/// `DISCONNECT`. Снимает он его сразу после таймаута, поэтому одной проверки
/// через этот интервал достаточно.
const HANDSHAKE_VERIFY_DELAY: Duration = RESERVE_SAFETY_MARGIN;

/// Ограничение частоты разовой проверки живости по PID для клиента без
/// наблюдаемого процесса (старая версия + процесс не открылся). Это не
/// таймер: проверка делается только при пробуждении диспетчера по другой
/// причине и не чаще этого периода (`NtOpenProcess` дороже atomic load).
const LIVENESS_CHECK_INTERVAL: Duration = Duration::from_secs(3);

/// Callback-интерфейс для обработки событий мультиклиентного сервера.
///
/// # Один поток на все слоты (0.10)
///
/// Все колбэки всех слотов вызываются последовательно на одном
/// потоке-диспетчере сервера. Данные слотов читаются по кругу: за круг --
/// не больше `MultiOptions::recv_batch` сообщений слота, так что болтливый
/// клиент не заслоняет остальных. Но **медленный колбэк задерживает всех
/// клиентов**: пока выполняется `on_message` одного слота, сообщения,
/// подключения и отключения остальных ждут. Долгую обработку выносите из
/// колбэка (своя очередь, пул потоков).
///
/// **Паника колбэка изолирована** (0.10): паника в колбэке слота
/// (`on_message`, `on_client_connect`, `on_client_disconnect_reason`)
/// перехватывается, сервер вызывает `on_error(Some(slot),
/// ShmError::WorkerPanicked { .. })` и отключает этот слот обычным локальным
/// путём (как `disconnect_client`: клиент получает `DISCONNECT`, claim
/// снимает он сам; `on_client_disconnect_reason(Local)`), остаток пачки
/// сообщений этого слота не доставляется; остальные слоты обслуживаются
/// дальше. Исключение -- паника `on_message` при дочитывании кольца
/// уходящего клиента (`DISCONNECT`, смерть процесса, снятый claim): остаток
/// дочитывания не доставляется, а отключение идёт обычным путём ухода -- с
/// исходной причиной (`Graceful`/`PeerDied`) и обычным снятием claim-а, без
/// задержки `RESERVE_TIMEOUT`. Так же испорченный кадр при этом
/// дочитывании (0.10, Task 7): `on_error(Some(slot), ShmError::Corrupted)`
/// отдельно, причина -- исходная, а не `Error`. Паника самого `on_error` (и колбэка
/// отключения после паники) отбрасывается без второго вызова `on_error` и
/// без смены причины разрыва -- текст печатает panic hook. Перехват возможен только при `panic = "unwind"` у итогового
/// бинарника; при `panic = "abort"` паника завершает процесс.
///
/// Клиент, который шлёт `CONNECT_REQ` без `CLIENT_HELLO` («толчки») без
/// остановки, стоит серверу только CPU: пробуждение и O(1) работы на
/// каждый сигнал (сверка одного слота; полный проход всех слотов -- только
/// на дедлайне или не чаще раза в 3 с, Task 10). Порт отдаёт завершения по
/// очереди, остальных слотов и остановку это не заслоняет; ограничения
/// частоты нет.
pub trait MultiHandler: Send + Sync + 'static {
    /// Вызывается при подключении нового клиента
    fn on_client_connect(&self, client_id: u32);

    /// Вызывается при отключении клиента
    fn on_client_disconnect(&self, client_id: u32);

    /// Отключение клиента с причиной (0.9). По умолчанию вызывает
    /// `on_client_disconnect`; сервер вызывает ТОЛЬКО этот метод.
    /// `PeerDied` -- процесс клиента завершился без штатного отключения
    /// (удерживаемый handle процесса), `Local` -- `disconnect_client`.
    fn on_client_disconnect_reason(&self, client_id: u32, reason: DisconnectReason) {
        let _ = reason;
        self.on_client_disconnect(client_id);
    }

    /// Вызывается при получении сообщения от клиента
    fn on_message(&self, client_id: u32, data: &[u8]);

    /// Вызывается при ошибке (client_id = None для общих ошибок)
    fn on_error(&self, client_id: Option<u32>, err: ShmError) {
        let _ = (client_id, err);
    }
}

/// Callback-интерфейс для MultiClient
pub trait MultiClientHandler: Send + Sync + 'static {
    /// Вызывается при успешном подключении (slot_id — назначенный слот)
    fn on_connect(&self, slot_id: u32);

    /// Вызывается при отключении
    fn on_disconnect(&self);

    /// Отключение с причиной (0.9). По умолчанию вызывает `on_disconnect`;
    /// worker вызывает ТОЛЬКО этот метод. `PeerDied` -- процесс сервера
    /// завершился без штатного отключения.
    fn on_disconnect_reason(&self, reason: DisconnectReason) {
        let _ = reason;
        self.on_disconnect();
    }

    /// Вызывается при получении сообщения от сервера
    fn on_message(&self, data: &[u8]);

    /// Вызывается при переполнении внутренней send-очереди (`max_send_queue`):
    /// `dropped` — сколько старых неотправленных сообщений вытеснено новыми.
    fn on_overflow(&self, _dropped: u32) {}

    /// Вызывается при ошибке
    fn on_error(&self, err: ShmError) {
        let _ = err;
    }
}

/// Опции для MultiServer
#[derive(Clone, Debug)]
pub struct MultiOptions {
    /// Максимальное количество одновременных клиентов
    pub max_clients: u32,
    /// Страховочный таймаут ожидания потока-диспетчера. `None` (по
    /// умолчанию, 0.9+) -- только события и дедлайны: диспетчер спит в
    /// ядре до подключения,
    /// данных, отключения, смерти клиента, `disconnect_client`/`stop` или
    /// ближайшего дедлайна освобождения слота -- ни одного пробуждения в
    /// простое. `Some(t)` -- дополнительно просыпаться не реже `t` (для
    /// корректности не нужно). До 0.9 -- `Duration` (50 мс, опрос).
    pub poll_timeout: Option<Duration>,
    /// Сколько сообщений слота обрабатывается за один круг диспетчера
    /// (0.10: круг обходит все слоты с данными по очереди -- честная
    /// ротация; см. [`MultiHandler`]).
    pub recv_batch: usize,
    /// Раскладка каждого слота: размер колец и лимиты сообщений по
    /// направлениям (сервер -> клиент, клиент -> сервер). Каждый слот --
    /// отдельная секция `layout.segment_size()` байт commit-а, поэтому
    /// для многих клиентов с короткими сообщениями выгодны маленькие кольца.
    pub layout: ChannelLayout,
    /// Политика безопасности объектов каждого слота -- секции и 7 событий
    /// (0.10; до 0.10 -- NULL DACL). По умолчанию --
    /// `ObjectSecurity::CurrentUser`.
    pub security: ObjectSecurity,
    /// Частное пространство имён слотов (0.10): секции и события
    /// `base_name_N` создаются в нём под относительными именами (базовое имя
    /// -- без `Local\`/`Global\`). Клиенту нужно пространство той же
    /// границы (`MultiClientOptions::namespace`). `None` (по умолчанию) --
    /// общий `\BaseNamedObjects`.
    pub namespace: Option<Arc<PrivateNamespace>>,
    /// Спин потока-диспетчера перед сном в ядре (0.10): до
    /// `spin.iterations` проверок колец клиент -> сервер всех подключённых
    /// слотов по позициям (и остановки), прежде чем уснуть на порту. Один
    /// бюджет на каждое засыпание (в простое -- ноль пробуждений). После
    /// удачного спина данные читаются, а порт всё равно опрашивается
    /// (нулевой таймаут) до следующего спина -- болтливый слот не заслоняет
    /// `CONNECT_REQ`/`DISCONNECT`/смерть процесса остальных.
    ///
    /// Во время спина `CONNECT_REQ`, `DISCONNECT` и смерть клиентов не
    /// наблюдаются: они замечаются не позже конца бюджета. Цена -- занятое
    /// ядро CPU на время спина; длительность итерации (`pause`) зависит от
    /// CPU (порядка 10..140 тактов). По умолчанию -- `SpinPolicy::NONE`.
    /// Пиру не виден.
    pub spin: SpinPolicy,
}

impl Default for MultiOptions {
    fn default() -> Self {
        Self {
            max_clients: DEFAULT_MAX_CLIENTS,
            poll_timeout: None,
            recv_batch: 32,
            layout: ChannelLayout::DEFAULT,
            security: ObjectSecurity::default(),
            namespace: None,
            spin: SpinPolicy::NONE,
        }
    }
}

/// Опции для MultiClient
#[derive(Clone, Debug)]
pub struct MultiClientOptions {
    /// Таймаут подключения к слоту
    pub slot_timeout: Duration,
    /// Страховочный таймаут ожидания подключённого клиента. `None` (по
    /// умолчанию, 0.9+) -- только события (данные, отключение, смерть
    /// сервера, `send`/`stop`). До 0.9 -- `Duration` (50 мс, опрос).
    pub poll_timeout: Option<Duration>,
    /// Пауза перед новой попыткой захвата слота (0.9): свободных слотов нет,
    /// подключение не удалось или разорвано сервером (разовая пауза после
    /// конца сессии; после смерти сервера её нет). Остановка прерывает
    /// паузу сразу. До 0.9 роль паузы играл `poll_timeout` (50 мс).
    ///
    /// 0.10 (Task 11, INTEROP R19): **сервера нет** (ни одного слота, или
    /// процесс сервера завершился) -- клиент не опрашивает раз в
    /// `retry_delay`, а спит в ядре на маяке присутствия базы
    /// `<база>_multi_beacon` и подключается сразу, как сервер его взведёт. Пауза
    /// `retry_delay` остаётся: после конца сессии (разово), при занятых
    /// или чужих слотах (`NoFreeSlot`/`AccessDenied`, до уведомления
    /// «claim освобождён») и как резерв (решение R-M) -- маяк недоступен
    /// (нет прав, нет привилегии `Global\`), процесс сервера не открылся,
    /// подключение к слоту не удалось. Одинаковую ошибку вне подключения
    /// ПОДРЯД `on_error` сообщает один раз (кроме `NoFreeSlot` -- о каждой
    /// попытке); другая ошибка между ними -- и повтор сообщается снова.
    pub retry_delay: Duration,
    /// Резервная проверка обнаружения сервера (0.10, решение W(3); см.
    /// `AutoOptions::discovery_fallback`): пока сервера нет и клиент спит на
    /// маяке базы или процессе сервера, попытка захвата -- не реже этого
    /// интервала. Подключённый клиент по нему не просыпается. По умолчанию
    /// -- `Some(DEFAULT_DISCOVERY_FALLBACK)` (10 с); `None` -- только события;
    /// меньше 1 мс (в том числе `Duration::ZERO`) -- `MultiClient::connect` отказывает
    /// `ShmError::InvalidConfig`.
    pub discovery_fallback: Option<Duration>,
    /// Количество сообщений за один цикл
    pub recv_batch: usize,
    /// Максимум неотправленных сообщений во внутренней очереди перед сбросом
    /// самого старого (overwrite-семантика, как у `AutoOptions.max_send_queue`).
    /// Без этого предела очередь росла бы неограниченно, если пир завис/тормозит.
    pub max_send_queue: usize,
    /// Частное пространство имён, в котором искать слоты (0.10; см.
    /// `MultiOptions::namespace`). `None` (по умолчанию) -- общий
    /// `\BaseNamedObjects`.
    pub namespace: Option<Arc<PrivateNamespace>>,
    /// Политика объектов маяка присутствия базы `<база>_multi_beacon`, если их
    /// создаёт клиент (0.10, Task 11; клиент стартовал раньше сервера). DACL
    /// маяка задаёт тот, кто создал его первым (решение R-L): служба на
    /// `Global\` с `ObjectSecurity::Everyone` требует той же политики у
    /// клиентов (или старта службы первой), иначе клиенты другого
    /// пользователя получат отказ в доступе к маяку и перейдут на резервный
    /// повтор `retry_delay`. По умолчанию -- `ObjectSecurity::CurrentUser`.
    pub security: ObjectSecurity,
    /// Спин подключённого worker-а перед сном в ядре (0.10): до
    /// `spin.iterations` проверок кольца сервер -> клиент по позициям (и
    /// новых `send`, и остановки). Один бюджет на каждое засыпание; после
    /// удачного спина набор событий всё равно опрашивается (нулевой
    /// таймаут).
    ///
    /// Во время спина `DISCONNECT` и смерть сервера не наблюдаются: они
    /// замечаются не позже конца бюджета. Цена -- занятое ядро CPU на время
    /// спина; длительность итерации (`pause`) зависит от CPU (порядка
    /// 10..140 тактов). По умолчанию -- `SpinPolicy::NONE`. Пиру не виден.
    pub spin: SpinPolicy,
}

impl Default for MultiClientOptions {
    fn default() -> Self {
        Self {
            slot_timeout: Duration::from_secs(5),
            poll_timeout: None,
            retry_delay: Duration::from_millis(250),
            discovery_fallback: Some(presence::DEFAULT_DISCOVERY_FALLBACK),
            recv_batch: 32,
            max_send_queue: 256,
            namespace: None,
            security: ObjectSecurity::default(),
            spin: SpinPolicy::NONE,
        }
    }
}

/// Состояние одного клиентского слота
struct ClientSlot {
    id: u32,
    server: SharedServer,
    connected: bool,
    /// Не подключён: чужой claim (token) и момент, когда сервер ВПЕРВЫЕ его
    /// увидел -- дедлайн освобождения = момент + `reserve_timeout`. Token
    /// хранится, чтобы новый захват не унаследовал возраст прежнего.
    claim_seen: Option<(u32, Instant)>,
    /// Подключён: разовый дедлайн проверки брошенного рукопожатия.
    verify_at: Option<Instant>,
    /// Подключён: claim (token) клиента на момент рукопожатия -- при его
    /// штатном уходе сервер снимает claim только CAS-ом с этим значением.
    session_claim: u32,
    /// Подключён клиент, не передавший PID в handshake (старая версия):
    /// наблюдение за процессом-владельцем claim-а (PID из
    /// `ControlBlock::slot_owner_pid`) -- тоже handle в наборе ожидания.
    owner_watch: Option<ProcessWatch>,
    /// Подключён без наблюдаемого процесса: момент последней разовой
    /// проверки живости по PID (ограничение частоты, не таймер).
    last_liveness_check: Option<Instant>,
    /// Handle процессов отключённых клиентов: закрываются диспетчером в
    /// конце сверки привязок этого слота (`Dispatcher::reconcile`, Task 10;
    /// прежде -- в общем проходе всех слотов), а не под блокировкой
    /// вызывающего `disconnect_client`. Пакет ожидания, ещё привязанный к
    /// такому процессу, закрытия не боится -- ядро держит ссылку на объект
    /// до срабатывания или отмены привязки; снимает её та же сверка, до
    /// закрытия handle.
    retired: Vec<ProcessWatch>,
    /// Task 7: не подключён -- «задержавшийся владелец»: процесс клиента
    /// сессии, которую закрыл сервер (`force_disconnect`, отказ S7), и токен
    /// её claim-а, пока claim всё ещё его. Процесс привязан к порту
    /// (`WaitKind::Peer`): подтверждённая смерть снимает claim сразу
    /// (`release_if_holder_dead`), без `reserve_timeout`. Один на слот;
    /// снимается, когда claim сменился (клиент ушёл сам), при новом
    /// рукопожатии и при остановке.
    lingering: Option<Lingering>,
}

impl ClientSlot {
    const fn new(id: u32, server: SharedServer) -> Self {
        Self {
            id,
            server,
            connected: false,
            claim_seen: None,
            verify_at: None,
            session_claim: CLAIM_FREE,
            owner_watch: None,
            last_liveness_check: None,
            retired: Vec::new(),
            lingering: None,
        }
    }

    /// Handle процесса клиента для набора ожидания: из handshake (0.8+) или
    /// процесс-владелец claim-а (клиент старой версии).
    fn peer_handle(&self) -> Option<BorrowedHandle<'_>> {
        self.server
            .peer_wait_handle()
            .or_else(|| self.owner_watch.as_ref().map(AsHandle::as_handle))
    }

    /// PID наблюдаемого процесса клиента.
    fn watched_pid(&self) -> Option<u32> {
        self.server
            .peer_pid()
            .or_else(|| self.owner_watch.as_ref().map(ProcessWatch::pid))
    }

    /// Разорвать подключение слота на стороне сервера: состояния handshake
    /// -> IDLE, handle процессов -> в `retired` (закроет диспетчер).
    ///
    /// `linger` (Task 7) -- сессию закрывает сервер (`force_disconnect`), а
    /// клиент, возможно, жив: handle его процесса остаётся наблюдаемым
    /// («задержавшийся владелец», `lingering`), если claim всё ещё claim
    /// сессии. Тот же handle уже привязан к порту (`WaitKind::Peer`) --
    /// привязка просто продолжается.
    fn reset_connection(&mut self, linger: bool) {
        self.connected = false;
        self.claim_seen = None;
        self.verify_at = None;
        self.last_liveness_check = None;
        let watch = match (self.server.take_peer(), self.owner_watch.take()) {
            (Some(peer), owner) => {
                self.retired.extend(owner);
                Some(peer)
            }
            (None, owner) => owner,
        };
        self.server.mark_disconnected();
        let Some(watch) = watch else {
            return;
        };
        if linger && self.session_claim != CLAIM_FREE && self.claim() == self.session_claim {
            self.retire_lingering();
            self.lingering = Some((watch, self.session_claim));
        } else {
            self.retired.push(watch);
        }
    }

    /// Снять «задержавшегося владельца»: handle -- в `retired` (закроется
    /// в конце сверки привязок слота).
    fn retire_lingering(&mut self) {
        if let Some((watch, _)) = self.lingering.take() {
            self.retired.push(watch);
        }
    }

    fn claim(&self) -> u32 {
        self.server
            .view()
            .control_block()
            .slot_claim
            .load(Ordering::Acquire)
    }

    /// Тесты (решение H): владелец claim-а (`slot_owner_pid`/`owner_start`)
    /// и наблюдаемый процесс сессии -- процесс `watch` (`pid`, `start`).
    #[cfg(test)]
    fn substitute_owner(&mut self, watch: ProcessWatch, pid: u32, start: u64) {
        let control = self.server.view().control_block();
        control.announce_owner_start(start);
        control.slot_owner_pid.store(pid, Ordering::Release);
        if let Some(old) = self.server.replace_peer(watch) {
            self.retired.push(old);
        }
    }
}

/// Итог прохода обслуживания слотов.
#[derive(Debug, Default)]
struct Sweep {
    /// Осиротевшие подключённые слоты: `(slot_id, ожидаемый claim, причина)`.
    orphaned: Vec<(u32, u32, DisconnectReason)>,
    /// Слоты, с которых снят «задержавшийся владелец» (Task 7): их
    /// привязку процесса диспетчер обязан снять (`Dispatcher::reconcile`).
    relinquished: Vec<u32>,
    /// Ближайший дедлайн (освобождение протухшего захвата, проверка
    /// рукопожатия) -- до него и ждёт диспетчер.
    next_deadline: Option<Instant>,
}

impl Sweep {
    fn deadline(&mut self, at: Instant) {
        self.next_deadline = Some(self.next_deadline.map_or(at, |d| d.min(at)));
    }
}

/// Мультиклиентный сервер.
///
/// # Конкурентное распределение слотов (без централизованного lobby)
///
/// Сервер создаёт N независимых сегментов-слотов `base_name_0..N-1`. Клиент
/// сам выбирает слот: пробегает слоты и атомарно захватывает первый свободный
/// через `compare_exchange(CLAIM_FREE -> token)` на `ControlBlock::slot_claim`
/// сегмента слота, затем выполняет обычный handshake `SharedClient`. Так N
/// клиентов подключаются ПОЛНОСТЬЮ КОНКУРЕНТНО, захватывая РАЗНЫЕ слоты на
/// РАЗНОЙ памяти — без общего состояния, без coalescing событий, без коллизий.
///
/// Возвращается как `Arc<MultiServer>`; `Drop` последней ручки
/// останавливает сервер (`stop`). Поток-диспетчер держит только ядро
/// (`MultiCore`), а не саму ручку, поэтому сервер, который отпустили без
/// `stop()`, не живёт вечно. Методы ядра доступны через `Deref`.
///
/// Все слоты обслуживает один поток (0.10): колбэки -- последовательно на
/// нём (см. [`MultiHandler`] о медленных колбэках).
pub struct MultiServer {
    core: Arc<MultiCore>,
}

impl std::ops::Deref for MultiServer {
    type Target = MultiCore;

    fn deref(&self) -> &MultiCore {
        &self.core
    }
}

impl Drop for MultiServer {
    fn drop(&mut self) {
        self.core.stop();
    }
}

impl std::fmt::Debug for MultiServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&*self.core, f)
    }
}

mod core_type {
    use super::{Arc, AtomicBool, AtomicU64, ClientSlot, CompletionPort, Duration, JoinHandle};
    use super::{MultiHandler, MultiOptions, Mutex, PresenceStatus, RwLock, ServerPresence};

    /// Состояние сервера, общее для ручки `MultiServer` и потока-диспетчера.
    /// Вне крейта тип не именуем: его методы доступны через `Deref` ручки.
    #[expect(
        unnameable_types,
        reason = "ядро -- деталь реализации: методы доступны через `Deref` ручки `MultiServer`"
    )]
    pub struct MultiCore {
        pub(in crate::multi) base_name: String,
        pub(in crate::multi) slots: RwLock<Vec<Mutex<ClientSlot>>>,
        pub(in crate::multi) max_clients: u32,
        pub(in crate::multi) running: Arc<AtomicBool>,
        /// Свой порт завершения потока-диспетчера (не общий реактор): к нему
        /// привязаны объекты ожидания всех слотов; `disconnect_client`/`stop`
        /// будят диспетчер постом `WAKE_KEY`.
        pub(in crate::multi) port: CompletionPort,
        /// Поток-диспетчер (один; вектор -- чтобы `stop` забирал его один раз).
        pub(in crate::multi) worker_handles: Mutex<Vec<JoinHandle<()>>>,
        pub(in crate::multi) handler: Arc<dyn MultiHandler>,
        pub(in crate::multi) options: MultiOptions,
        /// Через сколько протухает захват слота без подключения.
        pub(in crate::multi) reserve_timeout: Duration,
        /// Число пробуждений диспетчера (возвратов из ожидания порта) --
        /// доказательство отсутствия опроса в тестах.
        pub(in crate::multi) wakeups: AtomicU64,
        /// Отброшенные устаревшие завершения (снятая или перепривязанная
        /// привязка) -- тесты.
        #[cfg(test)]
        pub(in crate::multi) stale_completions: AtomicU64,
        /// Блокировки слотов диспетчером (проход дедлайнов, обработка
        /// `CONNECT_REQ`, сверка привязок) -- тесты: работа на пробуждение
        /// O(1) (Task 10). Relaxed: тестовая статистика, ничего не публикует.
        #[cfg(test)]
        pub(in crate::multi) slot_visits: AtomicU64,
        /// Пост пробуждения слота не удался (`wake_slot`): ближайший виток
        /// диспетчера сверяет все слоты (fix round 1, M5).
        pub(in crate::multi) wake_lost: AtomicBool,
        /// Маяк присутствия базы `<база>_multi_beacon` (0.10, R19): взведён после
        /// создания всех слотов, `stop` сбрасывает его первым шагом и
        /// забирает (повторный `stop` маяк не трогает).
        pub(in crate::multi) presence: Mutex<Option<ServerPresence>>,
        /// Состояние маяка базы (`presence()`, решение W(1)).
        pub(in crate::multi) presence_status: Mutex<PresenceStatus>,
        /// Сколько ближайших привязок провалить (тест повтора привязки).
        #[cfg(test)]
        pub(in crate::multi) fail_arms: std::sync::atomic::AtomicU32,
    }
}
use core_type::MultiCore;

impl MultiServer {
    /// Запуск мультиклиентного сервера
    pub fn start(
        base_name: &str,
        handler: Arc<dyn MultiHandler>,
        options: MultiOptions,
    ) -> Result<Arc<Self>> {
        Self::start_with(base_name, handler, options, RESERVE_TIMEOUT)
    }

    /// `start` с заданным таймаутом резервирования (тесты дедлайна).
    pub(crate) fn start_with(
        base_name: &str,
        handler: Arc<dyn MultiHandler>,
        options: MultiOptions,
        reserve_timeout: Duration,
    ) -> Result<Arc<Self>> {
        if options.max_clients == 0 || options.max_clients > MAX_MULTI_CLIENTS {
            return Err(ShmError::InvalidConfig("max_clients must be in 1..=256"));
        }
        options.layout.validate()?;

        // Создаём N независимых сегментов-слотов. Lobby не нужен — клиенты
        // захватывают слоты сами через атомарный claim (см. doc MultiServer).
        let mut slots = Vec::with_capacity(options.max_clients as usize);
        // Дескриптор -- один на все слоты (токен процесса читается один раз).
        let security = options.security.attributes()?;
        for slot_id in 0..options.max_clients {
            // Каждый слот публикует число слотов -- подсказка для диагностики,
            // перебор клиента им не ограничивается.
            let server = SharedServer::start_slot(
                options.namespace.as_ref(),
                &format!("{base_name}_{slot_id}"),
                options.layout,
                options.max_clients,
                &security,
                // Маяк -- один на базу (ниже), не на слот (R19).
                Presence::Off,
            )?;
            slots.push(Mutex::new(ClientSlot::new(slot_id, server)));
        }

        let server = Arc::new(MultiCore {
            base_name: base_name.to_owned(),
            slots: RwLock::new(slots),
            max_clients: options.max_clients,
            running: Arc::new(AtomicBool::new(true)),
            port: CompletionPort::new()?,
            worker_handles: Mutex::new(Vec::new()),
            handler,
            options,
            reserve_timeout,
            wakeups: AtomicU64::new(0),
            #[cfg(test)]
            stale_completions: AtomicU64::new(0),
            #[cfg(test)]
            slot_visits: AtomicU64::new(0),
            wake_lost: AtomicBool::new(false),
            presence: Mutex::new(None),
            presence_status: Mutex::new(PresenceStatus::Disabled),
            #[cfg(test)]
            fail_arms: AtomicU32::new(0),
        });

        let server_clone = server.clone();
        // Имя потока -- только в debug (короткий непрозрачный тег), чтобы в
        // release ни библиотека, ни имя канала не светились в списке потоков.
        #[cfg_attr(
            not(debug_assertions),
            expect(unused_mut, reason = "имя -- только в debug")
        )]
        let mut builder = thread::Builder::new();
        #[cfg(debug_assertions)]
        {
            builder = builder.name(format!("xsm-{base_name}"));
        }
        let handle = crate::thread_hook::spawn(builder, move || {
            Dispatcher::new(&server_clone).run();
        })
        .map_err(|e| map_spawn_error(e, "spawn multi dispatcher"))?;
        server.worker_handles.lock_clean().push(handle);

        // R19: маяк базы `<база>_multi_beacon` -- строго ПОСЛЕ создания всех
        // слотов (секций и событий): клиент, проснувшийся по нему, находит
        // слот 0. Лучшее усилие (решение W(1)): отказ создания или взвода
        // старт не отказывает -- `presence()` сообщает причину, клиенты
        // находят сервер резервной проверкой `discovery_fallback`.
        let (beacon, status) = ServerPresence::start(
            server.options.namespace.as_ref(),
            &multi_beacon_name(base_name),
            &security,
        );
        *server.presence.lock_clean() = beacon;
        *server.presence_status.lock_clean() = status;
        Ok(Arc::new(Self { core: server }))
    }
}

impl MultiCore {
    /// Отправка сообщения конкретному клиенту
    pub fn send_to(&self, client_id: u32, data: &[u8]) -> Result<()> {
        let slots = self.slots.read_clean();
        let slot_mutex = slots
            .get(client_id as usize)
            .ok_or(ShmError::NotConnected)?;
        let slot = slot_mutex.lock_clean();

        if !slot.connected {
            return Err(ShmError::NotConnected);
        }

        slot.server.send_to_client(data)?;
        Ok(())
    }

    /// Отправка клиенту **без перезаписи** непрочитанных данных
    /// (`SharedServer::try_send_to_client`): `Err(QueueFull)`, если в кольце
    /// слота нет места; кольцо тогда не меняется.
    pub fn try_send_to(&self, client_id: u32, data: &[u8]) -> Result<()> {
        let slots = self.slots.read_clean();
        let slot_mutex = slots
            .get(client_id as usize)
            .ok_or(ShmError::NotConnected)?;
        let slot = slot_mutex.lock_clean();

        if !slot.connected {
            return Err(ShmError::NotConnected);
        }

        slot.server.try_send_to_client(data)?;
        Ok(())
    }

    /// Свободное место в кольце Server -> Client слота (точное значение:
    /// запись в слот идёт только под его mutex-ом, так что до следующего
    /// `send_to`/`try_send_to` это нижняя граница). `None` -- слота нет или
    /// клиент не подключён.
    #[must_use]
    pub fn free_space(&self, client_id: u32) -> Option<FreeSpace> {
        let slots = self.slots.read_clean();
        let slot = slots.get(client_id as usize)?.lock_clean();
        slot.connected.then(|| slot.server.free_space())
    }

    /// Отправка сообщения всем подключённым клиентам
    pub fn broadcast(&self, data: &[u8]) -> Result<u32> {
        let slots = self.slots.read_clean();
        let mut sent_count = 0u32;

        for slot_mutex in slots.iter() {
            let slot = slot_mutex.lock_clean();
            if slot.connected && slot.server.send_to_client(data).is_ok() {
                sent_count += 1;
            }
        }

        Ok(sent_count)
    }

    /// Принудительное отключение клиента (`on_client_disconnect_reason(Local)`).
    ///
    /// Слот освобождает сам клиент, получив `DISCONNECT`; клиента, умершего
    /// раньше, чем он это заметил, -- сервер сразу по подтверждённой смерти
    /// его процесса (0.10), без ожидания `RESERVE_TIMEOUT`.
    pub fn disconnect_client(&self, client_id: u32) -> Result<()> {
        if client_id >= self.max_clients {
            return Err(ShmError::NotConnected);
        }
        self.force_disconnect(client_id, DisconnectReason::Local);
        Ok(())
    }

    /// Разорвать подключение слота (если подключён) с причиной `reason`.
    fn force_disconnect(&self, client_id: u32, reason: DisconnectReason) {
        let slots = self.slots.read_clean();
        let Some(slot_mutex) = slots.get(client_id as usize) else {
            return;
        };
        let mut slot = slot_mutex.lock_clean();

        if slot.connected {
            // Порядок: состояния handshake -> IDLE, потом `DISCONNECT` --
            // клиент, проснувшись, видит уже отключённый слот (так же
            // `wait_for_space` отличает настоящее отключение от устаревшего
            // сигнала). Ревизия 2: claim НЕ освобождаем -- его снимет сам
            // клиент, получив `DISCONNECT` (CAS token -> 0). Раньше слот
            // освобождался до того, как клиент проснулся: следующий клиент
            // успевал его захватить, и его `complete_handshake` сбрасывал
            // `DISCONNECT` прежнего -- два клиента на одном SPSC-кольце.
            // Task 7: клиента, умершего, не заметив отключения, освобождает
            // его подтверждённая смерть -- handle процесса остаётся
            // привязанным к порту («задержавшийся владелец», `lingering`);
            // claim снимает диспетчер (`handle_slot_peer_died`) ПОСЛЕ
            // `DISCONNECT` ниже: блокировка слота держится до сигнала.
            // Клиента без наблюдаемого процесса -- дедлайн `reserve_timeout`.
            slot.reset_connection(true);
            slot.server.signal_disconnect();
            drop(slot);
            drop(slots);
            // Набор ожиданий слота изменился (слот снова ждёт CONNECT_REQ) -- диспетчер сверит привязки.
            self.wake_slot(client_id);
            self.handler.on_client_disconnect_reason(client_id, reason);
        }
    }

    /// Получение списка подключённых клиентов
    pub fn connected_clients(&self) -> Vec<u32> {
        let slots = self.slots.read_clean();
        slots
            .iter()
            .filter_map(|slot_mutex| {
                let slot = slot_mutex.lock_clean();
                if slot.connected { Some(slot.id) } else { None }
            })
            .collect()
    }

    /// Количество подключённых клиентов
    pub fn client_count(&self) -> u32 {
        let slots = self.slots.read_clean();
        slots
            .iter()
            .filter(|slot_mutex| slot_mutex.lock_clean().connected)
            .count() as u32
    }

    /// Проверка подключения конкретного клиента
    pub fn is_client_connected(&self, client_id: u32) -> bool {
        let slots = self.slots.read_clean();
        slots
            .get(client_id as usize)
            .is_some_and(|slot_mutex| slot_mutex.lock_clean().connected)
    }

    /// Жив ли процесс клиента: `Some(true/false)` -- клиент наблюдается через
    /// удерживаемый handle процесса; `None` -- не подключён или наблюдения
    /// нет (клиент старой версии, процесс не открылся).
    #[must_use]
    pub fn is_client_alive(&self, client_id: u32) -> Option<bool> {
        let slots = self.slots.read_clean();
        let slot = slots.get(client_id as usize)?.lock_clean();
        if !slot.connected {
            return None;
        }
        slot.server
            .is_peer_alive()
            .or_else(|| slot.owner_watch.as_ref().map(|watch| !watch.has_exited()))
    }

    /// Остановка сервера.
    ///
    /// Синхронно дожидается выхода потока-диспетчера перед возвратом — после
    /// return ни один callback (`on_message`/`on_client_connect`/`on_error`)
    /// больше не будет вызван -- вызывающий может сразу после возврата
    /// освободить состояние, на которое ссылается handler. Диспетчер спит в
    /// ядре без таймаута -- его будит пост в порт, поэтому `stop()`
    /// возвращается сразу (как только вернётся выполняемый колбэк).
    /// Идемпотентна: повторный вызов — no-op.
    ///
    /// Ревизия 2: подключённым клиентам `DISCONNECT` сигналится сразу здесь
    /// (состояние `IDLE` -> `DISCONNECT`, как `disconnect_client`, без
    /// колбэков), а не когда отпустят последний `Arc` сервера (Drop слотов):
    /// раньше клиент оставался «подключённым» к остановленному серверу, пока
    /// кто-то держал `Arc<MultiServer>`.
    pub fn stop(&self) {
        // R19: маяк присутствия базы -- первым шагом, до остановки
        // диспетчера и отключения слотов.
        let presence = self.presence.lock_clean().take();
        if let Some(presence) = presence {
            presence.lower();
        }
        *self.presence_status.lock_clean() = PresenceStatus::Disabled;
        self.running.store(false, Ordering::Release);
        // Отказ поста возможен только при негодном порте -- тогда диспетчер
        // не спит в нём и выйдет по `running` на ближайшем витке.
        let _ = self.port.post(WAKE_KEY, WAKE_ANY);
        // Self-join исключён: если последний `Arc` отпустил сам диспетчер
        // (Drop на его потоке), свой handle не джойним -- поток уже выходит
        // и, выходя, снимает свои привязки, возвращая потреблённые сигналы
        // (`Dispatcher::release_all`), -- `DISCONNECT` ниже клиент получит.
        let handles: Vec<_> = self.worker_handles.lock_clean().drain(..).collect();
        for handle in handles {
            if handle.thread().id() != thread::current().id()
                && let Err(err) = join_worker(handle)
            {
                // Паника диспетчера (колбэк обработчика) -- не молча: на
                // потоке, вызвавшем `stop()`, до возврата из него.
                self.handler.on_error(None, err);
            }
        }
        // Диспетчер остановлен (или это он сам -- выйдет по `running`):
        // отключаем подключённые слоты под их mutex-ами.
        let slots = self.slots.read_clean();
        for slot_mutex in slots.iter() {
            let mut slot = slot_mutex.lock_clean();
            if slot.connected {
                slot.reset_connection(false);
                if let Some(events) = slot.server.events() {
                    let _ = events.disconnect.set();
                }
            }
            // Наблюдать «задержавшихся владельцев» больше некому.
            slot.retire_lingering();
        }
    }

    /// Маяк присутствия базы `<база>_multi_beacon` (0.10, INTEROP R19; см.
    /// `SharedServer::presence`): `Unavailable` -- сервер работает без
    /// маяка, `MultiClient`-ы находят его резервной проверкой
    /// `MultiClientOptions::discovery_fallback`; после `stop` -- `Disabled`.
    #[must_use]
    pub fn presence(&self) -> PresenceStatus {
        self.presence_status.lock_clean().clone()
    }

    /// Базовое имя канала
    pub fn base_name(&self) -> &str {
        &self.base_name
    }

    /// Получение имени канала для конкретного слота
    pub fn channel_name(&self, slot_id: u32) -> Option<String> {
        if slot_id < self.max_clients {
            Some(format!("{}_{}", self.base_name, slot_id))
        } else {
            None
        }
    }

    /// Число пробуждений диспетчера (тесты: ноль в простое).
    #[cfg(test)]
    pub(crate) fn wakeups(&self) -> u64 {
        self.wakeups.load(Ordering::Acquire)
    }

    /// Разбудить диспетчер: набор ожиданий слота изменился (слот отключён
    /// не им) -- он сверит привязки слота (`Dispatcher::reconcile`).
    ///
    /// Пост не удался (`NtSetIoCompletion`, нехватка ресурсов -- редко):
    /// отметка `wake_lost` -- диспетчер на ближайшем витке (следующее
    /// событие любого слота, дедлайн или `stop`) сверит ВСЕ слоты, закрыв
    /// handle отключённых клиентов (`ClientSlot::retired`), а не ждёт
    /// события именно этого слота (Task 10, fix round 1, M5).
    fn wake_slot(&self, slot_id: u32) {
        if self.port.post(WAKE_KEY, slot_id as usize).is_err() {
            self.wake_lost.store(true, Ordering::Release);
        }
    }

    /// Полный проход слотов `range` (Task 10, решение R: диспетчер зовёт
    /// его для всех слотов только на ближайшем дедлайне `Dispatcher::
    /// sweep_due` и не чаще `LIVENESS_CHECK_INTERVAL` оппортунистически --
    /// на пробуждении по другой причине; не на каждом витке). Никаких
    /// собственных пробуждений не планирует, кроме ДЕДЛАЙНОВ в
    /// `Sweep::next_deadline`.
    ///
    /// - **не подключён, claim занят** -- захват без подключения. Дедлайн
    ///   `first_seen + reserve_timeout`; наступил -- CAS(claim -> FREE).
    ///   «Первое наблюдение» (`observe_claim`) случается и вне прохода -- при
    ///   сверке слота после любого его события, в том числе «толчка»
    ///   клиента, не нашедшего свободного слота (`nudge_stale_claims`), с
    ///   дедлайном в `Dispatcher::schedule`, так что протухший захват не
    ///   ждёт тика.
    /// - **подключён, claim снят** -- «осиротевший» слот (брошенное
    ///   рукопожатие: сервер завершил handshake ровно когда клиент отвалился
    ///   по таймауту, снял claim и ушёл без `DISCONNECT`). Ловится на
    ///   ближайшем полном проходе, гарантированно -- на разовом дедлайне
    ///   `HANDSHAKE_VERIFY_DELAY` после handshake.
    /// - **подключён, процесс не наблюдается** (клиент старой версии, handle
    ///   не открылся) -- разовая проверка живости по PID владельца claim-а,
    ///   только в полном проходе (на пробуждении по другой причине) и не
    ///   чаще `LIVENESS_CHECK_INTERVAL`. Периодического тика ради неё нет:
    ///   смерть такого клиента замечается при следующем событии сервера.
    ///
    /// Отключение осиротевших (`handle_orphaned_slot_disconnect`) --
    /// вызывающим, ВНЕ блокировок; он перепроверяет claim через
    /// CAS(ожидаемый -> FREE) перед мутацией.
    ///
    /// Инвариант обеспечен принудительно (см. `client_worker`): клиентский
    /// `slot_timeout` всегда клампится ниже `RESERVE_TIMEOUT`, иначе сервер
    /// мог бы отнять слот у легитимно подключающегося клиента.
    fn reclaim_stale_claims(&self, range: Range<u32>, now: Instant) -> Sweep {
        let mut sweep = Sweep::default();
        let slots = self.slots.read_clean();
        for slot_id in range {
            let Some(slot_mutex) = slots.get(slot_id as usize) else {
                continue;
            };
            let mut slot = slot_mutex.lock_clean();
            #[cfg(test)]
            self.slot_visits.fetch_add(1, Ordering::Relaxed);
            let claim = slot.claim();
            if slot.connected {
                if claim == CLAIM_FREE {
                    sweep
                        .orphaned
                        .push((slot.id, CLAIM_FREE, DisconnectReason::Graceful));
                    continue;
                }
                if let Some(at) = slot.verify_at {
                    if now >= at {
                        slot.verify_at = None;
                    } else {
                        sweep.deadline(at);
                    }
                }
                if slot.peer_handle().is_some() {
                    continue; // смерть процесса придёт событием
                }
                let due = slot.last_liveness_check.is_none_or(|last| {
                    now.saturating_duration_since(last) >= LIVENESS_CHECK_INTERVAL
                });
                if due {
                    slot.last_liveness_check = Some(now);
                    let owner_pid = slot
                        .server
                        .view()
                        .control_block()
                        .slot_owner_pid
                        .load(Ordering::Acquire);
                    let owner_start = slot.server.view().control_block().owner_start_time();
                    if !win::is_process_alive(owner_pid, owner_start) {
                        sweep
                            .orphaned
                            .push((slot.id, claim, DisconnectReason::PeerDied));
                    }
                }
                continue;
            }
            // Не подключён. Task 7: «задержавшийся владелец» -- claim уже
            // не его (ушёл сам) или он подтверждённо умер (смерть могла
            // опередить привязку) -- наблюдение снимается, привязку `Peer`
            // слота сверит вызывающий (`Sweep::relinquished`).
            if let Some((watch, token)) = slot.lingering.as_ref()
                && (claim != *token
                    || release_if_holder_dead(slot.server.view().control_block(), watch, *token))
            {
                slot.retire_lingering();
                sweep.relinquished.push(slot.id);
            }
            // Протухший захват -- по дедлайну (claim перечитывается: снятие
            // умершего «задержавшегося» его освободило).
            let Some((claim, due)) = self.observe_claim(&mut slot, now) else {
                continue;
            };
            if now >= due {
                // CAS: если за это время claim сменился, это уже другой захват.
                let _ = slot
                    .server
                    .view()
                    .control_block()
                    .slot_claim
                    .compare_exchange(claim, CLAIM_FREE, Ordering::AcqRel, Ordering::Acquire);
                slot.claim_seen = None;
            } else {
                sweep.deadline(due);
            }
        }
        sweep
    }

    /// Пробы колец клиент -> сервер подключённых слотов (спин диспетчера):
    /// `(слот, проба)`.
    ///
    /// Пробы -- адреса заголовков колец в секциях слотов; секции создаются
    /// в `start` и уничтожаются только вместе с ядром, то есть после join
    /// диспетчера, -- дольше `'s`. Проба читает только атомарные позиции и
    /// от блокировки слота не зависит.
    fn collect_probes<'s>(&'s self, probes: &mut Vec<(u32, UnreadProbe<'s>)>) {
        let slots = self.slots.read_clean();
        for slot_mutex in slots.iter() {
            let slot = slot_mutex.lock_clean();
            if slot.connected {
                let probe = slot.server.client_ring_probe();
                // SAFETY: секция слота (а с ней заголовок кольца) живёт до
                // уничтожения ядра, то есть дольше `'s` (см. документацию
                // функции); проба читает только атомарные позиции.
                probes.push((slot.id, unsafe { probe.extend() }));
            }
        }
    }

    /// `C2S_CONNECT_REQ` на свободном слоте. Принят клиент -- возвращает
    /// дедлайн проверки брошенного рукопожатия: диспетчер включает его в
    /// ближайший полный проход (`Dispatcher::schedule`).
    ///
    /// Task 10 (П3, решение R): работа -- только над ЭТИМ слотом. Толчок
    /// без заявки (`nudge_stale_claims`, откатившийся клиент, чужой
    /// процесс) -- первое наблюдение claim-а следом, в сверке этого же
    /// слота (`Dispatcher::reconcile` -> `observe_claim`); обход всех
    /// слотов по пустому сигналу не делается.
    fn handle_slot_connect(&self, slot_id: u32) -> Option<Instant> {
        let slots = self.slots.read_clean();
        let slot_mutex = slots.get(slot_id as usize)?;
        let mut slot = slot_mutex.lock_clean();
        #[cfg(test)]
        self.slot_visits.fetch_add(1, Ordering::Relaxed);
        if slot.connected {
            return None; // Уже подключён
        }

        // Handshake -- общий с `SharedServer` (`complete_handshake`): PID
        // клиента из `ControlBlock::client_pid` -> удерживаемый handle процесса,
        // сброс устаревшего `DISCONNECT`, `S2C_CONNECT`.
        match slot.server.accept_pending() {
            Ok(()) => {
                slot.connected = true;
                // Новая сессия: наблюдение за клиентом прошлой не нужно
                // (claim взят свободным -- уже не его).
                slot.retire_lingering();
                // Ревизия 3: claim сессии -- из рукопожатия (S6: claim
                // владельца зафиксированной заявки, а не следующего клиента).
                slot.session_claim = slot.server.session_claim();
                slot.claim_seen = None;
                slot.last_liveness_check = None;
                let verify_at = Instant::now() + HANDSHAKE_VERIFY_DELAY;
                slot.verify_at = Some(verify_at);
                if slot.server.peer_pid().is_none() {
                    // Клиент старой версии не передал PID в handshake, но
                    // PID владельца claim-а он пишет всегда -- наблюдаем его.
                    let owner_pid = slot
                        .server
                        .view()
                        .control_block()
                        .slot_owner_pid
                        .load(Ordering::Acquire);
                    let owner_start = slot.server.view().control_block().owner_start_time();
                    slot.owner_watch = ProcessWatch::open_peer(owner_pid, owner_start);
                }
                let id = slot.id;
                drop(slot);
                drop(slots);
                self.handler.on_client_connect(id);
                Some(verify_at)
            }
            Err(err) => {
                // Отказ S7 (Task 7): сессия, зафиксированная S5, закрыта
                // (`disconnect_local`), а клиент сессии оставлен
                // «задержавшимся владельцем» -- наблюдает его слот.
                if let Some(lingering) = slot.server.take_lingering() {
                    slot.retire_lingering();
                    slot.lingering = Some(lingering);
                }
                drop(slot);
                drop(slots);
                // `HandshakeFailed` -- `CONNECT_REQ` без `CLIENT_HELLO`:
                // клиент откатил заявку по таймауту (claim он снял сам) или
                // это «толчок» клиента, не нашедшего свободного слота
                // (`nudge_stale_claims`); claim не трогаем: живой захват
                // скоро подключится, протухший снимет дедлайн
                // `reserve_timeout`, который назначит сверка слота следом
                // (`Dispatcher::reconcile` -> `observe_claim`). Иное
                // -- отказ самого рукопожатия (п. 11: сигнал `S2C_CONNECT`,
                // `WindowsError`) -- сообщается, а не глотается молча.
                if !matches!(err, ShmError::HandshakeFailed | ShmError::AlreadyConnected) {
                    report(self, Some(slot_id), err);
                }
                None
            }
        }
    }

    /// Захват свободного слота без заявки: первое наблюдение claim-а
    /// (`claim_seen` -- токен и момент; новый токен -- новый отсчёт) и его
    /// дедлайн `момент + reserve_timeout`: `(токен, дедлайн)`. Сам отъём --
    /// в полном проходе на этом дедлайне (`reclaim_stale_claims`) и только
    /// CAS-ом именно этого токена. Claim свободен -- ни наблюдения, ни
    /// дедлайна.
    fn observe_claim(&self, slot: &mut ClientSlot, now: Instant) -> Option<(u32, Instant)> {
        let claim = slot.claim();
        if claim == CLAIM_FREE {
            slot.claim_seen = None;
            return None;
        }
        let seen_at = match slot.claim_seen {
            Some((token, at)) if token == claim => at,
            _ => {
                slot.claim_seen = Some((claim, now));
                now
            }
        };
        Some((claim, seen_at + self.reserve_timeout))
    }

    /// Дочитать кольцо ушедшего клиента -- ДО колбэка отключения, но только
    /// до снимка `write_pos`, сделанного здесь, в момент обнаружения ухода
    /// (`DISCONNECT`, смерть процесса, снятый claim). Дописанное после снимка
    /// не доставляется: враждебный клиент, который сигналит `DISCONNECT` (или
    /// снимает claim) и продолжает писать, иначе держал бы единственный
    /// поток-диспетчер в дочитывании, и все слоты стояли бы. Позиции -- по
    /// модулю 2^32; число кадров ограничено физической ёмкостью кольца
    /// (`min(max_messages, capacity / frame_size(MIN_MESSAGE_SIZE)) + 1`,
    /// `RingBuffer::drain_snapshot`), а позиция чтения ведётся приватно:
    /// пир, сдвинувший общий `read_pos` во время дочитывания, получает
    /// `Corrupted` (честный клиент к моменту ухода уже не пишет -- R14).
    ///
    /// `Corrupted` при дочитывании (решение C, Task 7) --
    /// `on_error(Some(slot), Corrupted)` и конец дочитывания, но НЕ разрыв:
    /// слот отключает вызывающий путь ухода со своей причиной и своим
    /// снятием claim-а.
    fn drain_slot(&self, slot_id: u32, buffer: &mut Vec<u8>) {
        let mut bound = {
            let slots = self.slots.read_clean();
            let Some(slot_mutex) = slots.get(slot_id as usize) else {
                return;
            };
            let slot = slot_mutex.lock_clean();
            if !slot.connected {
                return;
            }
            slot.server.client_drain_snapshot()
        };
        while self.receive_from_slot(slot_id, buffer, Some(&mut bound)) {}
    }

    /// `drain_slot` ушедшего клиента с изоляцией паники колбэка: паника
    /// `on_message` -- `on_error(Some(slot), WorkerPanicked)` (его паника
    /// отбрасывается), остаток дочитывания не доставляется, а отключение
    /// идёт дальше обычным путём ухода -- с исходной причиной (`Graceful`,
    /// `PeerDied`, причина осиротевшего слота) и обычным снятием claim-а.
    fn drain_departed(&self, slot_id: u32, buffer: &mut Vec<u8>) {
        let drained = catch_unwind(AssertUnwindSafe(|| self.drain_slot(slot_id, buffer)));
        if let Err(payload) = drained {
            report(self, Some(slot_id), panic_error(&*payload));
        }
    }

    /// `S2C_DISCONNECT` подключённого слота: клиент ушёл штатно. Всё, что он
    /// успел записать (прощальное сообщение), доставляется ДО колбэка.
    fn handle_slot_disconnect(&self, slot_id: u32, buffer: &mut Vec<u8>) {
        {
            let slots = self.slots.read_clean();
            let Some(slot_mutex) = slots.get(slot_id as usize) else {
                return;
            };
            let slot = slot_mutex.lock_clean();
            if !slot.connected {
                // Слот уже отключён локально (`disconnect_client`), а привязка
                // диспетчера ещё стояла и поглотила сигнал, адресованный
                // КЛИЕНТУ (событие одно на обе стороны). Возвращаем его.
                if let Some(events) = slot.server.events() {
                    let _ = events.disconnect.set();
                }
                return;
            }
            if !slot.server.client_left() {
                // T11c (решение AD): клиент в сессии (`client_state ==
                // SERVER_READY`) -- сигнал устаревший (окно между CAS ухода
                // прежнего клиента и его сигналом, пир 0.9, посторонний
                // процесс). Поглощён: возвращать некому -- сервер сигналит,
                // только закрыв слот (`connected == false` под этой же
                // блокировкой, ветка выше), а клиент -- только уходя
                // (состояние до сигнала, R14). Привязку `DISCONNECT`
                // вызывающий `reconcile` поставит снова; работа на сигнал
                // -- O(1), остальные объекты слота -- свои пакеты порта.
                return;
            }
        }

        self.drain_departed(slot_id, buffer);

        let was_connected = {
            let slots = self.slots.read_clean();
            let Some(slot_mutex) = slots.get(slot_id as usize) else {
                return;
            };
            let mut slot = slot_mutex.lock_clean();
            let was = slot.connected;
            if was {
                let session_claim = slot.session_claim;
                slot.reset_connection(false);
                // Ревизия 2: CAS claim сессии -> FREE, а не store. Клиент 0.9
                // снимает claim сам сразу после `DISCONNECT` (R13), и новый
                // клиент мог уже захватить слот, пока мы дочитывали кольцо --
                // безусловный store отнял бы у него захват. Клиент старой
                // версии, не снявший claim, освобождается этим CAS.
                let _ = slot
                    .server
                    .view()
                    .control_block()
                    .slot_claim
                    .compare_exchange(
                        session_claim,
                        CLAIM_FREE,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
            }
            was
        };

        if was_connected {
            self.handler
                .on_client_disconnect_reason(slot_id, DisconnectReason::Graceful);
        }
    }

    /// Процесс клиента завершился (handle сигнален): дочитать кольцо, затем
    /// отключить слот с `PeerDied`. Claim снимается, только если его держит
    /// именно умерший процесс (PID владельца совпадает) -- новый захват не
    /// затирается.
    fn handle_slot_peer_died(&self, slot_id: u32, buffer: &mut Vec<u8>) {
        self.drain_departed(slot_id, buffer);

        let was_connected = {
            let slots = self.slots.read_clean();
            let Some(slot_mutex) = slots.get(slot_id as usize) else {
                return;
            };
            let mut slot = slot_mutex.lock_clean();
            if !slot.connected {
                // Task 7: умер «задержавшийся владелец» -- клиент сессии,
                // которую сервер уже закрыл (`force_disconnect`, отказ S7):
                // claim всё ещё его -- снять сразу, одним CAS токена сессии
                // (`release_if_holder_dead`), без `reserve_timeout`.
                // Колбэка нет: отключение о сессии уже сообщено.
                if let Some((watch, token)) = slot.lingering.as_ref()
                    && watch.has_exited()
                {
                    let _ =
                        release_if_holder_dead(slot.server.view().control_block(), watch, *token);
                    slot.retire_lingering();
                }
                return;
            }
            let control = slot.server.view().control_block();
            let claim = control.slot_claim.load(Ordering::Acquire);
            let owner = control.slot_owner_pid.load(Ordering::Acquire);
            if claim != CLAIM_FREE && slot.watched_pid() == Some(owner) {
                let _ = control.slot_claim.compare_exchange(
                    claim,
                    CLAIM_FREE,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
            // Handle процесса обязан уйти из привязок в любом случае:
            // он сигнален навсегда и иначе будил бы диспетчер без конца.
            slot.reset_connection(false);
            true
        };

        if was_connected {
            self.handler
                .on_client_disconnect_reason(slot_id, DisconnectReason::PeerDied);
        }
    }

    /// Отключение осиротевшего слота, обнаруженного `reclaim_stale_claims`:
    /// либо брошенное рукопожатие (claim был `CLAIM_FREE`), либо
    /// подтверждённо мёртвый процесс-владелец (claim — его последний
    /// известный token). В обоих случаях `expected_claim` — это ЗНАЧЕНИЕ
    /// claim, увиденное в момент детекции.
    ///
    /// Между детекцией и этим вызовом могло пройти время: если claim успел
    /// измениться (новый клиент захватил освободившийся слот), безусловная
    /// мутация затёрла бы легитимное состояние (аудит 2026-07-10, находка
    /// "unconditional store race"). Поэтому `claim == expected_claim`
    /// проверяется и одновременно фиксируется ОДНИМ атомарным
    /// `compare_exchange(expected_claim, CLAIM_FREE)`: если он проваливается
    /// — слот уже не тот, что мы считали осиротевшим, пропускаем без единой
    /// мутации остального состояния.
    fn handle_orphaned_slot_disconnect(
        &self,
        slot_id: u32,
        expected_claim: u32,
        reason: DisconnectReason,
    ) {
        let was_connected = {
            let slots = self.slots.read_clean();
            let Some(slot_mutex) = slots.get(slot_id as usize) else {
                return;
            };
            let mut slot = slot_mutex.lock_clean();

            if !slot.connected {
                return; // уже обработан (например явным disconnect-событием)
            }

            let claim_field = &slot.server.view().control_block().slot_claim;
            if claim_field
                .compare_exchange(
                    expected_claim,
                    CLAIM_FREE,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                return; // claim изменился — состояние слота уже не то, что при детекции
            }

            // claim атомарно переведён в FREE выше (или уже был FREE и остался
            // им) -- освобождать его повторно не нужно.
            slot.reset_connection(false);
            true
        };

        if was_connected {
            self.handler.on_client_disconnect_reason(slot_id, reason);
        }
    }

    /// Получение сообщений от слота (не больше `recv_batch` за вызов).
    /// `true` -- пачка выбрана целиком, в кольце могут остаться сообщения.
    /// `bound` -- дочитывание ушедшего клиента (`drain_slot`): только кадры
    /// до снимка, граница достигнута -- `false`.
    fn receive_from_slot(
        &self,
        slot_id: u32,
        buffer: &mut Vec<u8>,
        mut bound: Option<&mut DrainBound>,
    ) -> bool {
        // Собираем все сообщения под lock-ом
        let mut messages: Vec<Vec<u8>> = Vec::new();
        let mut error: Option<ShmError> = None;
        let batch = self.options.recv_batch.max(1);
        let mut reached = false;

        {
            let slots = self.slots.read_clean();
            let Some(slot_mutex) = slots.get(slot_id as usize) else {
                return false;
            };
            let slot = slot_mutex.lock_clean();
            if !slot.connected {
                return false;
            }
            for _ in 0..batch {
                let received = match bound.as_deref_mut() {
                    // Дочитывание: только до снимка, своей позицией чтения;
                    // сдвиг `read_pos` пиром или испорченный кадр --
                    // `Corrupted` (ниже: `on_error`, дочитывание кончается,
                    // разрыв -- за путём ухода, решение C).
                    Some(bound) => match slot.server.drain_from_client(buffer, bound) {
                        Ok(Some(len)) => Ok(len),
                        Ok(None) => {
                            reached = true;
                            break;
                        }
                        Err(err) => Err(err),
                    },
                    None => slot.server.receive_from_client(buffer),
                };
                match received {
                    Ok(len) => messages.push(buffer[..len].to_vec()),
                    Err(ShmError::QueueEmpty) => break,
                    Err(err) => {
                        error = Some(err);
                        break;
                    }
                }
            }
        }

        let more = !reached && error.is_none() && messages.len() == batch;

        // Отдаём handler-у без lock-а. `stop()` из колбэка: остаток пачки
        // не доставляем -- после `stop()` колбэков нет.
        for data in &messages {
            if !self.running.load(Ordering::Acquire) {
                return false;
            }
            self.handler.on_message(slot_id, data);
        }

        if let Some(err) = error {
            let corrupted = err == ShmError::Corrupted;
            // Паника `on_error` отбрасывается (rustdoc `MultiHandler`): ни
            // второго `on_error`, ни подмены причины разрыва на `Local`.
            report(self, Some(slot_id), err);
            if corrupted && bound.is_none() {
                // Клиент испортил кольцо: `read_pos` не сдвинется, и без
                // разрыва слот навсегда остался бы «подключённым», а каждое
                // `DATA` давало бы новый `on_error`.
                //
                // Дочитывание ушедшего (`bound`) -- без разрыва (решение C,
                // Task 7): уход продолжает свой путь со своей причиной
                // (`PeerDied`/`Graceful`/причина осиротевшего слота) и своим
                // снятием claim-а, порча -- отдельный `on_error` выше. Разрыв
                // с `Error` здесь подменял бы причину и оставлял claim
                // умершего до `reserve_timeout`.
                self.force_disconnect(slot_id, DisconnectReason::Error);
                return false;
            }
        }
        more
    }
}

impl std::fmt::Debug for MultiCore {
    /// Ручная реализация: `handler` -- `Arc<dyn MultiHandler>`, у трейта нет
    /// `Debug`. Печатаем то, что реально полезно в логах: имя и заполненность.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MultiServer")
            .field("base_name", &self.base_name)
            .field("max_clients", &self.max_clients)
            .field("connected", &self.client_count())
            .field("running", &self.running.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

// ============================================================================
// Поток-диспетчер MultiServer (0.10): все слоты на одном порту завершения
// ============================================================================

/// Вид ожидания слота -- часть ключа пакета.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WaitKind {
    /// `C2S_CONNECT_REQ` свободного слота.
    Connect,
    /// `S2C_DISCONNECT` подключённого слота (событие одно на обе стороны).
    Disconnect,
    /// `C2S_DATA` подключённого слота.
    Data,
    /// Процесс клиента (объект уведомления: сигнал не потребляется).
    Peer,
}

impl WaitKind {
    const ALL: [Self; WAIT_KINDS] = [Self::Connect, Self::Disconnect, Self::Data, Self::Peer];

    /// Индекс вида в ключе и в `SlotWaits::arms`.
    const fn index(self) -> usize {
        match self {
            Self::Connect => 0,
            Self::Disconnect => 1,
            Self::Data => 2,
            Self::Peer => 3,
        }
    }

    /// Ключ пакета: `slot * WAIT_KINDS + вид` (для 256 слотов < 1024, на x86
    /// тоже без переполнения).
    const fn key(self, slot_id: u32) -> usize {
        slot_id as usize * WAIT_KINDS + self.index()
    }

    /// Ключ -> (слот, вид); `None` -- не ключ слота.
    fn decode(key: usize) -> Option<(u32, Self)> {
        let slot_id = u32::try_from(key / WAIT_KINDS).ok()?;
        let kind = *Self::ALL.get(key % WAIT_KINDS)?;
        Some((slot_id, kind))
    }
}

/// Одна привязка (вид ожидания слота) диспетчера.
///
/// Метка пакета (`ApcContext`) -- поколение привязки: растёт при каждой
/// привязке, поэтому завершение, извлечённое из порта после отмены или
/// перепривязки, по метке не совпадает и отбрасывается (`take`).
#[derive(Debug, Default)]
struct Arm {
    /// Пакет создаётся при первой привязке и затем переиспользуется.
    packet: Option<WaitPacket>,
    /// Поколение последней привязки -- метка её пакета.
    generation: usize,
    /// Пакет привязан, и его завершение ещё не обработано.
    armed: bool,
    /// Объект привязки (адрес handle): у процесса клиента он меняется между
    /// сессиями -- другой объект перепривязывается.
    target: usize,
    /// Последняя попытка привязки не удалась -- повтор через
    /// `REARM_BACKOFF` (`Dispatcher::rearm_at`); ошибка сообщается один раз
    /// на серию отказов.
    failed: bool,
}

impl Arm {
    /// Привязать пакет к `target` с ключом `key` и новым поколением.
    ///
    /// Отказ привязки, в котором не доказана вина объекта
    /// (`WaitPacket::survives_arm_failure`), закрывает пакет: повтор через
    /// `REARM_BACKOFF` на том же негодном пакете отказывал бы бесконечно.
    /// Если пакет был прежний -- сразу один повтор с новым.
    fn arm(&mut self, port: &CompletionPort, target: BorrowedHandle<'_>, key: usize) -> Result<()> {
        let raw = target.as_raw_handle();
        let generation = self.generation.wrapping_add(1);
        loop {
            let reused = self.packet.is_some();
            let packet = match &mut self.packet {
                Some(packet) => packet,
                slot @ None => slot.insert(WaitPacket::new()?),
            };
            let status = port.arm_status(packet, raw, key, generation);
            if status == STATUS_SUCCESS {
                self.generation = generation;
                self.armed = true;
                self.target = raw.addr();
                return Ok(());
            }
            if !packet.survives_arm_failure(status) {
                // Пакет негоден или его вина не исключена -- закрыть;
                // прежний -- ровно один повтор с новым (на новом пакете
                // `reused == false`, второй отказ возвращается).
                self.packet = None;
                if reused {
                    continue;
                }
            }
            return Err(status_to_error(status, "NtAssociateWaitCompletionPacket"));
        }
    }

    /// Снять привязку: `true` -- сигнал объекта потреблён и до обработчика
    /// не дойдёт (пакет вынут из очереди или уже извлечён, но ещё не
    /// обработан -- его завершение будет отброшено по поколению).
    fn cancel(&mut self) -> bool {
        if !self.armed {
            return false;
        }
        self.armed = false;
        let Some(packet) = &self.packet else {
            return false;
        };
        let (outcome, clean) = packet.cancel_for_reuse();
        if !clean {
            // Неожиданный код отмены (невозможен при живом дескрипторе):
            // состояние пакета неизвестно, перепривязка могла бы отказывать
            // бесконечно (`REARM_BACKOFF` без конца). Пакет закрывается,
            // следующая привязка создаст новый (правило пула реактора).
            self.packet = None;
        }
        outcome != CancelOutcome::NotFired
    }

    /// Принять завершение с меткой `tag`: `true` -- это текущая привязка
    /// (она снята -- ядро потребило сигнал), `false` -- устаревшее.
    const fn take(&mut self, tag: usize) -> bool {
        if self.armed && self.generation == tag {
            self.armed = false;
            true
        } else {
            false
        }
    }
}

/// Привязки одного слота и его место в очереди чтения.
#[derive(Debug, Default)]
struct SlotWaits {
    /// По видам: индекс -- `WaitKind::index`.
    arms: [Arm; WAIT_KINDS],
    /// Слот стоит в очереди чтения `Dispatcher::ready`.
    queued: bool,
}

/// Поток-диспетчер: привязки всех слотов, очередь чтения (честная
/// ротация) и буферы. Живёт только на своём потоке; ядро -- по ссылке
/// (поток держит `Arc<MultiCore>`, а не ручку `MultiServer`).
struct Dispatcher<'a> {
    core: &'a MultiCore,
    /// Привязки по слотам (индекс -- номер слота).
    waits: Vec<SlotWaits>,
    /// Слоты, в кольцах которых могут быть данные: обходятся по кругу, не
    /// больше `recv_batch` сообщений слота за круг.
    ready: VecDeque<u32>,
    /// Пачка завершений из порта.
    completions: Vec<Completion>,
    /// Буфер чтения сообщений.
    buffer: Vec<u8>,
    /// Когда повторить неудавшиеся привязки (`Arm::failed`); входит в
    /// таймаут ожидания порта, чтобы повтор не ждал постороннего события.
    rearm_at: Option<Instant>,
    /// Ближайший дедлайн слотов (отъём протухшего захвата, проверка
    /// брошенного рукопожатия): на нём -- полный проход
    /// `reclaim_stale_claims` (Task 10, решение R). Назначает его полный
    /// проход (`Sweep::next_deadline`) и обработка событий слота
    /// (`schedule`); входит в таймаут ожидания порта.
    sweep_due: Option<Instant>,
    /// Момент последнего полного прохода: не реже раза в
    /// `LIVENESS_CHECK_INTERVAL` полный проход делается и без дедлайна --
    /// оппортунистически, только на пробуждении (не таймер), ради проверки
    /// живости клиентов без наблюдаемого процесса и ловли осиротевших
    /// слотов раньше их дедлайна.
    last_sweep: Instant,
}

impl<'a> Dispatcher<'a> {
    fn new(core: &'a MultiCore) -> Self {
        Self {
            core,
            waits: (0..core.max_clients)
                .map(|_| SlotWaits::default())
                .collect(),
            ready: VecDeque::new(),
            completions: Vec::new(),
            buffer: Vec::new(),
            rearm_at: None,
            // Первый виток -- полный проход (слоты могли быть захвачены до
            // старта диспетчера).
            sweep_due: Some(Instant::now()),
            last_sweep: Instant::now(),
        }
    }

    /// Включить дедлайн `at` в ближайший полный проход.
    fn schedule(&mut self, at: Instant) {
        self.sweep_due = Some(self.sweep_due.map_or(at, |due| due.min(at)));
    }

    /// Цикл диспетчера: проход дедлайнов, спин, ожидание порта до события
    /// или ближайшего дедлайна, обработка завершений, круг чтения.
    fn run(&mut self) {
        let core = self.core;
        for slot_id in 0..core.max_clients {
            self.reconcile(slot_id);
        }
        let mut probes: Vec<(u32, UnreadProbe<'a>)> = Vec::new();

        while core.running.load(Ordering::Acquire) {
            // Потерянный пост `wake_slot` (M5): сверить все слоты -- O(N),
            // только после отказа ядра, не на каждом витке.
            if core.wake_lost.swap(false, Ordering::AcqRel) {
                for slot_id in 0..core.max_clients {
                    self.reconcile(slot_id);
                }
            }
            let now = Instant::now();
            // Task 10 (П3, решение R): полный проход всех слотов -- O(N)
            // блокировок -- только на дедлайне или не чаще
            // `LIVENESS_CHECK_INTERVAL` (на пробуждении по другой причине),
            // а не на каждом витке: пустой `CONNECT_REQ` одного слота стоит
            // O(1) (`handle_slot_connect` + `reconcile` этого слота).
            let full = self.sweep_due.is_some_and(|due| now >= due)
                || now.saturating_duration_since(self.last_sweep) >= LIVENESS_CHECK_INTERVAL;
            let sweep = if full {
                self.last_sweep = now;
                let sweep = core.reclaim_stale_claims(0..core.max_clients, now);
                self.sweep_due = sweep.next_deadline;
                sweep
            } else {
                Sweep::default()
            };
            for (slot_id, expected_claim, reason) in sweep.orphaned {
                // Клиент мог уйти штатно (DISCONNECT, затем снял claim), а
                // диспетчер разобрал проход раньше его DISCONNECT: всё, что
                // клиент успел записать до обнаружения, -- до колбэка.
                self.isolate(Some(slot_id), |d| {
                    d.core.drain_departed(slot_id, &mut d.buffer);
                    d.core
                        .handle_orphaned_slot_disconnect(slot_id, expected_claim, reason);
                });
                self.reconcile(slot_id);
            }
            for slot_id in sweep.relinquished {
                // «Задержавшийся владелец» снят -- снять привязку процесса.
                self.reconcile(slot_id);
            }
            if self.rearm_at.is_some_and(|at| now >= at) {
                self.rearm_at = None;
                self.retry_failed_arms();
            }

            // Спин (`MultiOptions::spin`) -- один бюджет на каждое засыпание:
            // только когда очередь чтения пуста (иначе сна и так не будет) и
            // есть подключённые слоты. Увидели данные -- читаем их кругом, а
            // ожидание порта всё равно идёт, с нулевым таймаутом: завершения
            // `CONNECT_REQ`/`DISCONNECT`/смерти процесса других слотов
            // разбираются до следующего спина (болтливый слот не заслоняет
            // их -- Task 12). Порт отдаёт всю очередь пачкой, приоритета
            // «меньшего индекса», как у `NtWaitForMultipleObjects`, нет.
            let mut spun = false;
            if self.ready.is_empty() && core.options.spin.is_enabled() {
                core.collect_probes(&mut probes);
                spun = !probes.is_empty()
                    && spin_until(core.options.spin, None, || {
                        probes.iter().any(|(_, probe)| probe.has_unread())
                            || !core.running.load(Ordering::Acquire)
                    });
                if spun {
                    for &(slot_id, ref probe) in &probes {
                        if probe.has_unread() {
                            self.enqueue(slot_id);
                        }
                    }
                    self.round();
                }
                probes.clear();
            }
            if !core.running.load(Ordering::Acquire) {
                break;
            }

            let timeout = if spun || !self.ready.is_empty() {
                Some(Duration::ZERO)
            } else {
                let nearest = match (self.sweep_due, self.rearm_at) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                let until_deadline = nearest.map(|at| at.saturating_duration_since(Instant::now()));
                match (core.options.poll_timeout, until_deadline) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                }
            };

            let result = core.port.wait_batch(&mut self.completions, timeout);
            // Одно пробуждение на каждый возврат из ожидания порта.
            core.wakeups.fetch_add(1, Ordering::AcqRel);
            match result {
                Ok(_) => {
                    let completions = std::mem::take(&mut self.completions);
                    for &completion in &completions {
                        // `stop()` из колбэка: дальше колбэков нет.
                        if !core.running.load(Ordering::Acquire) {
                            break;
                        }
                        self.complete(completion);
                    }
                    self.completions = completions;
                }
                Err(err) => report(core, None, err),
            }
            // Таймаут (дедлайн) разберёт полный проход в начале витка.
            if !self.ready.is_empty() {
                self.round();
            }
        }
        self.release_all();
    }

    /// Поставить слот в очередь чтения (если его там нет).
    fn enqueue(&mut self, slot_id: u32) {
        if let Some(waits) = self.waits.get_mut(slot_id as usize)
            && !waits.queued
        {
            waits.queued = true;
            self.ready.push_back(slot_id);
        }
    }

    /// Один круг чтения: каждый слот, стоявший в очереди к началу круга, --
    /// пачка до `recv_batch`; выбрал пачку целиком -- в конец очереди
    /// (честная ротация), нет -- кольцо пусто по позициям (`read_message`
    /// подтвердил пустоту `confirm_empty`), и слот уходит из очереди: его
    /// `DATA` привязан (перепривязка идёт до чтения), следующий кадр
    /// разбудит порт.
    fn round(&mut self) {
        let core = self.core;
        for _ in 0..self.ready.len() {
            if !core.running.load(Ordering::Acquire) {
                return;
            }
            let Some(slot_id) = self.ready.pop_front() else {
                return;
            };
            let mut more = false;
            self.isolate(Some(slot_id), |d| {
                more = d.core.receive_from_slot(slot_id, &mut d.buffer, None);
            });
            if more {
                self.ready.push_back(slot_id);
            } else if let Some(waits) = self.waits.get_mut(slot_id as usize) {
                waits.queued = false;
            }
        }
    }

    /// Завершение из порта: пробуждение (`WAKE_KEY`) или сработавшая
    /// привязка слота. Устаревшее (поколение не совпало) отбрасывается.
    fn complete(&mut self, completion: Completion) {
        if completion.key == WAKE_KEY {
            // `WAKE_ANY` -- остановка: её разберёт проверка `running`.
            if completion.tag != WAKE_ANY
                && let Ok(slot_id) = u32::try_from(completion.tag)
            {
                self.reconcile(slot_id);
            }
            return;
        }
        let current = WaitKind::decode(completion.key).and_then(|(slot_id, kind)| {
            let arm = self
                .waits
                .get_mut(slot_id as usize)?
                .arms
                .get_mut(kind.index())?;
            arm.take(completion.tag).then_some((slot_id, kind))
        });
        let Some((slot_id, kind)) = current else {
            // Relaxed: тестовая статистика, ничего не публикует.
            #[cfg(test)]
            self.core.stale_completions.fetch_add(1, Ordering::Relaxed);
            return;
        };
        self.isolate(Some(slot_id), |d| match kind {
            WaitKind::Connect => {
                if let Some(at) = d.core.handle_slot_connect(slot_id) {
                    d.schedule(at);
                }
            }
            WaitKind::Disconnect => d.core.handle_slot_disconnect(slot_id, &mut d.buffer),
            WaitKind::Peer => d.core.handle_slot_peer_died(slot_id, &mut d.buffer),
            // Перепривязка -- в `reconcile` ниже, ДО чтения кольца.
            WaitKind::Data => {}
        });
        self.reconcile(slot_id);
    }

    /// Выполнить работу диспетчера, зовущую колбэки слота `slot`, изолируя
    /// панику обработчика (решение 26): паника -- `on_error(slot,
    /// WorkerPanicked)` (его собственная паника отбрасывается), слот
    /// отключается обычным локальным путём (`force_disconnect(Local)`:
    /// ревизия 2 -- claim не снимается, клиент получает `DISCONNECT`; паника
    /// колбэка отключения тоже отбрасывается, без рекурсии), остальные слоты
    /// обслуживаются дальше. Работает при `panic = "unwind"` у итогового
    /// бинарника; при `abort` паника завершает процесс, как везде.
    fn isolate(&mut self, slot: Option<u32>, work: impl FnOnce(&mut Self)) {
        let Err(payload) = catch_unwind(AssertUnwindSafe(|| work(self))) else {
            return;
        };
        let core = self.core;
        report(core, slot, panic_error(&*payload));
        drop(payload);
        if let Some(slot_id) = slot {
            let disconnected = catch_unwind(AssertUnwindSafe(|| {
                core.force_disconnect(slot_id, DisconnectReason::Local);
            }));
            // Паника колбэка отключения: сообщать некуда (это был бы второй
            // круг той же ошибки), её текст уже напечатал panic hook.
            drop(disconnected);
            self.reconcile(slot_id);
        }
    }

    /// Повторить неудавшиеся привязки (`Arm::failed`) всех слотов.
    fn retry_failed_arms(&mut self) {
        let failed: Vec<u32> = (0..self.core.max_clients)
            .filter(|&slot_id| {
                self.waits
                    .get(slot_id as usize)
                    .is_some_and(|waits| waits.arms.iter().any(|arm| arm.failed))
            })
            .collect();
        for slot_id in failed {
            self.reconcile(slot_id);
        }
    }

    /// Свести привязки слота с его состоянием: свободный -- ждёт
    /// `CONNECT_REQ`; подключённый -- `DISCONNECT`, `DATA` и процесс
    /// клиента (handshake-PID, иначе владелец claim-а). Лишние привязки
    /// снимаются, недостающие ставятся.
    ///
    /// Потерянных сигналов нет: снятая привязка, потребившая сигнал
    /// автосбросного события, возвращает его (`CONNECT_REQ`, `DISCONNECT`
    /// -- `NtSetEvent`; `DISCONNECT` адресован и клиенту) или перечитывает
    /// кольцо (`DATA`); поставленная привязка `DATA` всегда сопровождается
    /// чтением кольца (перепривязка -- до чтения). Handle и события
    /// заимствуются под блокировкой слота, привязка -- тоже под ней
    /// (handle процесса закрывается только под ней же); колбэк ошибки --
    /// после блокировок.
    fn reconcile(&mut self, slot_id: u32) {
        let core = self.core;
        let mut failure = None;
        let mut any_failed = false;
        let mut recheck = false;
        let mut claim_due = None;
        {
            let slots = core.slots.read_clean();
            let (Some(slot_mutex), Some(waits)) = (
                slots.get(slot_id as usize),
                self.waits.get_mut(slot_id as usize),
            ) else {
                return;
            };
            let mut slot = slot_mutex.lock_clean();
            #[cfg(test)]
            core.slot_visits.fetch_add(1, Ordering::Relaxed);
            // Слоты всегда named (`start_slot`), но на anonymous-слоте событий
            // нет -- молча пропускаем вместо паники в диспетчере.
            let Some(events) = slot.server.events() else {
                return;
            };
            for kind in WaitKind::ALL {
                let desired = match kind {
                    WaitKind::Connect => (!slot.connected).then(|| events.connect_req.as_handle()),
                    WaitKind::Disconnect => slot.connected.then(|| events.disconnect.as_handle()),
                    WaitKind::Data => slot.connected.then(|| events.c2s.data.as_handle()),
                    // Не подключён -- «задержавшийся владелец» (Task 7),
                    // если есть: процесс сигналит один раз, в простое
                    // пробуждений нет.
                    WaitKind::Peer => {
                        if slot.connected {
                            slot.peer_handle()
                        } else {
                            slot.lingering.as_ref().map(|(watch, _)| watch.as_handle())
                        }
                    }
                };
                let Some(arm) = waits.arms.get_mut(kind.index()) else {
                    continue;
                };
                if desired.is_none() {
                    // Привязка больше не нужна: серия отказов закончилась,
                    // следующий отказ -- новая серия (снова сообщается).
                    arm.failed = false;
                }
                let target = desired.map(|handle| handle.as_raw_handle().addr());
                if arm.armed && target != Some(arm.target) && arm.cancel() {
                    // Сигнал потреблён снятой привязкой -- вернуть.
                    match kind {
                        WaitKind::Connect => {
                            let _ = events.connect_req.set();
                        }
                        WaitKind::Disconnect => {
                            let _ = events.disconnect.set();
                        }
                        WaitKind::Data => recheck = true,
                        WaitKind::Peer => {}
                    }
                }
                if !arm.armed
                    && let Some(handle) = desired
                {
                    match arm_checked(core, arm, handle, kind.key(slot_id)) {
                        Ok(()) => {
                            arm.failed = false;
                            recheck |= kind == WaitKind::Data;
                        }
                        Err(err) => {
                            // Сообщить -- первый отказ серии; повторять --
                            // каждый раз через `REARM_BACKOFF`.
                            if !arm.failed {
                                failure = Some(err);
                            }
                            arm.failed = true;
                            any_failed = true;
                        }
                    }
                }
            }
            // Привязки слота сведены -- handle отключённых клиентов можно
            // закрыть (привязку к процессу ядро держит по ссылке на объект,
            // а снятая выше уже не ссылается на него).
            slot.retired.clear();
            // Task 10 (П3): захват свободного слота без заявки (толчок,
            // клиент, отключённый сервером и не ушедший) -- первое
            // наблюдение и дедлайн здесь, в сверке ЭТОГО слота, а не обходом
            // всех слотов на каждом витке.
            if !slot.connected {
                claim_due = core.observe_claim(&mut slot, Instant::now());
            }
        }
        if let Some((_, due)) = claim_due {
            self.schedule(due);
        }
        if recheck {
            self.enqueue(slot_id);
        }
        if any_failed {
            // Без привязки слот не услышит своё событие (свободный слот стал
            // бы «чёрной дырой»: клиенты захватывают его и ждут до
            // таймаута) -- повтор через короткую ограниченную паузу. Отказ
            // (`STATUS_INSUFFICIENT_RESOURCES`, негодный handle) редок, а
            // 100 мс не превращают стойкий отказ в горячий цикл; момент
            // повтора входит в таймаут ожидания порта -- в простое он не
            // ждёт постороннего события.
            let at = Instant::now() + REARM_BACKOFF;
            self.rearm_at = Some(self.rearm_at.map_or(at, |old| old.min(at)));
        }
        if let Some(err) = failure {
            report(core, Some(slot_id), err);
        }
    }

    /// Выход диспетчера: снять все привязки, вернув потреблённые сигналы
    /// `CONNECT_REQ`/`DISCONNECT`. Важно для `stop()` из колбэка (и Drop
    /// последней ручки на этом потоке): `stop` сигналит `DISCONNECT`
    /// клиентам, пока привязки ещё стоят, -- поглощённый сигнал вернётся.
    fn release_all(&mut self) {
        let slots = self.core.slots.read_clean();
        for (slot_mutex, waits) in slots.iter().zip(self.waits.iter_mut()) {
            let slot = slot_mutex.lock_clean();
            let events = slot.server.events();
            for kind in WaitKind::ALL {
                let Some(arm) = waits.arms.get_mut(kind.index()) else {
                    continue;
                };
                if arm.cancel()
                    && let Some(events) = events
                {
                    match kind {
                        WaitKind::Connect => {
                            let _ = events.connect_req.set();
                        }
                        WaitKind::Disconnect => {
                            let _ = events.disconnect.set();
                        }
                        // Читателя больше нет; процесс -- объект уведомления.
                        WaitKind::Data | WaitKind::Peer => {}
                    }
                }
            }
        }
    }
}

/// Сообщить об ошибке `on_error`, изолируя панику самого обработчика
/// ошибок: она отбрасывается (сообщать о ней некуда, без рекурсии; текст
/// напечатал panic hook).
fn report(core: &MultiCore, slot: Option<u32>, err: ShmError) {
    let reported = catch_unwind(AssertUnwindSafe(|| core.handler.on_error(slot, err)));
    drop(reported);
}

/// Привязка с тестовой инъекцией отказа (`MultiCore::fail_arms`).
fn arm_checked(
    core: &MultiCore,
    arm: &mut Arm,
    target: BorrowedHandle<'_>,
    key: usize,
) -> Result<()> {
    #[cfg(test)]
    if core
        .fail_arms
        .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
        .is_ok()
    {
        return Err(ShmError::InvalidConfig("тестовый отказ привязки"));
    }
    arm.arm(&core.port, target, key)
}

impl std::fmt::Debug for Dispatcher<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dispatcher")
            .field("slots", &self.waits.len())
            .field("ready", &self.ready.len())
            .finish_non_exhaustive()
    }
}

// ============================================================================
// MultiClient — клиент с автоматическим назначением слота
// ============================================================================

/// Очередь отправки `MultiClient`, общая для API-потоков и worker-а.
///
/// `send` сам соблюдает `max_send_queue` (вытесняя старейшее), поэтому
/// память ограничена и пока клиент не подключён (раньше mpsc-канал рос без
/// предела, а лимит применялся только при подключении). `on_overflow`
/// зовёт worker -- раз на пачку вытесненных.
#[derive(Debug, Default)]
struct ClientOutbox {
    queue: Mutex<VecDeque<Vec<u8>>>,
    evicted: AtomicU32,
    /// Лимит размера сообщения кольца клиент -> сервер (`0` -- ещё не
    /// подключались): длина проверяется в `send` синхронно.
    max_message: AtomicUsize,
    /// Счётчик принятых `send` (0.10): спин worker-а прерывается, увидев
    /// его изменение, не беря lock очереди на каждой итерации.
    sends: AtomicU32,
}

/// Мультиклиент — подключается к базовому имени, получает слот автоматически
#[derive(Debug)]
pub struct MultiClient {
    outbox: Arc<ClientOutbox>,
    max_send_queue: usize,
    /// Будит worker: новая команда (`send`/`stop`/Drop). Безымянное,
    /// автосброс.
    wake: Arc<EventHandle>,
    join: Mutex<Option<JoinHandle<()>>>,
    running: Arc<AtomicBool>,
    slot_id: Arc<AtomicU32>,
    layout: Arc<Mutex<Option<ChannelLayout>>>,
    /// Число пробуждений подключённого worker-а (тесты: ноль в простое).
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "читается только тестами (доказательство отсутствия опроса)"
        )
    )]
    wakeups: Arc<AtomicU64>,
}

impl MultiClient {
    /// Подключение к мультисерверу
    ///
    /// Клиент автоматически:
    /// 1. Пробегает слоты base_name_0.. и атомарно захватывает свободный (CAS)
    /// 2. Выполняет обычный handshake с захваченным слотом
    /// 3. При потере связи — повторяет захват свободного слота (через
    ///    `retry_delay`)
    pub fn connect(
        base_name: &str,
        handler: Arc<dyn MultiClientHandler>,
        options: MultiClientOptions,
    ) -> Result<Self> {
        presence::validate_fallback(options.discovery_fallback)?;
        let outbox = Arc::new(ClientOutbox::default());
        let wake = Arc::new(EventHandle::create_unnamed(false)?);
        let running = Arc::new(AtomicBool::new(true));
        let slot_id = Arc::new(AtomicU32::new(SLOT_ID_NO_SLOT));
        let layout = Arc::new(Mutex::new(None));
        let wakeups = Arc::new(AtomicU64::new(0));
        let max_send_queue = options.max_send_queue.max(1);

        let shared = ClientShared {
            outbox: outbox.clone(),
            wake: wake.clone(),
            running: running.clone(),
            slot_id: slot_id.clone(),
            layout: layout.clone(),
            wakeups: wakeups.clone(),
        };
        let name = base_name.to_owned();

        #[cfg_attr(
            not(debug_assertions),
            expect(unused_mut, reason = "имя -- только в debug")
        )]
        let mut builder = thread::Builder::new();
        #[cfg(debug_assertions)]
        {
            builder = builder.name(format!("xsmc-{base_name}"));
        }
        let handle = crate::thread_hook::spawn(builder, move || {
            client_worker(&name, &handler, &options, &shared);
        })
        .map_err(|e| map_spawn_error(e, "spawn multi client worker"))?;

        Ok(Self {
            outbox,
            max_send_queue,
            wake,
            join: Mutex::new(Some(handle)),
            running,
            slot_id,
            layout,
            wakeups,
        })
    }

    /// Отправка сообщения серверу (асинхронно, с перезаписью старейшего в
    /// кольце). Переполнение очереди (`max_send_queue`) вытесняет самое
    /// старое неотправленное сообщение (`on_overflow`). Длина проверяется
    /// синхронно: против лимита кольца слота, когда он известен.
    pub fn send(&self, data: &[u8]) -> Result<()> {
        if !self.running.load(Ordering::Acquire) {
            return Err(ShmError::NotReady);
        }
        if data.len() < MIN_MESSAGE_SIZE {
            return Err(ShmError::MessageTooSmall);
        }
        let limit = match self.outbox.max_message.load(Ordering::Acquire) {
            0 => max_payload_for(MAX_RING_CAPACITY),
            limit => limit,
        };
        if data.len() > limit {
            return Err(ShmError::MessageTooLarge);
        }
        let data = data.to_vec();
        {
            let mut queue = self.outbox.queue.lock_clean();
            if queue.len() >= self.max_send_queue {
                queue.pop_front();
                self.outbox.evicted.fetch_add(1, Ordering::AcqRel);
            }
            queue.push_back(data);
        }
        self.outbox.sends.fetch_add(1, Ordering::Release);
        let _ = self.wake.set();
        Ok(())
    }

    /// Получить назначенный slot_id (SLOT_ID_NO_SLOT если не подключён)
    #[must_use]
    pub fn slot_id(&self) -> u32 {
        self.slot_id.load(Ordering::Acquire)
    }

    /// Проверка подключения
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.slot_id.load(Ordering::Acquire) != SLOT_ID_NO_SLOT
    }

    /// Раскладка слота текущего (или последнего) подключения; `None` --
    /// ещё ни разу не подключался.
    #[must_use]
    pub fn layout(&self) -> Option<ChannelLayout> {
        *self.layout.lock_clean()
    }

    /// Остановка клиента синхронно: принятое до вызова дописывается в
    /// кольцо, worker выходит и джойнится -- после возврата колбэков нет.
    /// Из собственного колбэка -- без ожидания себя.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
        let _ = self.wake.set();
        let handle = self.join.lock_clean().take();
        if let Some(handle) = handle {
            // Обработчик (`MultiClientHandler`) есть только у worker-а:
            // сообщить панику некуда, panic hook её уже напечатал;
            // паниковать из `stop()`/`Drop` нельзя.
            let _panicked = join_unless_self(handle);
        }
    }

    /// Число пробуждений подключённого worker-а.
    #[cfg(test)]
    pub(crate) fn wakeups(&self) -> u64 {
        self.wakeups.load(Ordering::Acquire)
    }
}

impl Drop for MultiClient {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Состояние, общее для `MultiClient` и его worker-а.
struct ClientShared {
    outbox: Arc<ClientOutbox>,
    wake: Arc<EventHandle>,
    running: Arc<AtomicBool>,
    slot_id: Arc<AtomicU32>,
    layout: Arc<Mutex<Option<ChannelLayout>>>,
    wakeups: Arc<AtomicU64>,
}

/// Клампит клиентский `slot_timeout` ниже `RESERVE_TIMEOUT` с запасом
/// `RESERVE_SAFETY_MARGIN` — иначе при достаточно большом вызывающим-заданном
/// `slot_timeout` (сверху он не ограничен) сервер мог бы счесть
/// клиента протухшим и отдать слот другому раньше, чем клиент сам отвалится
/// по своему таймауту.
fn clamp_slot_timeout(requested: Duration) -> Duration {
    requested.min(RESERVE_TIMEOUT.saturating_sub(RESERVE_SAFETY_MARGIN))
}

/// Сообщить о сообщениях, вытесненных отправителями из очереди.
fn report_evicted(outbox: &ClientOutbox, handler: &Arc<dyn MultiClientHandler>) {
    let evicted = outbox.evicted.swap(0, Ordering::AcqRel);
    if evicted > 0 {
        handler.on_overflow(evicted);
    }
}

/// Очередь -> кольцо. `send_to_server` пишет с перезаписью старейшего и
/// места не ждёт; сообщение, которое записать нельзя вовсе (длина вне
/// пределов кольца), выбрасывается с `on_error` -- иначе оно навсегда
/// застряло бы в голове очереди. `false` -- кольцо испорчено сервером
/// (`Corrupted`), соединение надо разорвать.
fn flush_queue(
    client: &SharedClient,
    outbox: &ClientOutbox,
    handler: &Arc<dyn MultiClientHandler>,
) -> bool {
    report_evicted(outbox, handler);
    loop {
        let next = outbox.queue.lock_clean().pop_front();
        let Some(data) = next else {
            return true;
        };
        if let Err(err) = client.send_to_server(&data) {
            if err == ShmError::NotConnected {
                // Сервер ушёл или сессия уже чужая (Task 6 надёжности, как
                // клиент Auto): не ошибка канала, а конец сессии, о котором
                // решит worker (`DISCONNECT`/смерть сервера/сверка
                // `generation`). Сообщение -- обратно в голову очереди
                // (уйдёт в следующую сессию с сервером), без `on_error` и
                // без попыток остальных: иначе каждое сообщение очереди
                // давало бы свою ошибку и терялось.
                outbox.queue.lock_clean().push_front(data);
                return true;
            }
            let corrupted = err == ShmError::Corrupted;
            handler.on_error(err);
            if corrupted {
                return false;
            }
        }
    }
}

/// Прочитать всё, что есть в кольце сервера. `false` -- кольцо испорчено
/// (`Corrupted`): соединение надо разорвать, иначе каждое `DATA` давало
/// бы новую ошибку, а данные не доставлялись бы никогда.
fn receive_all(
    client: &SharedClient,
    buffer: &mut Vec<u8>,
    handler: &Arc<dyn MultiClientHandler>,
    running: &AtomicBool,
) -> bool {
    while running.load(Ordering::Acquire) {
        match client.receive_from_server(buffer) {
            Ok(len) => handler.on_message(&buffer[..len]),
            // `NotConnected` -- сессия сменилась посреди чтения (кадр новой
            // сессии не забран): конец сессии решает worker, без `on_error`.
            Err(ShmError::QueueEmpty | ShmError::NotConnected) => break,
            Err(err) => {
                let corrupted = err == ShmError::Corrupted;
                handler.on_error(err);
                return !corrupted;
            }
        }
    }
    true
}

/// Спин подключённого `MultiClient` перед сном (`MultiClientOptions::spin`).
/// `true` -- появились данные от сервера, новый `send` или остановка: в ядро
/// не уходить. Непустая очередь на входе -- спина нет (`wake` уже взведён).
fn spin_before_wait(
    spin: SpinPolicy,
    outbox: &ClientOutbox,
    running: &AtomicBool,
    has_unread: impl Fn() -> bool,
) -> bool {
    if !spin.is_enabled() || !outbox.queue.lock_clean().is_empty() {
        return false;
    }
    let sends = outbox.sends.load(Ordering::Acquire);
    spin_until(spin, None, || {
        has_unread()
            || outbox.sends.load(Ordering::Acquire) != sends
            || !running.load(Ordering::Acquire)
    })
}

/// Worker для MultiClient
fn client_worker(
    base_name: &str,
    handler: &Arc<dyn MultiClientHandler>,
    options: &MultiClientOptions,
    shared: &ClientShared,
) {
    let mut buffer = Vec::new();
    let running = &*shared.running;
    let wake = &*shared.wake;
    let outbox = &*shared.outbox;
    let effective_slot_timeout = clamp_slot_timeout(options.slot_timeout);
    let namespace = options.namespace.as_deref();

    let mut waiter = AbsenceWaiter {
        base_name,
        handler,
        options,
        shared,
        presence: None,
        reported: Reported::default(),
    };

    // Слот, с которого начинать перебор: после неудачного рукопожатия --
    // следующий за неудачным (T11b, как клиент C), иначе -- с нуля.
    let mut next_slot = 0;

    while running.load(Ordering::Acquire) {
        report_evicted(outbox, handler);
        // Шаг 1: атомарно захватываем свободный слот (без централизованного lobby).
        let (slot_id, slot_name, token) = match claim_free_slot(namespace, base_name, next_slot) {
            Ok(v) => v,
            Err(err) => {
                // Возможно, слот держит захват упавшего клиента: толкаем
                // сервер, чтобы он назначил дедлайн его освобождения. И при
                // `AccessDenied`: слот, занятый захватчиком с запрещающим
                // DACL (`B_N` создан заранее), не должен отключать толчок
                // остальных слотов.
                if matches!(err, ShmError::NoFreeSlot | ShmError::AccessDenied) {
                    nudge_stale_claims(namespace, base_name);
                }
                // Task 11 (R19): ни одного слота -- сервера нет, сон на
                // маяке базы. Все слоты заняты (`NoFreeSlot`, о каждой
                // попытке) или часть слотов чужие (`AccessDenied`: итог
                // перебора, где остальные слоты могли быть просто заняты, --
                // ждать выхода сервера нельзя) -- прежняя пауза
                // `retry_delay` (до Task 12).
                let go_on = if err == ShmError::NotConnected {
                    waiter.report_once(err);
                    waiter.await_server(Failure::Absent)
                } else {
                    if err == ShmError::NoFreeSlot {
                        waiter.reported.note(&err);
                        handler.on_error(err);
                    } else {
                        waiter.report_once(err);
                    }
                    waiter.pause()
                };
                if !go_on {
                    break;
                }
                continue;
            }
        };

        // Шаг 2: подключаемся к захваченному слоту обычным handshake.
        // Ревизия 2: ожидание `S2C_CONNECT` прерывается `wake` -- `stop`/Drop
        // не ждут `slot_timeout` (`stop` гасит `running` сам).
        let mut should_stop = || !running.load(Ordering::Acquire);
        let connected = SharedClient::connect_interruptible(
            namespace,
            &slot_name,
            effective_slot_timeout,
            // Ревизия 3: claim слота уже наш (`try_claim_slot`) -- рукопожатие
            // его не берёт и не освобождает (снимает `release_claim` ниже).
            ClaimMode::Held(token),
            Interrupt {
                wake: wake.as_handle(),
                should_stop: &mut should_stop,
            },
        );
        let mut client = match connected {
            Ok(c) => c,
            Err(_) if !running.load(Ordering::Acquire) => {
                release_claim(namespace, &slot_name, token);
                break;
            }
            Err(err) => {
                // Не подключились — освобождаем захваченный слот (best-effort;
                // иначе сервер вернёт его в оборот по RESERVE_TIMEOUT).
                release_claim(namespace, &slot_name, token);
                // Следующая попытка -- со следующего слота (T11b): слот, чьё
                // рукопожатие не удаётся (испорчен, сервер его не обслуживает),
                // не держит клиента, пока остальные свободны.
                next_slot = slot_id.saturating_add(1);
                // Task 11: процесс сервера слота завершился (слоты держит
                // кто-то ещё) -- сервера нет, сон на маяке базы; прочее --
                // прежняя пауза, одинаковая ошибка подряд -- один раз.
                let failure = presence::classify(&err, namespace, None);
                waiter.report_once(err);
                let go_on = match failure {
                    Some(Failure::Absent) => waiter.await_server(Failure::Absent),
                    _ => waiter.pause(),
                };
                if !go_on {
                    break;
                }
                continue;
            }
        };
        waiter.reported.clear();
        next_slot = 0;

        let layout = *client.layout();
        outbox
            .max_message
            .store(layout.client_to_server.max_message_size, Ordering::Release);
        *shared.layout.lock_clean() = Some(layout);
        shared.slot_id.store(slot_id, Ordering::Release);
        handler.on_connect(slot_id);

        // Шаг 3: работаем по событиям. Набор: [DISCONNECT, DATA, процесс
        // сервера (0.8+), wake] -- меньший индекс важнее.
        let client_events = client.events();
        let peer = client.peer_wait_handle();
        // Хвост массива -- заполнитель (`wake`), в ожидание уходит `[..count]`.
        let wake_handle = wake.as_handle();
        let mut handles = [
            client_events.disconnect.as_handle(),
            client_events.s2c.data.as_handle(),
            wake_handle,
            wake_handle,
        ];
        let (count, peer_index) = match peer {
            Some(peer) => {
                handles[2] = peer;
                (4, Some(2))
            }
            None => (3, None),
        };

        let reason = loop {
            // Сессия уже чужая (сверка и на пробуждениях ниже): отправка и
            // приём ответили бы `NotConnected` -- уходим, как там.
            if !client.is_session_current() {
                break Some(DisconnectReason::Graceful);
            }
            if !flush_queue(&client, outbox, handler) {
                break Some(DisconnectReason::Error);
            }
            if !running.load(Ordering::Acquire) {
                // Остановка: принятое до `stop()` уже в кольце -- сервер
                // дочитает его до DISCONNECT (Drop клиента ниже).
                break None;
            }
            if !receive_all(&client, &mut buffer, handler, running) {
                break Some(DisconnectReason::Error);
            }
            // Сессия сменилась посреди прохода (запись и чтение ответили
            // `NotConnected` молча) -- решение сразу, а не на пробуждении,
            // которого при чужой сессии может не быть (как клиент Auto).
            if !client.is_session_current() {
                break Some(DisconnectReason::Graceful);
            }

            // Спин (`MultiClientOptions::spin`) -- один бюджет на каждое
            // засыпание (в простое: один раз, затем сон без таймаута).
            // Непустая очередь (`send` из колбэка этого прохода) -- без
            // спина: `wake` уже взведён. Увидели данные (новый `send`) --
            // ожидание всё равно идёт, но с нулевым таймаутом: `DISCONNECT`
            // и смерть сервера проверяются после КАЖДОГО спина, а сверка
            // сессии -- в ветках ниже, как после обычного пробуждения
            // (кольцо могло достаться новому клиенту слота). `DATA` вручную
            // не сбрасываем.
            let spun = spin_before_wait(options.spin, outbox, running, || {
                client.has_unread_from_server()
            });
            let timeout = if spun {
                Some(Duration::ZERO)
            } else {
                options.poll_timeout
            };

            // Тесты (T11c): «противник» -- `DISCONNECT` к каждому ожиданию.
            #[cfg(test)]
            crate::server::adversary_resignal(client.view(), &client_events.disconnect);
            let result = win::wait_any(&handles[..count], timeout);
            shared.wakeups.fetch_add(1, Ordering::AcqRel);
            // T11c (решение AD): `DISCONNECT` -- уход, только если сервер
            // покинул НАШУ сессию (`server_left`: `server_state` или
            // `generation`). Иначе сигнал устаревший: поглощён (возвращать
            // некому -- сервер сигналит, только уйдя), остальные объекты
            // набора -- нулевым ожиданием (поток таких сигналов не скрывает
            // смерть сервера; данные, очередь и остановку проход и так
            // решает по состоянию).
            let result = match result {
                Ok(Some(0)) if !client.server_left() => {
                    rest_after_stale_disconnect(&handles[..count])
                }
                other => other,
            };
            match result {
                Ok(Some(0)) if !client.is_session_current() => {
                    // Сессия уже чужая: сигнал (автосброс) -- для нового
                    // клиента слота, возвращаем (как DATA ниже и клиент
                    // Auto); уходим без дочитывания чужого кольца.
                    let _ = client_events.disconnect.set();
                    break Some(DisconnectReason::Graceful);
                }
                Ok(Some(0)) => {
                    // Штатное отключение сервера: его последние сообщения
                    // доставляем ДО колбэка.
                    receive_all(&client, &mut buffer, handler, running);
                    break Some(DisconnectReason::Graceful);
                }
                Ok(Some(i)) if Some(i) == peer_index => {
                    receive_all(&client, &mut buffer, handler, running);
                    break Some(DisconnectReason::PeerDied);
                }
                // DATA, wake или страховочный таймаут -- следующий проход.
                // Ревизия 2: сначала сверить сессию. `generation` сменился --
                // слот уже у другого клиента (сервер старой версии освободил
                // claim до нашего пробуждения, и `complete_handshake`
                // нового клиента сбросил наш `DISCONNECT`). Сигнал `DATA` был
                // для него: возвращаем, и уходим без `DISCONNECT`.
                Ok(Some(index)) => {
                    if !client.is_session_current() {
                        if index == 1 {
                            let _ = client.events().s2c.data.set();
                        }
                        break Some(DisconnectReason::Graceful);
                    }
                }
                Ok(None) => {
                    if !client.is_session_current() {
                        break Some(DisconnectReason::Graceful);
                    }
                }
                Err(err) => {
                    handler.on_error(err);
                    break Some(DisconnectReason::Error);
                }
            }
        };

        // slot_id сбрасывается ЗДЕСЬ, а не только на ветке disconnect: выход по
        // `running == false` (stop()/Drop) её минует, и `is_connected()` навсегда
        // оставался бы `true` уже после остановки клиента (аудит 2026-07-28).
        shared.slot_id.store(SLOT_ID_NO_SLOT, Ordering::Release);
        if let Some(reason) = reason {
            // Сервер ушёл сам: `DISCONNECT` в ответ не сигналим -- поздний
            // сигнал мог бы достаться уже следующему клиенту этого слота.
            // Исключение -- наш разрыв из-за порчи кольца: тогда штатный
            // уход (Drop клиента ниже) сигналит `DISCONNECT` сам.
            client.end_session(reason);
            if running.load(Ordering::Acquire) {
                handler.on_disconnect_reason(reason);
            }
        }
        drop(client);
        // Слот освободился у нас — снимаем claim (best-effort), чтобы он сразу
        // вернулся в оборот. CAS token->FREE сработает, только если claim ещё наш
        // (если сервер уже отнял слот по таймауту/force-disconnect — это no-op).
        release_claim(namespace, &slot_name, token);

        // Пауза после конца сессии -- разовая реакция (клиент, отключённый
        // `disconnect_client`, не возвращается мгновенно). Сервер умер --
        // паузы нет: следующая попытка уводит в сон на маяке базы (Task 11),
        // и новый сервер принимается сразу.
        if reason != Some(DisconnectReason::PeerDied) && !waiter.pause() {
            break;
        }
    }
}

/// Клиент Multi вне подключения (Task 11, R19): сон в ядре на маяке
/// присутствия базы или процессе сервера вместо опроса раз в `retry_delay`.
struct AbsenceWaiter<'a> {
    base_name: &'a str,
    handler: &'a Arc<dyn MultiClientHandler>,
    options: &'a MultiClientOptions,
    shared: &'a ClientShared,
    /// Маяк базы: открывается при первой нужде, один раз на жизнь worker-а.
    presence: Option<PresenceWatch>,
    /// Ошибки эпизода вне подключения, о которых уже сообщено.
    reported: Reported,
}

impl AbsenceWaiter<'_> {
    fn running(&self) -> bool {
        self.shared.running.load(Ordering::Acquire)
    }

    fn wakeup(&self) {
        self.shared.wakeups.fetch_add(1, Ordering::AcqRel);
    }

    /// `on_error`, если это не повтор предыдущей ошибки (M1 ревью Task 11).
    fn report_once(&mut self, err: ShmError) {
        if self.reported.first(&err) {
            self.handler.on_error(err);
        }
    }

    /// `on_error` для ошибки маяка базы (свой ряд повторов).
    fn report_presence(&mut self, err: ShmError) {
        if self.reported.presence_first(&err) {
            self.handler.on_error(err);
        }
    }

    /// Пауза `retry_delay`, прерываемая остановкой; возврат -- пробуждение.
    /// `false` -- остановка.
    fn pause(&self) -> bool {
        let go_on = wait_delay_or(
            &self.shared.running,
            &self.shared.wake,
            self.options.retry_delay,
        );
        self.wakeup();
        go_on
    }

    /// Резерв (решение R-M): пауза `retry_delay`, но не дольше резервной
    /// проверки `discovery_fallback` (W(3)). `false` -- остановка.
    fn degraded_pause(&self) -> bool {
        let delay = presence::min_timeout(
            Some(self.options.retry_delay),
            self.options.discovery_fallback,
        )
        .unwrap_or(self.options.retry_delay);
        let go_on = wait_delay_or(&self.shared.running, &self.shared.wake, delay);
        self.wakeup();
        go_on
    }

    /// Ждать после неудачи `failure` (см. `presence::PresenceWatch::wait`):
    /// `wake` -- индекс 0 (`stop` прерывает сразу, `send` -- отчёт о
    /// вытесненных и снова сон); ждать нечего -- резервная пауза
    /// `retry_delay` (решение R-M). Сон -- не дольше `discovery_fallback`
    /// от входа (решение W(3)), затем попытка. `false` -- остановка.
    fn await_server(&mut self, failure: Failure) -> bool {
        let deadline = crate::ring::deadline_of(presence::min_timeout(
            self.options.poll_timeout,
            self.options.discovery_fallback,
        ));
        loop {
            if !self.running() {
                return false;
            }
            let left = deadline.map(|at| at.saturating_duration_since(Instant::now()));
            if left.is_some_and(|left| left.is_zero()) {
                return true;
            }
            if self.presence.is_none() {
                match PresenceWatch::open(
                    self.options.namespace.as_deref(),
                    self.options.namespace.as_ref(),
                    &multi_beacon_name(self.base_name),
                    &self.options.security,
                ) {
                    Ok(watch) => self.presence = Some(watch),
                    Err(err) => {
                        self.report_presence(err);
                        return self.degraded_pause();
                    }
                }
            }
            let wakeups = &self.shared.wakeups;
            let waited = match self.presence.as_mut() {
                Some(watch) => {
                    watch.wait(failure, Some(self.shared.wake.as_handle()), left, &|| {
                        wakeups.fetch_add(1, Ordering::AcqRel);
                    })
                }
                None => Ok(PresenceWait::Unavailable),
            };
            match waited {
                Ok(PresenceWait::Retry | PresenceWait::TimedOut) => return self.running(),
                Ok(PresenceWait::Woken) => report_evicted(&self.shared.outbox, self.handler),
                Ok(PresenceWait::Unavailable) => return self.degraded_pause(),
                Err(err) => {
                    self.report_presence(err);
                    return self.degraded_pause();
                }
            }
        }
    }
}

/// Пытается атомарно захватить конкретный слот через `compare_exchange`.
/// `Ok(true)` — захвачено нами; `Ok(false)` — слот занят/невалиден;
/// `Err(AccessDenied)` (0.10) — слот есть, но DACL не даёт его этому
/// пользователю (чужой сервер или захватчик имени); любая другая `Err(_)` —
/// слота с таким именем нет (сегмент не открылся).
fn try_claim_slot(
    namespace: Option<&PrivateNamespace>,
    slot_name: &str,
    token: u32,
) -> Result<bool> {
    // Сервер создаёт секцию под именем `ObjectPath::mapping(slot_name)` —
    // открываем так же (в том же пространстве), но отображаем только
    // заголовок: кольца для захвата не нужны.
    let mapping = Mapping::open_header(ObjectPath::mapping(namespace, slot_name))?;
    let Some(control) = SharedView::peek_control(&mapping) else {
        // Не опубликован (сервер ещё инициализирует), другой формат или
        // чужой объект -- пропускаем.
        return Ok(false);
    };
    let claimed = control
        .slot_claim
        .compare_exchange(CLAIM_FREE, token, Ordering::AcqRel, Ordering::Acquire)
        .is_ok();
    if claimed {
        // Находка 23: время создания своего процесса -- ДО PID, чья
        // Release-запись публикует обе половины: сервер, прочитавший
        // `slot_owner_pid` (Acquire), отличит переиспользованный PID.
        control.announce_owner_start(win::current_process_start_time());
        // Публикуем свой PID для liveness-проверки сервером (см.
        // `ControlBlock::slot_owner_pid`) -- пишем СРАЗУ после успешного захвата,
        // Release гарантирует, что сервер, увидевший claim (Acquire), увидит
        // и корректный PID, а не мусор/значение от предыдущего владельца.
        control
            .slot_owner_pid
            .store(std::process::id(), Ordering::Release);
    }
    Ok(claimed)
    // mapping размапится здесь; захваченный claim остаётся в shared memory.
}

/// Снять собственный claim со слота (CAS token -> FREE), если он всё ещё наш.
fn release_claim(namespace: Option<&PrivateNamespace>, slot_name: &str, token: u32) {
    if let Ok(mapping) = Mapping::open_header(ObjectPath::mapping(namespace, slot_name))
        && let Some(control) = SharedView::peek_control(&mapping)
    {
        let _ = control.slot_claim.compare_exchange(
            token,
            CLAIM_FREE,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

/// «Толчок» сервера (0.9): клиент, не нашедший свободного слота, взводит
/// `C2S_CONNECT_REQ` каждого слота, чей claim занят, а сервер его ещё не
/// принял (`server_state != SERVER_READY`). Сервер просыпается, видит
/// `CONNECT_REQ` без `CLIENT_HELLO` (не рукопожатие -- claim не трогает) и
/// назначает дедлайн освобождения протухшего захвата. Так упавший между
/// захватом и подключением клиент не держит слот, пока сервер спит, -- без
/// какого-либо тика на сервере. Перебор -- как в `claim_free_slot`: до
/// первого отсутствующего слота, `slot_count` не учитывается; слот с отказом
/// в доступе (0.10) пропускается, а не завершает перебор.
fn nudge_stale_claims(namespace: Option<&PrivateNamespace>, base_name: &str) {
    for slot_id in 0..MAX_MULTI_CLIENTS {
        let slot_name = format!("{base_name}_{slot_id}");
        let mapping = match Mapping::open_header(ObjectPath::mapping(namespace, &slot_name)) {
            Ok(mapping) => mapping,
            Err(ShmError::AccessDenied) => continue, // слот есть, но чужой
            Err(_) => break,                         // слотов больше нет
        };
        let Some(control) = SharedView::peek_control(&mapping) else {
            continue;
        };
        let claimed = control.slot_claim.load(Ordering::Acquire) != CLAIM_FREE;
        let accepted = control.server_state.load(Ordering::Acquire) == HANDSHAKE_SERVER_READY;
        if claimed
            && !accepted
            && let Ok(event) = EventHandle::open(ObjectPath::event(
                namespace,
                &slot_name,
                Direction::ClientToServer,
                EVENT_CONNECT_REQ_SUFFIX,
            ))
        {
            let _ = event.set();
        }
    }
}

/// Пробегает слоты `base_name_0..` и атомарно захватывает первый свободный.
/// Конкурентные клиенты захватывают РАЗНЫЕ слоты (CAS на разной памяти).
///
/// Перебор -- до первого отсутствующего слота, не дальше
/// `MAX_MULTI_CLIENTS`. `ControlBlock::slot_count` границей НЕ служит: его
/// может переписать любой клиент с доступом к слоту, и `slot_count = 1` в слоте 0
/// закрыл бы остальные слоты для всех -- отказ в обслуживании.
///
/// 0.10: слот, в доступе к которому отказано (`AccessDenied` -- DACL
/// сервера не включает этого пользователя), перебор НЕ завершает: слот
/// существует, просто чужой. Конец перебора -- любая другая ошибка
/// открытия (слота нет). Приоритет итоговой ошибки, если ни один слот не
/// захвачен: `AccessDenied` (был хоть один отказ в доступе) > `NoFreeSlot`
/// (слоты есть, все заняты) > `NotConnected` (ни одного слота -- сервер не
/// запущен). Иначе клиент пользователя против службы SYSTEM с политикой по
/// умолчанию видел бы вечный `NotConnected` -- неотличимо от «сервера нет».
///
/// `start` (T11b, поведение клиента C): перебор начинается со слота `start`
/// и по кругу возвращается к `0..start` -- клиент, чьё рукопожатие на слоте
/// `k` не удалось, следующей попыткой берёт `k + 1`, а не снова `k` (иначе
/// испорченный слот 0 держал бы клиента вечно, хотя остальные свободны).
/// Отсутствующий слот завершает только свою часть круга.
fn claim_free_slot(
    namespace: Option<&PrivateNamespace>,
    base_name: &str,
    start: u32,
) -> Result<(u32, String, u32)> {
    let token = crate::shared::claim_token();
    let mut saw_slot = false;
    let mut denied = false;
    let start = start.min(MAX_MULTI_CLIENTS);
    for range in [start..MAX_MULTI_CLIENTS, 0..start] {
        for slot_id in range {
            let slot_name = format!("{base_name}_{slot_id}");
            match try_claim_slot(namespace, &slot_name, token) {
                Ok(true) => return Ok((slot_id, slot_name, token)),
                Ok(false) => saw_slot = true,
                Err(ShmError::AccessDenied) => denied = true,
                Err(_) => break, // слотов дальше нет
            }
        }
    }
    if denied {
        Err(ShmError::AccessDenied) // слоты есть, но DACL их не даёт
    } else if saw_slot {
        Err(ShmError::NoFreeSlot) // сервер есть, но все слоты заняты
    } else {
        Err(ShmError::NotConnected) // сервер не запущен (нет ни одного слота)
    }
}

#[cfg(test)]
mod tests;
