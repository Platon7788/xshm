use std::collections::VecDeque;
use std::os::windows::io::{AsHandle, BorrowedHandle};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::client::{ClaimMode, Interrupt, SharedClient};
use crate::constants::{FRAME_ALIGN, MAX_RING_CAPACITY, MIN_MESSAGE_SIZE};
use crate::error::{DisconnectReason, Result, ShmError};
use crate::layout::{ChannelLayout, max_payload_for};
use crate::namespace::PrivateNamespace;
use crate::presence::{self, Failure, Presence, PresenceWait, PresenceWatch, Reported};
use crate::ring::{FreeSpace, SpinPolicy, WriteOutcome, frame_size, spin_until};
use crate::security::ObjectSecurity;
use crate::server::SharedServer;
use crate::sync_ext::{LockExt, join_worker};
use crate::wait_delay_or;
use crate::win::{self, EventHandle};

pub(crate) fn map_spawn_error(err: std::io::Error, context: &'static str) -> ShmError {
    // Код Win32 -- беззнаковый по смыслу (`GetLastError`): битовый каст.
    let code = err.raw_os_error().map_or(u32::MAX, i32::cast_unsigned);
    ShmError::WindowsError { code, context }
}

/// Джойнит worker-поток, если это безопасно; при self-join -- отпускает
/// `JoinHandle` без блокировки.
///
/// Если `stop()`/`Drop` вызывается СИНХРОННО из собственного worker-потока
/// (пользовательский `AutoHandler` останавливает или дропает сервер/клиент
/// прямо внутри своего же callback'а -- `on_disconnect`, `on_message` и т.п.
/// вызываются ИМЕННО на worker-потоке), безусловный `handle.join()` был бы
/// self-join deadlock: поток ждал бы сам себя навсегда (аудит 2026-07-10).
///
/// При обнаружении self-join `JoinHandle` просто дропается без join: поток
/// уже видит `running=false` (устанавливается до этого вызова) и завершится
/// сам, как только колбэк вернёт управление.
///
/// `Err(WorkerPanicked)` -- worker завершился паникой (см. `join_worker`).
pub(crate) fn join_unless_self(handle: JoinHandle<()>) -> Result<()> {
    if handle.thread().id() == thread::current().id() {
        return Ok(());
    }
    join_worker(handle)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelKind {
    ServerToClient,
    ClientToServer,
}

/// Callback-интерфейс `AutoServer`/`AutoClient`.
///
/// Все методы вызываются СИНХРОННО из собственного worker-потока
/// `AutoServer`/`AutoClient`. После возврата `stop()` (или `Drop`) из
/// другого потока ни один метод больше не вызывается. Если реализация
/// останавливает/дропает тот же объект изнутри своего колбэка, ожидания
/// самого себя нет: worker завершится сразу после возврата из колбэка.
pub trait AutoHandler: Send + Sync + 'static {
    fn on_connect(&self) {}
    fn on_disconnect(&self) {}
    /// Отключение с причиной (0.8+). По умолчанию вызывает `on_disconnect()` --
    /// существующие реализации работают как раньше; переопределите, чтобы
    /// отличать штатное отключение от смерти процесса пира
    /// (`DisconnectReason::PeerDied`). Worker вызывает ТОЛЬКО этот метод.
    fn on_disconnect_reason(&self, reason: DisconnectReason) {
        let _ = reason;
        self.on_disconnect();
    }
    fn on_message(&self, _direction: ChannelKind, _payload: &[u8]) {}
    /// Потеряны сообщения направления `direction`: `count` вытеснено из
    /// кольца перезаписью или из очереди отправки при её переполнении
    /// (`max_send_queue`), либо (только `AutoServer`, 0.10) сброшено из
    /// очереди в конце сессии -- неотправленное ушедшему клиенту следующему
    /// не доставляется (в том числе принятое `try_send`); отчёт -- перед
    /// `on_disconnect_reason`, граница -- момент, когда worker обнаружил
    /// конец сессии. Вызывается раз на пачку, а не на каждое сообщение.
    fn on_overflow(&self, _direction: ChannelKind, _count: u32) {}
    /// Читатель освободил место в исходящем кольце (событие SPACE: кольцо
    /// опустело или места хватает ждущему lossless-сообщению). К моменту
    /// вызова worker уже дописал в кольцо из своей очереди всё, что влезло,
    /// — `try_send`, получивший `QueueFull`, стоит повторить.
    fn on_space_available(&self, _direction: ChannelKind) {}
    /// Ошибка worker-а. Клиент (`AutoClient::connect`): неудачная попытка
    /// подключения приходит сюда (0.10, Task 11: одинаковая ошибка подряд --
    /// один раз, кроме `AlreadyConnected`; другая ошибка между ними -- и
    /// повтор сообщается снова), а
    /// следующая попытка -- по событию (см. `AutoOptions::reconnect_delay`).
    /// `AlreadyConnected` (0.10, ревизия 3 рукопожатия): канал занят другим
    /// клиентом -- о каждой попытке, повтор с растущим отступом: пауза
    /// удваивается от `reconnect_delay` до 2 с (`BUSY_RECONNECT_CAP`;
    /// больший `reconnect_delay` -- без роста), любой другой исход попытки
    /// её сбрасывает. Выделенный канал Dispatch не повторяет:
    /// `AlreadyConnected` там -- канал занят чужим.
    fn on_error(&self, _err: ShmError) {}
}

#[derive(Clone, Debug)]
pub struct AutoOptions {
    /// Страховочный таймаут ожидания worker-а. `None` (по умолчанию, 0.9+)
    /// -- только события: worker спит в ядре, пока не придут данные, место,
    /// отключение, смерть пира, новая команда (`send`/`try_send`/`stop`
    /// будят его своим событием) -- ни одного пробуждения в простое.
    /// `Some(t)` -- дополнительно просыпаться не реже `t` (не нужно для
    /// корректности; оставлено для диагностики).
    pub poll_timeout: Option<Duration>,
    /// Пауза между попытками переподключения `AutoClient` (`AutoServer` не
    /// использует). Канал занят другим клиентом (`AlreadyConnected`) --
    /// пауза удваивается с каждой неудачей до 2 с (или остаётся этой, если
    /// она больше); первый повтор -- через `reconnect_delay`.
    ///
    /// 0.10 (Task 11, INTEROP R19): **сервера нет** (секции или событий нет,
    /// сегмент не опубликован, процесс сервера завершился) -- клиент НЕ
    /// опрашивает раз в `reconnect_delay`, а спит в ядре на маяке
    /// присутствия канала `<имя>_beacon` (его держит каждый именованный
    /// сервер) и подключается сразу, как сервер его взведёт; маяк,
    /// оставленный упавшим сервером, клиент гасит (`Beacon::clear_if_stale`)
    /// и спит дальше. **Сервер есть, но подключиться нельзя** (другой формат
    /// памяти, порча, отказ в доступе) -- клиент спит до выхода процесса
    /// этого сервера. Пауза `reconnect_delay` остаётся: разовая после конца
    /// сессии (после смерти сервера -- без неё), при занятом канале и как
    /// резерв (решение R-M) -- сервер не ответил за `connect_timeout`, маяк
    /// недоступен (нет прав или привилегии `Global\`), процесс сервера не
    /// открылся.
    pub reconnect_delay: Duration,
    /// Резервная проверка обнаружения сервера `AutoClient` (0.10, решение
    /// W(3); `AutoServer` не использует): пока сервер не найден и клиент
    /// спит на маяке присутствия или процессе сервера, он всё равно делает
    /// попытку не реже этого интервала. Защищает от маяка, занятого заранее
    /// и никогда не взводимого, от сервера без маяка (пир без R19, сервер,
    /// которому не удалось создать маяк, -- `PresenceStatus::Unavailable`)
    /// и от подменённой личности сервера в состоянии маяка. Подключённый
    /// клиент по нему не просыпается. По умолчанию --
    /// `Some(DEFAULT_DISCOVERY_FALLBACK)` (10 с: не больше 0,1 пробуждения в
    /// секунду, пока сервера нет); `None` -- только события.
    /// `Some(Duration::ZERO)` -- `AutoClient::connect` отказывает
    /// `ShmError::InvalidConfig` (каждое ожидание стало бы немедленной
    /// попыткой -- цикл без сна).
    pub discovery_fallback: Option<Duration>,
    /// Сколько `AutoClient` ждёт ответа сервера на одну попытку
    /// подключения (`AutoServer` не использует).
    pub connect_timeout: Duration,
    /// Лимит очереди отправки (сообщения, принятые `send`/`try_send`, но
    /// ещё не записанные в кольцо); `0` трактуется как `1`. Переполнение:
    /// `send` вытесняет самое старое обычное сообщение (`on_overflow`),
    /// `try_send` получает `QueueFull`.
    pub max_send_queue: usize,
    /// Сколько сообщений worker доставляет за один проход до проверки
    /// остальных событий.
    pub recv_batch: usize,
    /// Раскладка канала (размер колец, лимиты сообщений по каждому
    /// направлению). Использует только `AutoServer`: клиент получает её из
    /// сегмента (`AutoClient::layout`).
    pub layout: ChannelLayout,
    /// Политика безопасности объектов канала (0.10; до 0.10 -- NULL DACL):
    /// `AutoServer` -- секции, 7 событий и маяка присутствия `<имя>_beacon`
    /// (R19). `AutoClient` объекты канала открывает, а не создаёт, но маяк
    /// присутствия создаёт сам, если пришёл раньше сервера (Task 11), -- с
    /// этой политикой. DACL маяка задаёт тот, кто создал его первым
    /// (решение R-L): служба на `Global\` с `ObjectSecurity::Everyone`
    /// требует той же политики у клиентов (или старта службы первой), иначе
    /// клиенты другого пользователя получат отказ в доступе к маяку и
    /// перейдут на резервный повтор `reconnect_delay`. По умолчанию --
    /// `ObjectSecurity::CurrentUser`.
    pub security: ObjectSecurity,
    /// Частное пространство имён канала (0.10): `AutoServer` создаёт объекты
    /// в нём, `AutoClient` в нём же их ищет (при каждом переподключении).
    /// Имя канала -- относительное (без `Local\`/`Global\`). `None` (по
    /// умолчанию) -- общий `\BaseNamedObjects`. Worker держит ссылку, пока
    /// жив: пространство не исчезает, пока его кто-то использует.
    pub namespace: Option<Arc<PrivateNamespace>>,
    /// Спин worker-а перед сном в ядре (0.10): подключённый worker, которому
    /// нечего делать, до `spin.iterations` раз проверяет входящее кольцо по
    /// позициям (и новые `send`/`try_send`, и остановку), прежде чем уснуть
    /// на наборе событий. Один бюджет на каждое засыпание: в простое worker
    /// отспинивает его один раз и спит без таймаута. После удачного спина
    /// набор событий всё равно опрашивается (ожидание с нулевым таймаутом).
    ///
    /// Во время спина `DISCONNECT`, `SPACE` и смерть пира не наблюдаются:
    /// они замечаются не позже конца бюджета (или раньше, если придут
    /// данные). Цена -- занятое ядро CPU на время спина; длительность
    /// итерации (`pause`) зависит от CPU (порядка 10..140 тактов), поэтому
    /// бюджет в итерациях -- разное время на разных машинах. По умолчанию
    /// -- `SpinPolicy::NONE` (без спина). Пиру не виден.
    pub spin: SpinPolicy,
    /// Способ доставки входящих сообщений в `on_message` (0.10). По
    /// умолчанию -- копия в буфер worker-а (`Delivery::COPY`); срез прямо
    /// из кольца -- только через `unsafe` (`AutoOptions::zero_copy_delivery`).
    pub delivery: Delivery,
}

/// Способ доставки входящих сообщений в `on_message` Auto (0.10).
///
/// [`Delivery::COPY`] (по умолчанию): worker копирует сообщение в свой
/// буфер (с проверкой «перезаписано во время копии» -- ADR 11) и отдаёт
/// срез буфера -- безопасно при любом пире. Zero-copy ([`Delivery::zero_copy`])
/// отдаёт срез прямо из кольца разделяемой памяти, без копии; создать его
/// можно только `unsafe` -- выбор «пир доверенный и пишет без перезаписи»
/// подтверждает вызывающий.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Delivery {
    zero_copy: bool,
}

impl Delivery {
    /// Копия в буфер worker-а (по умолчанию).
    pub const COPY: Self = Self { zero_copy: false };

    /// Доставка срезом прямо из кольца.
    ///
    /// # Safety
    /// Пока выполняется `on_message`, никто не изменяет байты сообщения в
    /// разделяемой памяти (иначе -- гонка данных и TOCTOU в обработчике,
    /// см. `SharedServer::receive_with`). Выполнено, когда пир -- доверенный
    /// честный xshm, пишущий в эту сторону без перезаписи (`try_send`,
    /// не `send`). **Пир, отправляющий в это направление обычным `send`
    /// (overwrite-oldest, а не `try_send`/lossless), контракт нарушает**:
    /// его вытеснение может затереть кадр во время `on_message`. Перезапись,
    /// всё же замеченная после `on_message`, даёт
    /// `on_error(Overrun)` -- это защита в глубину, а не разрешение.
    /// `on_message` держит кадр в кольце: долгий обработчик задерживает
    /// освобождение места для писателя.
    #[must_use]
    pub const unsafe fn zero_copy() -> Self {
        Self { zero_copy: true }
    }

    /// Доставка срезом из кольца включена.
    #[must_use]
    pub const fn is_zero_copy(self) -> bool {
        self.zero_copy
    }
}

impl AutoOptions {
    /// Включить доставку `on_message` срезом прямо из кольца (zero-copy,
    /// 0.10), без промежуточного буфера worker-а.
    ///
    /// # Safety
    /// Контракт [`Delivery::zero_copy`]: пока выполняется `on_message`, пир
    /// не изменяет байты сообщения (доверенный честный пир xshm, пишущий
    /// без перезаписи). Пир, который шлёт в это направление обычным `send`
    /// (overwrite-oldest), а не `try_send`/lossless, контракт НАРУШАЕТ.
    #[must_use]
    pub const unsafe fn zero_copy_delivery(mut self) -> Self {
        // SAFETY: контракт `Delivery::zero_copy` -- контракт этой функции.
        self.delivery = unsafe { Delivery::zero_copy() };
        self
    }
}

impl Default for AutoOptions {
    fn default() -> Self {
        Self {
            poll_timeout: None,
            reconnect_delay: Duration::from_millis(250),
            discovery_fallback: Some(presence::DEFAULT_DISCOVERY_FALLBACK),
            connect_timeout: Duration::from_secs(2),
            max_send_queue: 256,
            recv_batch: 32,
            layout: ChannelLayout::DEFAULT,
            security: ObjectSecurity::default(),
            namespace: None,
            spin: SpinPolicy::NONE,
            delivery: Delivery::COPY,
        }
    }
}

/// Снимок счётчиков `AutoServer`/`AutoClient` (`stats()`).
///
/// Поля читаются свободно; строит снимок только крейт (`#[non_exhaustive]`,
/// 0.10, как `FreeSpace`): новый счётчик не ломает потребителей. Снаружи --
/// `Default`.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct AutoStatsSnapshot {
    pub sent_messages: u64,
    pub send_overflows: u64,
    pub received_messages: u64,
    pub receive_overflows: u64,
    /// `AutoServer` (0.10): сообщения, сброшенные из очереди отправки в
    /// конце сессии -- они предназначались ушедшему клиенту (о каждой
    /// пачке -- один `on_overflow`). У `AutoClient` всегда `0`: его очередь
    /// уходит в следующую сессию с тем же сервером.
    pub discarded_messages: u64,
}

#[derive(Default, Debug)]
struct AutoStats {
    sent_messages: AtomicU64,
    send_overflows: AtomicU64,
    received_messages: AtomicU64,
    receive_overflows: AtomicU64,
    discarded_messages: AtomicU64,
    /// Возвраты worker-а из ожидания в ядре (0.9) -- тесты доказывают по
    /// нему отсутствие пробуждений в простое. В снимок не входит.
    wakeups: AtomicU64,
    /// Тесты (C1 повторного ревью Task 11): попытки подключения клиента --
    /// горячий цикл попыток без ожиданий в ядре счётчик `wakeups` не видит.
    #[cfg(test)]
    attempts: AtomicU64,
}

impl AutoStats {
    // Relaxed во всех счётчиках: статистика, ничего не публикует.
    fn wake(&self) {
        self.wakeups.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> AutoStatsSnapshot {
        AutoStatsSnapshot {
            sent_messages: self.sent_messages.load(Ordering::Relaxed),
            send_overflows: self.send_overflows.load(Ordering::Relaxed),
            received_messages: self.received_messages.load(Ordering::Relaxed),
            receive_overflows: self.receive_overflows.load(Ordering::Relaxed),
            discarded_messages: self.discarded_messages.load(Ordering::Relaxed),
        }
    }
}

/// Исходящее сообщение в очереди.
#[derive(Debug)]
struct Outgoing {
    data: Vec<u8>,
    /// `true` -- пришло через `try_send`: пишется в кольцо только
    /// `try_write_message` (без перезаписи) и никогда не вытесняется из
    /// очереди; `false` -- обычный `send` с политикой overwrite-oldest.
    lossless: bool,
}

impl Outgoing {
    /// Байты, которые сообщение займёт в кольце.
    const fn frame(&self) -> usize {
        frame_size(self.data.len())
    }
}

type SendQueue = VecDeque<Outgoing>;

/// Очередь отправки, общая для API-потоков и worker-а.
///
/// Отправители кладут в хвост под коротким lock-ом и сами соблюдают
/// `max_send_queue` (вытесняя старейшее обычное сообщение) -- память
/// ограничена даже когда worker надолго занят колбэком. Worker снимает
/// голову, пишет в кольцо без lock-а и возвращает её обратно, если кольцо
/// полно (порядок FIFO не нарушается: голову снимает только он).
#[derive(Debug, Default)]
struct Outbox {
    queue: Mutex<SendQueue>,
    /// Вытеснено отправителями при переполнении очереди и ещё не сообщено:
    /// `on_overflow` зовёт worker (колбэки -- только на его потоке).
    evicted: AtomicU32,
}

impl Outbox {
    fn lock(&self) -> std::sync::MutexGuard<'_, SendQueue> {
        // Паника внутри lock-а невозможна (только операции VecDeque), но
        // отравленный mutex не должен валить поток отправителя.
        self.queue.lock_clean()
    }

    fn pop_front(&self) -> Option<Outgoing> {
        self.lock().pop_front()
    }

    fn push_front(&self, msg: Outgoing) {
        self.lock().push_front(msg);
    }
}

/// Счётчики исходящего направления, общие для API-потоков и worker-а.
///
/// `pending_*` -- принятые `send`/`try_send`, но ещё не записанные в кольцо
/// (в `Outbox` или в руках worker-а); `ring_free_*` -- последний снимок
/// `free_space()` кольца, снятый worker-ом (он же единственный писатель
/// кольца, поэтому снимок -- нижняя граница до его следующей записи).
///
/// Порядок, на котором держится консервативность `estimate()`: worker
/// сначала публикует новый (меньший) снимок кольца, и только потом
/// (`Release`) уменьшает `pending_*`; `estimate()` читает `pending_*`
/// (`Acquire`) ДО снимка. Увидел уменьшенный `pending` -- увидит и
/// уменьшенный снимок; не увидел -- вычтет ещё не списанное сообщение из
/// снимка, в котором его ещё нет. В обоих случаях оценка не завышена.
///
/// `peer_*` -- состояние наблюдения за процессом пира, которое публикует
/// worker (он единственный держит `ProcessWatch`): PID и статус
/// `PEER_UNKNOWN`/`PEER_ALIVE`/`PEER_DEAD`.
///
/// `tx_max_message` -- лимит размера сообщения исходящего кольца (`0` --
/// ещё неизвестен: клиент до первого подключения); `layout` -- раскладка
/// текущего (у клиента -- последнего) подключения.
#[derive(Debug, Default)]
struct ChannelState {
    pending_msgs: AtomicUsize,
    pending_bytes: AtomicUsize,
    ring_free_bytes: AtomicUsize,
    /// `FreeSpace::max_frame` того же снимка (0.10, с учётом PAD).
    ring_max_frame: AtomicUsize,
    ring_free_msgs: AtomicU32,
    peer_pid: AtomicU32,
    peer_status: AtomicU8,
    /// Сервер: PID клиента, сверенный по ненулевому времени создания
    /// (`0` -- не сверен); только для сверки PID канала Dispatch.
    peer_verified_pid: AtomicU32,
    tx_max_message: AtomicUsize,
    layout: Mutex<Option<ChannelLayout>>,
}

const PEER_UNKNOWN: u8 = 0;
const PEER_ALIVE: u8 = 1;
const PEER_DEAD: u8 = 2;

impl ChannelState {
    /// Раскладка канала известна (сервер -- при старте, клиент -- при
    /// каждом подключении); `tx` -- исходящее направление этой стороны.
    fn set_layout(&self, layout: ChannelLayout, tx: ChannelKind) {
        let ring = match tx {
            ChannelKind::ServerToClient => layout.server_to_client,
            ChannelKind::ClientToServer => layout.client_to_server,
        };
        self.tx_max_message
            .store(ring.max_message_size, Ordering::Release);
        *self.layout.lock_clean() = Some(layout);
    }

    fn layout(&self) -> Option<ChannelLayout> {
        *self.layout.lock_clean()
    }

    /// Проверка длины сообщения до постановки в очередь: против лимита
    /// исходящего кольца, если он уже известен, иначе -- против
    /// абсолютного максимума формата.
    fn check_len(&self, len: usize) -> Result<()> {
        if len < MIN_MESSAGE_SIZE {
            return Err(ShmError::MessageTooSmall);
        }
        let limit = match self.tx_max_message.load(Ordering::Acquire) {
            0 => max_payload_for(MAX_RING_CAPACITY),
            limit => limit,
        };
        if len > limit {
            return Err(ShmError::MessageTooLarge);
        }
        Ok(())
    }

    /// Worker: соединение поднято; `pid` -- наблюдаемый пир (если есть).
    fn peer_connected(&self, pid: Option<u32>) {
        self.peer_pid.store(pid.unwrap_or(0), Ordering::Release);
        let status = if pid.is_some() {
            PEER_ALIVE
        } else {
            PEER_UNKNOWN
        };
        self.peer_status.store(status, Ordering::Release);
    }

    /// Worker: соединение разорвано. После смерти пира PID сохраняется
    /// (для диагностики), статус -- `PEER_DEAD` до следующего подключения.
    fn peer_disconnected(&self, reason: DisconnectReason) {
        self.peer_verified_pid.store(0, Ordering::Release);
        if reason == DisconnectReason::PeerDied {
            self.peer_status.store(PEER_DEAD, Ordering::Release);
        } else {
            self.peer_status.store(PEER_UNKNOWN, Ordering::Release);
            self.peer_pid.store(0, Ordering::Release);
        }
    }

    fn peer_alive(&self) -> Option<bool> {
        match self.peer_status.load(Ordering::Acquire) {
            PEER_ALIVE => Some(true),
            PEER_DEAD => Some(false),
            _ => None,
        }
    }

    fn peer_pid(&self) -> Option<u32> {
        Some(self.peer_pid.load(Ordering::Acquire)).filter(|&pid| pid != 0)
    }

    /// Принять сообщение в учёт.
    fn acquire(&self, frame: usize) {
        self.pending_bytes.fetch_add(frame, Ordering::AcqRel);
        self.pending_msgs.fetch_add(1, Ordering::AcqRel);
    }

    /// То же, но только если в очереди есть место (`try_send`): CAS-цикл,
    /// чтобы несколько отправителей не превысили `max_send_queue`.
    fn try_acquire(&self, frame: usize, max_send_queue: usize) -> bool {
        let admitted = self
            .pending_msgs
            .try_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                (cur < max_send_queue).then_some(cur + 1)
            })
            .is_ok();
        if admitted {
            self.pending_bytes.fetch_add(frame, Ordering::AcqRel);
        }
        admitted
    }

    /// Списать сообщение из учёта (записано в кольцо или выброшено).
    fn release(&self, frame: usize) {
        self.pending_bytes.fetch_sub(frame, Ordering::AcqRel);
        self.pending_msgs.fetch_sub(1, Ordering::AcqRel);
    }

    /// Опубликовать снимок свободного места кольца (только worker).
    fn publish_ring(&self, space: FreeSpace) {
        self.ring_free_bytes.store(space.bytes, Ordering::Release);
        self.ring_max_frame
            .store(space.max_frame, Ordering::Release);
        self.ring_free_msgs.store(space.messages, Ordering::Release);
    }

    /// Консервативная оценка места для НОВОГО сообщения: кольцо минус всё,
    /// что уже принято, но ещё не записано; слоты ограничены и кольцом, и
    /// свободным местом очереди `max_send_queue`.
    ///
    /// `max_frame` (0.10, PAD): без ожидающих сообщений -- из снимка кольца
    /// (там позиция записи известна). С ожидающими позиция, с которой ляжет
    /// новое сообщение, неизвестна, поэтому оценка -- наихудший PAD. Ожидающие
    /// и новое пишутся подряд и занимают не больше свободного места
    /// (`<= capacity`), значит пересекают конец кольца не больше одного раза
    /// -- PAD на всю серию один и меньше вызвавшего его кадра: `<= max(F,
    /// M) - 8`, где `F` -- новый кадр, `M` -- кадр максимального сообщения
    /// направления (оценка сверху для любого ожидающего). Новый кадр `F`
    /// помещается, если `pending + F + max(F, M) - 8 <= ring_bytes`; так как
    /// `F <= M`, это `F <= bytes + 8 - M` (`pending_max_frame`).
    fn estimate(&self, max_send_queue: usize) -> FreeSpace {
        let pending_msgs = self.pending_msgs.load(Ordering::Acquire);
        let pending_bytes = self.pending_bytes.load(Ordering::Acquire);
        let ring_msgs = self.ring_free_msgs.load(Ordering::Acquire) as usize;
        let ring_bytes = self.ring_free_bytes.load(Ordering::Acquire);
        let ring_max_frame = self.ring_max_frame.load(Ordering::Acquire);
        let queue_room = max_send_queue.saturating_sub(pending_msgs);
        let ring_room = ring_msgs.saturating_sub(pending_msgs);
        let bytes = ring_bytes.saturating_sub(pending_bytes);
        let messages = queue_room.min(ring_room) as u32;
        if bytes == 0 || messages == 0 {
            return FreeSpace::ZERO;
        }
        let max_message_size = self.tx_max_message.load(Ordering::Acquire);
        let max_frame = if pending_msgs == 0 {
            ring_max_frame.min(bytes)
        } else {
            pending_max_frame(bytes, frame_size(max_message_size))
        };
        FreeSpace {
            bytes,
            max_frame,
            messages,
            max_message_size,
        }
    }
}

/// Наибольший новый кадр, гарантированно помещающийся после ожидающих
/// сообщений при любой позиции записи: `bytes` -- свободное место кольца
/// за вычетом ожидающих, `max_frame` -- кадр максимального сообщения
/// направления (наихудший PAD серии -- `max_frame - 8`, см.
/// `ChannelState::estimate`).
const fn pending_max_frame(bytes: usize, max_frame: usize) -> usize {
    (bytes + FRAME_ALIGN).saturating_sub(max_frame) & !(FRAME_ALIGN - 1)
}

/// Постановка сообщения в очередь (общая для `AutoServer`/`AutoClient`);
/// принятое сообщение будит worker его событием `wake`.
fn enqueue(
    outbox: &Outbox,
    wake: &EventHandle,
    running: &AtomicBool,
    gauge: &ChannelState,
    data: &[u8],
    lossless: bool,
    max_send_queue: usize,
) -> Result<()> {
    if !running.load(Ordering::Acquire) {
        return Err(ShmError::NotReady);
    }
    // Длину проверяем синхронно: иначе ошибка ушла бы в on_error
    // асинхронно, а вызывающий считал бы сообщение принятым.
    gauge.check_len(data.len())?;
    let cap = max_send_queue.max(1);
    let msg = Outgoing {
        data: data.to_vec(),
        lossless,
    };
    let frame = msg.frame();
    {
        let mut queue = outbox.lock();
        if lossless {
            if !gauge.try_acquire(frame, cap) {
                return Err(ShmError::QueueFull);
            }
        } else if queue.len() >= cap {
            // Переполнение: вытесняем самое старое ОБЫЧНОЕ сообщение; если
            // в очереди одни lossless -- выбрасывается само новое.
            let Some(pos) = queue.iter().position(|m| !m.lossless) else {
                outbox.evicted.fetch_add(1, Ordering::AcqRel);
                drop(queue);
                let _ = wake.set();
                return Ok(());
            };
            if let Some(evicted) = queue.remove(pos) {
                gauge.release(evicted.frame());
            }
            outbox.evicted.fetch_add(1, Ordering::AcqRel);
            gauge.acquire(frame);
        } else {
            gauge.acquire(frame);
        }
        queue.push_back(msg);
    }
    // Ошибка сигнала -- только испорченный дескриптор; сообщение всё равно
    // уйдёт при следующем пробуждении worker-а.
    let _ = wake.set();
    Ok(())
}

/// Общая часть `AutoServer`/`AutoClient` на стороне API.
#[derive(Debug)]
struct Endpoint {
    outbox: Arc<Outbox>,
    /// Будит worker: новая команда (`send`/`try_send`/`stop`/Drop).
    wake: Arc<EventHandle>,
    join: Mutex<Option<JoinHandle<()>>>,
    stats: Arc<AutoStats>,
    running: Arc<AtomicBool>,
    gauge: Arc<ChannelState>,
    max_send_queue: usize,
}

/// Всё, что нужно worker-у (общее с `Endpoint`).
struct Worker {
    handler: Arc<dyn AutoHandler>,
    options: AutoOptions,
    outbox: Arc<Outbox>,
    wake: Arc<EventHandle>,
    stats: Arc<AutoStats>,
    running: Arc<AtomicBool>,
    gauge: Arc<ChannelState>,
    /// Исходящее направление этой стороны.
    tx: ChannelKind,
}

impl Endpoint {
    /// Поднять worker-поток: `body` получает контекст и работает, пока
    /// `running`.
    fn spawn(
        handler: Arc<dyn AutoHandler>,
        options: AutoOptions,
        tx: ChannelKind,
        thread_name: impl FnOnce() -> String,
        layout: Option<ChannelLayout>,
        body: impl FnOnce(&Worker) + Send + 'static,
    ) -> Result<Self> {
        let wake = EventHandle::create_unnamed(false).map(Arc::new)?;
        let endpoint = Self {
            outbox: Arc::default(),
            wake,
            join: Mutex::new(None),
            stats: Arc::default(),
            running: Arc::new(AtomicBool::new(true)),
            gauge: Arc::default(),
            max_send_queue: options.max_send_queue,
        };
        if let Some(layout) = layout {
            endpoint.gauge.set_layout(layout, tx);
        }
        let worker = Worker {
            handler,
            options,
            outbox: Arc::clone(&endpoint.outbox),
            wake: Arc::clone(&endpoint.wake),
            stats: Arc::clone(&endpoint.stats),
            running: Arc::clone(&endpoint.running),
            gauge: Arc::clone(&endpoint.gauge),
            tx,
        };
        // Имя потока -- только в debug (короткий непрозрачный тег), в release
        // поток безымянный: имя канала не светится в Process Explorer.
        #[cfg_attr(
            not(debug_assertions),
            expect(unused_mut, reason = "имя -- только в debug")
        )]
        let mut builder = thread::Builder::new();
        #[cfg(debug_assertions)]
        {
            builder = builder.name(thread_name());
        }
        #[cfg(not(debug_assertions))]
        drop(thread_name);
        let join = crate::thread_hook::spawn(builder, move || body(&worker))
            .map_err(|err| map_spawn_error(err, "spawn auto worker"))?;
        *endpoint.join.lock_clean() = Some(join);
        Ok(endpoint)
    }

    fn send(&self, data: &[u8], lossless: bool) -> Result<()> {
        enqueue(
            &self.outbox,
            &self.wake,
            &self.running,
            &self.gauge,
            data,
            lossless,
            self.max_send_queue,
        )
    }

    fn free_space(&self) -> FreeSpace {
        self.gauge.estimate(self.max_send_queue.max(1))
    }

    /// Синхронная остановка: worker дописывает очередь в кольцо (сколько
    /// влезет) и выходит, поток джойнится -- после возврата колбэков нет.
    /// Из собственного колбэка -- без ожидания себя (`join_unless_self`).
    fn stop(&self) {
        // Паника worker-а здесь сообщать некуда: обработчик живёт только в
        // контексте worker-а (`Worker::handler`), а держать его ещё и
        // здесь -- риск цикла `Arc` (прокси Dispatch ссылаются на свои
        // каналы). Текст паники уже напечатал panic hook потребителя;
        // паниковать из `stop()`/`Drop` нельзя. Кому итог нужен (Dispatch,
        // у которого есть свой обработчик), зовёт `stop_joined`.
        let _panicked = self.stop_joined();
    }

    /// `stop`, возвращающий итог join-а: `Err(WorkerPanicked)` -- worker
    /// завершился паникой (колбэк обработчика). Повторный вызов -- `Ok`.
    fn stop_joined(&self) -> Result<()> {
        self.running.store(false, Ordering::Release);
        let _ = self.wake.set();
        let handle = self.join.lock_clean().take();
        handle.map_or(Ok(()), join_unless_self)
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Worker {
    fn notify_disconnect(&self, reason: DisconnectReason) {
        // Сначала статус пира (его видят `is_peer_alive()`), затем колбэк.
        self.gauge.peer_disconnected(reason);
        self.handler.on_disconnect_reason(reason);
    }

    fn running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// `on_error` -- если это не повтор предыдущей ошибки вне подключения
    /// (Task 11, M1: одинаковая ошибка подряд -- один раз).
    fn report_once(&self, reported: &mut Reported, err: ShmError) {
        if reported.first(&err) {
            self.handler.on_error(err);
        }
    }

    /// `on_error` для ошибки маяка присутствия (свой ряд повторов).
    fn report_presence(&self, reported: &mut Reported, err: ShmError) {
        if reported.presence_first(&err) {
            self.handler.on_error(err);
        }
    }

    /// Пауза `delay`, прерываемая остановкой (`wake`); возврат из неё --
    /// пробуждение worker-а. `false` -- остановка.
    fn pause(&self, delay: Duration) -> bool {
        let go_on = wait_delay_or(&self.running, &self.wake, delay);
        self.stats.wake();
        go_on
    }

    /// Резерв (решение R-M): ждать нечего -- пауза `reconnect_delay`, но не
    /// дольше резервной проверки `discovery_fallback` (W(3) ограничивает
    /// каждое ожидание обнаружения). `false` -- остановка.
    fn degraded_pause(&self) -> bool {
        let delay = presence::min_timeout(
            Some(self.options.reconnect_delay),
            self.options.discovery_fallback,
        );
        self.pause(delay.unwrap_or(self.options.reconnect_delay))
    }

    /// Клиент вне подключения после неудачи `failure` (Task 11, R19): сон в
    /// ядре на маяке присутствия канала `name` или на процессе сервера
    /// (`presence::PresenceWatch::wait`), без опроса. `wake` -- индекс 0:
    /// `stop` прерывает сразу, `send` -- отчёт о вытесненных и снова сон.
    /// Ждать нечего (маяк недоступен -- нет прав или привилегии `Global\`,
    /// процесс сервера не открылся, состояние маяка испорчено) -- резерв:
    /// пауза `reconnect_delay` (решение R-M), ошибка маяка -- один раз.
    /// Сон -- не дольше `discovery_fallback` от входа (решение W(3); `send`
    /// дедлайн не продлевает), затем попытка. `false` -- остановка.
    fn await_server(
        &self,
        name: &str,
        failure: Failure,
        presence: &mut Option<PresenceWatch>,
        reported: &mut Reported,
    ) -> bool {
        let deadline = crate::ring::deadline_of(presence::min_timeout(
            self.options.poll_timeout,
            self.options.discovery_fallback,
        ));
        loop {
            if !self.running() {
                return false;
            }
            let left = deadline.map(|at| at.saturating_duration_since(std::time::Instant::now()));
            if left.is_some_and(|left| left.is_zero()) {
                return true;
            }
            if presence.is_none() {
                match PresenceWatch::open(
                    self.options.namespace.as_deref(),
                    self.options.namespace.as_ref(),
                    name,
                    &self.options.security,
                ) {
                    Ok(watch) => *presence = Some(watch),
                    Err(err) => {
                        self.report_presence(reported, err);
                        return self.degraded_pause();
                    }
                }
            }
            let Some(watch) = presence.as_mut() else {
                return self.degraded_pause();
            };
            let waited = watch.wait(failure, Some(self.wake.as_handle()), left, &|| {
                self.stats.wake();
            });
            match waited {
                Ok(PresenceWait::Retry | PresenceWait::TimedOut) => return self.running(),
                Ok(PresenceWait::Woken) => self.report_evicted(),
                Ok(PresenceWait::Unavailable) => {
                    return self.degraded_pause();
                }
                Err(err) => {
                    self.report_presence(reported, err);
                    return self.degraded_pause();
                }
            }
        }
    }

    /// Сервер, конец сессии (Ruling S надёжности): очередь отправки
    /// предназначалась ушедшему клиенту -- сбросить её, а не отдать
    /// следующему. Возвращает число сброшенных; сообщает о них вызывающий
    /// (`report_discarded`) -- вне блокировки, ДО `on_disconnect_reason`:
    /// отчёт относится к закрываемой сессии (у Dispatch -- последний колбэк
    /// клиента перед его отключением). Граница -- момент, когда worker
    /// ОБНАРУЖИЛ конец сессии: `send` после ухода клиента, но до разбора
    /// `DISCONNECT`, считается ещё сообщением ушедшему. Отправленное из
    /// колбэка отключения и позже адресовано следующему клиенту и остаётся.
    fn discard_session_queue(&self) -> u32 {
        let dropped = core::mem::take(&mut *self.outbox.lock());
        for msg in &dropped {
            self.gauge.release(msg.frame());
        }
        u32::try_from(dropped.len()).unwrap_or(u32::MAX)
    }

    /// Один отчёт о сброшенной очереди: счётчик и `on_overflow` -- раз на
    /// конец сессии, не на сообщение.
    fn report_discarded(&self, count: u32) {
        if count > 0 {
            // Relaxed: статистика.
            self.stats
                .discarded_messages
                .fetch_add(u64::from(count), Ordering::Relaxed);
            self.handler.on_overflow(self.tx, count);
        }
    }

    /// Сообщить о сообщениях, вытесненных отправителями из очереди.
    fn report_evicted(&self) {
        let evicted = self.outbox.evicted.swap(0, Ordering::AcqRel);
        if evicted > 0 {
            self.handler.on_overflow(self.tx, evicted);
        }
    }

    /// Пишет очередь в кольцо, пока пишется (см. `process_send_queue`).
    /// `false` -- кольцо испорчено пиром, соединение надо разорвать.
    fn flush<E: SendEndpoint>(&self, endpoint: &E) -> bool {
        self.report_evicted();
        process_send_queue(
            endpoint,
            &self.outbox,
            &self.handler,
            &self.stats,
            &self.gauge,
            self.tx,
        )
    }

    /// Доставить до `recv_batch` сообщений.
    fn receive<R: ReceiveEndpoint>(&self, endpoint: &R, buffer: &mut Vec<u8>) -> ReceiveOutcome {
        process_receive_queue(
            endpoint,
            &self.handler,
            &self.stats,
            buffer,
            self.options.recv_batch,
            self.tx.reverse(),
            self.options.delivery,
        )
    }

    /// Пир отключился или умер -- новых данных не будет, но всё, что он
    /// успел записать до этого, доставляем ДО `on_disconnect_reason`.
    fn drain_remaining<R: ReceiveEndpoint>(&self, endpoint: &R, buffer: &mut Vec<u8>) {
        loop {
            let outcome = self.receive(endpoint, buffer);
            if outcome.fatal || !outcome.more_pending || !self.running() {
                break;
            }
        }
    }

    /// Обработать `SPACE`: сначала дописать очередь в освободившееся место,
    /// потом сообщить -- `try_send` после `on_space_available` находит место
    /// и в очереди.
    fn on_space<E: SendEndpoint>(&self, endpoint: &E) -> bool {
        let ok = self.flush(endpoint);
        if ok {
            self.handler.on_space_available(self.tx);
        }
        ok
    }

    /// Спин перед сном (`AutoOptions::spin`) -- возвращает таймаут
    /// следующего ожидания. Во входящем кольце появились данные, пришла
    /// новая отправка или остановка -- `Some(0)`: ожидание НЕ пропускается,
    /// а только не блокирует, так что `DISCONNECT`, `SPACE`, смерть пира и
    /// `wake` проверяются после КАЖДОГО спина (непрерывный поток данных не
    /// может держать worker вне `wait_any`). Иначе -- обычный
    /// `poll_timeout`.
    ///
    /// Вызывается ровно один раз перед каждым ожиданием в ядре, только у
    /// подключённого worker-а, только когда проход ничего не оставил
    /// несделанным (`more_pending == false`). Поэтому бюджет -- один на
    /// засыпание: worker в простое отспинивает его один раз и спит на
    /// наборе событий без таймаута (ноль пробуждений, как без спина).
    ///
    /// Непустая очередь отправки на входе -- спина нет: её голова либо ждёт
    /// `SPACE` (тогда спин по `pending_msgs` закончился бы сразу и крутил
    /// цикл), либо поставлена колбэком этого прохода -- тогда `wake` уже
    /// взведён и ожидание вернётся сразу. `DATA`, взведённый писателем за
    /// время спина, поглощает нулевое ожидание (или даёт одно лишнее
    /// пробуждение потом); сбрасывать его вручную нельзя -- можно потерять
    /// сигнал следующего кадра.
    fn spin_before_wait(&self, has_unread: impl Fn() -> bool) -> Option<Duration> {
        let spun = self.options.spin.is_enabled()
            && self.outbox.lock().is_empty()
            && spin_until(self.options.spin, None, || {
                has_unread()
                    || self.gauge.pending_msgs.load(Ordering::Acquire) > 0
                    || !self.running()
            });
        if spun {
            Some(Duration::ZERO)
        } else {
            self.options.poll_timeout
        }
    }

    /// Обработать результат ожидания подключённого worker-а. `None` --
    /// соединение живо, `Some(reason)` -- разорвано (уже доставлено всё,
    /// что пир успел записать; колбэк отключения -- за вызывающим).
    fn handle_wait<E: SendEndpoint + ReceiveEndpoint>(
        &self,
        endpoint: &E,
        set: &WaitSet<'_>,
        space: BorrowedHandle<'_>,
        result: Result<Option<usize>>,
        buffer: &mut Vec<u8>,
    ) -> Option<DisconnectReason> {
        match result {
            Ok(Some(DISCONNECT_INDEX)) => {
                // Штатное отключение: всё, что пир успел записать (например,
                // прощальное сообщение), доставляем ДО `on_disconnect`.
                self.drain_remaining(endpoint, buffer);
                Some(DisconnectReason::Graceful)
            }
            Ok(Some(DATA_INDEX)) => {
                // Данные прочитаем на следующем проходе. SPACE (автосброс,
                // больший индекс) мог прийти одновременно: при непрерывном
                // входящем потоке NT отдавал бы DATA каждый раз, и
                // `on_space_available` откладывался бы бесконечно.
                // Lock очереди -- отдельным выражением: `on_space` берёт его снова.
                let queued = !self.outbox.lock().is_empty();
                let space_too =
                    queued && matches!(win::wait_any(&[space], Some(Duration::ZERO)), Ok(Some(0)));
                if space_too && !self.on_space(endpoint) {
                    return Some(DisconnectReason::Error);
                }
                None
            }
            Ok(Some(SPACE_INDEX)) => (!self.on_space(endpoint)).then_some(DisconnectReason::Error),
            Ok(Some(i)) if Some(i) == set.peer => {
                self.drain_remaining(endpoint, buffer);
                Some(DisconnectReason::PeerDied)
            }
            // `wake` (новая команда / остановка) или страховочный таймаут.
            Ok(Some(_) | None) => None,
            Err(err) => {
                self.handler.on_error(err);
                Some(DisconnectReason::Error)
            }
        }
    }
}

impl ChannelKind {
    const fn reverse(self) -> Self {
        match self {
            Self::ServerToClient => Self::ClientToServer,
            Self::ClientToServer => Self::ServerToClient,
        }
    }
}

#[derive(Debug)]
pub struct AutoServer {
    endpoint: Endpoint,
    layout: ChannelLayout,
    /// Маяк присутствия канала на момент старта (решение W(1)).
    presence: crate::PresenceStatus,
}

impl AutoServer {
    /// Поднять канал `name` (раскладка -- `options.layout`) и worker,
    /// который принимает клиентов и обслуживает их по событиям.
    ///
    /// Канал держит маяк присутствия `<имя>_beacon` (0.10, INTEROP R19; см.
    /// `SharedServer::start_with`): `AutoClient`, стартовавший раньше
    /// сервера, спит на нём, а не опрашивает раз в `reconnect_delay`.
    pub fn start(name: &str, handler: Arc<dyn AutoHandler>, options: AutoOptions) -> Result<Self> {
        Self::start_with_presence(name, handler, options, Presence::On)
    }

    /// `start` с выбором маяка присутствия (выделенные каналы Dispatch --
    /// без маяка, R19).
    pub(crate) fn start_with_presence(
        name: &str,
        handler: Arc<dyn AutoHandler>,
        options: AutoOptions,
        presence: Presence,
    ) -> Result<Self> {
        let mut server = SharedServer::start_slot(
            options.namespace.as_ref(),
            name,
            options.layout,
            0,
            &options.security.attributes()?,
            presence,
        )?;
        let layout = *server.layout();
        let status = server.presence();
        let endpoint = Endpoint::spawn(
            handler,
            options,
            ChannelKind::ServerToClient,
            || format!("xsa-{name}"),
            Some(layout),
            move |worker| server_worker(&mut server, worker),
        )?;
        Ok(Self {
            endpoint,
            layout,
            presence: status,
        })
    }

    /// Раскладка канала.
    #[must_use]
    pub const fn layout(&self) -> &ChannelLayout {
        &self.layout
    }

    /// Маяк присутствия канала (0.10, INTEROP R19; см.
    /// `SharedServer::presence`): `Unavailable` -- сервер работает без
    /// маяка, `AutoClient`-ы находят его резервной проверкой
    /// `AutoOptions::discovery_fallback`.
    #[must_use]
    pub fn presence(&self) -> crate::PresenceStatus {
        self.presence.clone()
    }

    /// Асинхронная отправка с политикой overwrite-oldest: при переполнении
    /// очереди вытесняется самое старое обычное сообщение, при переполнении
    /// кольца -- самые старые непрочитанные (`on_overflow`).
    /// `Err(MessageTooSmall | MessageTooLarge)` -- длина вне
    /// `MIN_MESSAGE_SIZE..=layout.server_to_client.max_message_size`
    /// (проверяется синхронно); `Err(NotReady)` -- сервер остановлен.
    pub fn send(&self, data: &[u8]) -> Result<()> {
        self.endpoint.send(data, false)
    }

    /// Асинхронная отправка **без потерь** (backpressure).
    ///
    /// - `Ok(())` -- сообщение принято и будет записано в кольцо
    ///   `try_write_message`-ом, то есть без перезаписи чего-либо; из очереди
    ///   оно не вытесняется (ни `send`, ни `try_send`), порядок -- FIFO вместе
    ///   с остальными сообщениями канала;
    /// - `Err(QueueFull)` -- в очереди уже `max_send_queue` непереданных
    ///   сообщений; ничего не принято, повторите позже (`free_space`,
    ///   `on_space_available`);
    /// - `Err(MessageTooSmall | MessageTooLarge)` -- проверяется синхронно;
    /// - `Err(NotReady)` -- сервер остановлен.
    ///
    /// Гарантия «без потерь» действует в пределах одного подключения: при
    /// переподключении handshake сбрасывает кольца, и уже записанные, но не
    /// прочитанные сообщения пропадают. Неотправленные к концу сессии (в
    /// очереди) сообщения предназначались ушедшему клиенту и следующему не
    /// достаются (0.10): worker сбрасывает очередь и сообщает об этом один
    /// раз, перед `on_disconnect_reason`, -- `on_overflow(ServerToClient, n)`
    /// и `AutoStatsSnapshot::discarded_messages`. Граница -- момент, когда
    /// worker обнаружил конец сессии: принятое после ухода клиента, но до
    /// разбора его `DISCONNECT`, тоже сбрасывается. Принятое позже (из
    /// колбэка отключения, между сессиями, до первого клиента) уходит
    /// следующему клиенту.
    /// Смешивать с `send` можно, но сообщения `send`
    /// пишутся с перезаписью и могут вытеснить из КОЛЬЦА ранее записанные
    /// сообщения `try_send` -- для строго lossless-канала используйте только
    /// `try_send`.
    pub fn try_send(&self, data: &[u8]) -> Result<()> {
        self.endpoint.send(data, true)
    }

    /// Консервативная оценка места под новое сообщение: свободное место
    /// кольца Server -> Client (снимок worker-а) минус всё принятое, но ещё
    /// не записанное; `messages` дополнительно ограничено свободным местом
    /// очереди. Если `free_space().fits(n)`, то `try_send` payload-а длины
    /// `n` будет принят и записан в кольцо без ожидания читателя (при одном
    /// отправляющем потоке). Вне подключения -- `FreeSpace::ZERO`.
    ///
    /// Это оценка, а не гарантия: при нескольких параллельных отправителях
    /// она может устареть или (поля снимка читаются раздельно) кратко
    /// завысить место.
    #[must_use]
    pub fn free_space(&self) -> FreeSpace {
        self.endpoint.free_space()
    }

    /// Жив ли процесс пира по данным worker-а: `Some(true)` -- подключён и
    /// наблюдается через удерживаемый handle; `Some(false)` -- последнее
    /// отключение было `PeerDied` (до следующего подключения); `None` -- нет
    /// подключения или пир не наблюдается (нет прав).
    #[must_use]
    pub fn is_peer_alive(&self) -> Option<bool> {
        self.endpoint.gauge.peer_alive()
    }

    /// PID наблюдаемого пира (после `PeerDied` -- PID умершего процесса).
    #[must_use]
    pub fn peer_pid(&self) -> Option<u32> {
        self.endpoint.gauge.peer_pid()
    }

    /// PID текущего клиента, сверенный по ненулевому времени создания
    /// процесса (`SharedServer::verified_peer_pid`); публикуется worker-ом
    /// до `on_connect`. Сверка самоописания, не аутентификация.
    pub(crate) fn verified_peer_pid(&self) -> Option<u32> {
        Some(
            self.endpoint
                .gauge
                .peer_verified_pid
                .load(Ordering::Acquire),
        )
        .filter(|&p| p != 0)
    }

    /// Остановить сервер синхронно: очередь дописывается в кольцо (сколько
    /// влезет), клиент получает `DISCONNECT`, worker-поток джойнится -- после
    /// возврата колбэков нет. Повторный вызов и `Drop` после него -- no-op.
    pub fn stop(&self) {
        self.endpoint.stop();
    }

    /// `stop` с итогом join-а worker-а: `Err(WorkerPanicked)` -- worker
    /// завершился паникой (колбэк обработчика). Dispatch сообщает её в
    /// `on_error` своего обработчика с id клиента.
    pub(crate) fn stop_joined(&self) -> Result<()> {
        self.endpoint.stop_joined()
    }

    #[must_use]
    pub fn stats(&self) -> AutoStatsSnapshot {
        self.endpoint.stats.snapshot()
    }

    /// Число пробуждений worker-а (тесты: ноль в простое).
    #[cfg(test)]
    pub(crate) fn wakeups(&self) -> u64 {
        self.endpoint.stats.wakeups.load(Ordering::Relaxed)
    }
}

fn server_worker(server: &mut SharedServer, worker: &Worker) {
    let mut buffer = Vec::new();
    // Anonymous-сервер не имеет событий, а auto-mode построен на ожидании
    // событий. Недостижимо: AutoServer::start всегда поднимает named-сервер.
    if server.events().is_none() {
        worker.handler.on_error(ShmError::InvalidConfig(
            "anonymous server is not supported in auto-mode",
        ));
        return;
    }
    let gauge = &worker.gauge;
    let mut connected = false;

    while worker.running() {
        if !connected {
            // Вне подключения писать некуда: оценка места -- ноль.
            gauge.publish_ring(FreeSpace::ZERO);
            worker.report_evicted();
            // Ждём клиента ИЛИ команду/остановку (событие `wake`) -- без
            // опроса по таймеру.
            let accepted =
                server.wait_for_client_or(worker.wake.as_handle(), worker.options.poll_timeout);
            worker.stats.wake();
            match accepted {
                Ok(true) => {
                    connected = true;
                    gauge.peer_connected(server.peer_pid());
                    gauge
                        .peer_verified_pid
                        .store(server.verified_peer_pid().unwrap_or(0), Ordering::Release);
                    worker.handler.on_connect();
                }
                Ok(false) => continue,
                Err(err) => {
                    worker.handler.on_error(err);
                    continue;
                }
            }
        }

        let reason = 'session: {
            if !worker.flush(server) {
                break 'session Some(DisconnectReason::Error);
            }
            let outcome = worker.receive(server, &mut buffer);
            if outcome.fatal {
                break 'session Some(DisconnectReason::Error);
            }
            if outcome.more_pending || !worker.running() {
                // Ещё есть данные (или остановка) -- не блокируемся.
                break 'session None;
            }
            // Спин -- один бюджет на засыпание (см. `spin_before_wait`).
            let timeout = worker.spin_before_wait(|| server.has_unread_from_client());
            // Набор -- заимствование событий сервера на одно ожидание
            // (`wait_for_client_or`/`mark_disconnected` требуют `&mut`).
            let Some(events) = server.events() else {
                // Недостижимо: события проверены на входе в worker.
                break 'session Some(DisconnectReason::Error);
            };
            let space = events.s2c.space.as_handle();
            let set = WaitSet::new(
                [
                    events.disconnect.as_handle(),
                    events.c2s.data.as_handle(),
                    space,
                ],
                server.peer_wait_handle(),
                worker.wake.as_handle(),
            );
            let result = win::wait_any(set.handles(), timeout);
            worker.stats.wake();
            worker.handle_wait(server, &set, space, result, &mut buffer)
        };
        if let Some(reason) = reason {
            // Неотправленное ушедшему клиенту следующему не достаётся
            // (Ruling S): забрать и сообщить -- до колбэка отключения.
            let discarded = worker.discard_session_queue();
            worker.report_discarded(discarded);
            worker.notify_disconnect(reason);
            if reason == DisconnectReason::Error {
                // Разрыв по нашей ошибке (порча кольца, отказ ожидания):
                // клиент ещё в сессии -- `IDLE`, потом `DISCONNECT`
                // (final review I2; так же `force_disconnect` Multi).
                server.disconnect_local();
            } else {
                // Клиент ушёл сам или умер -- сигналить некому.
                server.mark_disconnected();
            }
            connected = false;
        }
    }

    // Остановка: принятое до `stop()` (например, прощальное сообщение)
    // дописываем в кольцо, сколько влезет, -- клиент дочитает его до
    // события отключения.
    if connected {
        let _ = worker.flush(server);
    }
}

/// Индексы событий канала в наборе ожидания worker-а. NT при одновременном
/// сигнале возвращает наименьший индекс: штатный DISCONNECT и данные имеют
/// приоритет над «процесс пира завершился», а тот -- над `wake`.
const DISCONNECT_INDEX: usize = 0;
const DATA_INDEX: usize = 1;
const SPACE_INDEX: usize = 2;

/// Набор ожидания worker-а: DISCONNECT, DATA, SPACE, handle процесса пира
/// (если наблюдается), `wake` (последним).
struct WaitSet<'a> {
    /// Хвост за `count` -- заполнитель (копия `wake`), в ожидание не уходит.
    handles: [BorrowedHandle<'a>; 5],
    count: usize,
    /// Индекс handle процесса пира.
    peer: Option<usize>,
}

impl<'a> WaitSet<'a> {
    const fn new(
        base: [BorrowedHandle<'a>; 3],
        peer: Option<BorrowedHandle<'a>>,
        wake: BorrowedHandle<'a>,
    ) -> Self {
        match peer {
            Some(peer) => Self {
                handles: [base[0], base[1], base[2], peer, wake],
                count: 5,
                peer: Some(3),
            },
            None => Self {
                handles: [base[0], base[1], base[2], wake, wake],
                count: 4,
                peer: None,
            },
        }
    }

    fn handles(&self) -> &[BorrowedHandle<'a>] {
        &self.handles[..self.count]
    }
}

#[derive(Debug)]
pub struct AutoClient {
    endpoint: Endpoint,
}

impl AutoClient {
    /// Клиент канала `name`: worker подключается и переподключается сам
    /// (сервера нет -- ждёт его маяк присутствия без опроса, см.
    /// `AutoOptions::reconnect_delay`), раскладку канала берёт из сегмента.
    ///
    /// Канал занят другим клиентом -- `on_error(AlreadyConnected)` на каждой
    /// попытке, с отступом, удваивающимся от `reconnect_delay` до 2 с, пока
    /// канал не освободится (см. `AutoOptions::reconnect_delay`).
    pub fn connect(
        name: &str,
        handler: Arc<dyn AutoHandler>,
        options: AutoOptions,
    ) -> Result<Self> {
        Self::connect_with(name, handler, options, ClientMode::Reconnect)
    }

    /// Клиент выделенного канала Dispatch (ревизия 2): не переподключается.
    /// Первое подключение не удалось или канал разорван -- `on_error` (для
    /// неудачного подключения), `on_disconnect_reason(..)` и выход worker-а,
    /// без повторов к каналу, которого больше нет. PID сервера из
    /// рукопожатия публикуется в `server_pid` и НЕ стирается при разрыве.
    pub(crate) fn connect_dedicated(
        name: &str,
        handler: Arc<dyn AutoHandler>,
        options: AutoOptions,
        server_pid: Arc<AtomicU32>,
    ) -> Result<Self> {
        Self::connect_with(name, handler, options, ClientMode::Dedicated(server_pid))
    }

    fn connect_with(
        name: &str,
        handler: Arc<dyn AutoHandler>,
        options: AutoOptions,
        mode: ClientMode,
    ) -> Result<Self> {
        presence::validate_fallback(options.discovery_fallback)?;
        let name_owned = name.to_owned();
        let endpoint = Endpoint::spawn(
            handler,
            options,
            ChannelKind::ClientToServer,
            || format!("xsc-{name}"),
            None,
            move |worker| client_worker(&name_owned, worker, &mode),
        )?;
        Ok(Self { endpoint })
    }

    /// Раскладка канала текущего (или последнего) подключения; `None` --
    /// ещё ни разу не подключался.
    #[must_use]
    pub fn layout(&self) -> Option<ChannelLayout> {
        self.endpoint.gauge.layout()
    }

    /// Асинхронная отправка с политикой overwrite-oldest
    /// (см. `AutoServer::send`). Длина проверяется против лимита кольца
    /// Client -> Server, когда он известен (после первого подключения).
    pub fn send(&self, data: &[u8]) -> Result<()> {
        self.endpoint.send(data, false)
    }

    /// Асинхронная отправка без потерь с backpressure (`Err(QueueFull)`,
    /// когда в очереди `max_send_queue` непереданных сообщений). Полная
    /// семантика -- `AutoServer::try_send`.
    pub fn try_send(&self, data: &[u8]) -> Result<()> {
        self.endpoint.send(data, true)
    }

    /// Консервативная оценка места под новое сообщение в направлении
    /// Client -> Server (см. `AutoServer::free_space`). Оценка: при
    /// параллельных отправителях может устареть.
    #[must_use]
    pub fn free_space(&self) -> FreeSpace {
        self.endpoint.free_space()
    }

    /// Жив ли процесс пира (см. `AutoServer::is_peer_alive`).
    #[must_use]
    pub fn is_peer_alive(&self) -> Option<bool> {
        self.endpoint.gauge.peer_alive()
    }

    /// PID наблюдаемого пира (после `PeerDied` -- PID умершего процесса).
    #[must_use]
    pub fn peer_pid(&self) -> Option<u32> {
        self.endpoint.gauge.peer_pid()
    }

    /// Остановить клиента синхронно (см. `AutoServer::stop`): сервер
    /// получает `DISCONNECT` после того, как очередь дописана в кольцо.
    pub fn stop(&self) {
        self.endpoint.stop();
    }

    #[must_use]
    pub fn stats(&self) -> AutoStatsSnapshot {
        self.endpoint.stats.snapshot()
    }

    /// Число пробуждений worker-а (тесты: ноль в простое).
    #[cfg(test)]
    pub(crate) fn wakeups(&self) -> u64 {
        self.endpoint.stats.wakeups.load(Ordering::Relaxed)
    }

    /// Число попыток подключения (тесты: нет горячего цикла попыток).
    #[cfg(test)]
    pub(crate) fn attempts(&self) -> u64 {
        self.endpoint.stats.attempts.load(Ordering::Relaxed)
    }
}

/// Режим клиентского worker-а.
enum ClientMode {
    /// `AutoClient::connect`: переподключаться (сервера нет -- ждать его
    /// маяк присутствия, Task 11).
    Reconnect,
    /// Выделенный канал Dispatch: одно подключение; PID сервера -- сюда.
    Dedicated(Arc<AtomicU32>),
}

/// Предел отступа `AutoClient` при занятом канале (`AlreadyConnected`):
/// пауза удваивается от `reconnect_delay` до этого значения (или остаётся
/// `reconnect_delay`, если он сам больше).
pub(crate) const BUSY_RECONNECT_CAP: Duration = Duration::from_secs(2);

/// Отступ повторов при занятом канале (final review M4): `on_error(
/// AlreadyConnected)` -- не 4 раза в секунду бесконечно, а с удвоением паузы
/// до `BUSY_RECONNECT_CAP`. Первый повтор -- через `reconnect_delay`, как
/// раньше (держатель, ушедший сразу, не задерживает нас). Любой другой исход
/// попытки сбрасывает отступ.
#[derive(Default)]
struct BusyBackoff {
    /// Пауза перед следующим повтором; `None` -- серии отказов нет.
    current: Option<Duration>,
}

impl BusyBackoff {
    /// Пауза после неудачной попытки (`busy` -- отказ `AlreadyConnected`).
    fn next(&mut self, busy: bool, base: Duration) -> Duration {
        if !busy {
            self.reset();
            return base;
        }
        let delay = self.current.unwrap_or(base);
        self.current = Some(delay.saturating_mul(2).min(BUSY_RECONNECT_CAP.max(base)));
        delay
    }

    const fn reset(&mut self) {
        self.current = None;
    }
}

/// Тесты (Task 6 надёжности): точка прохода worker-а клиента Auto, где
/// срабатывает одноразовый хук канала (`WORKER_HOOKS`).
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkerPoint {
    /// После сверки сессии в начале прохода, до записи очереди (`flush`).
    BeforeFlush,
}

/// Тесты: одноразовый хук точки прохода.
#[cfg(test)]
pub(crate) type WorkerHook = Box<dyn FnOnce() + Send>;

/// Тесты: хуки worker-ов клиента Auto -- (имя канала, точка, хук). Хук
/// играет роль сервера и другого клиента, успевших сменить сессию ровно в
/// этом окне прохода.
#[cfg(test)]
pub(crate) static WORKER_HOOKS: Mutex<Vec<(String, WorkerPoint, WorkerHook)>> =
    Mutex::new(Vec::new());

/// Тесты: вызвать хук канала `name` в точке `point`, если он задан.
/// Блокировка снимается ДО вызова хука.
#[cfg(test)]
fn run_worker_hook(name: &str, point: WorkerPoint) {
    let hook = {
        let mut hooks = WORKER_HOOKS.lock_clean();
        hooks
            .iter()
            .position(|(channel, at, _)| channel == name && *at == point)
            .map(|index| hooks.swap_remove(index).2)
    };
    if let Some(hook) = hook {
        hook();
    }
}

fn client_worker(name: &str, worker: &Worker, mode: &ClientMode) {
    let mut buffer = Vec::new();
    let gauge = &worker.gauge;
    let dedicated = matches!(mode, ClientMode::Dedicated(_));
    let mut busy_backoff = BusyBackoff::default();
    // Маяк присутствия сервера (Task 11): открывается при первой нужде,
    // один раз на жизнь worker-а.
    let mut presence: Option<PresenceWatch> = None;
    let mut reported = Reported::default();

    while worker.running() {
        // Ревизия 2: ожидание `S2C_CONNECT` прерывается `wake` -- `stop`/Drop
        // не ждут `connect_timeout`; `send` ожидание не прерывает.
        let mut should_stop = || !worker.running();
        #[cfg(test)]
        worker.stats.attempts.fetch_add(1, Ordering::Relaxed);
        let connected = SharedClient::connect_interruptible(
            worker.options.namespace.as_deref(),
            name,
            worker.options.connect_timeout,
            // Ревизия 3: канал Auto (и выделенный канал Dispatch) -- свой
            // захват claim; занят -- `AlreadyConnected`, повтор с
            // отступом `BusyBackoff` (выделенный -- без повтора).
            ClaimMode::Take,
            Interrupt {
                wake: worker.wake.as_handle(),
                should_stop: &mut should_stop,
            },
        );
        let mut client = match connected {
            Ok(client) => client,
            Err(_) if !worker.running() => break,
            Err(err) if dedicated => {
                // Выделенный канал не поднялся -- повторять не к чему
                // (сервер его уже снёс или не дождался нас).
                worker.handler.on_error(err);
                worker.notify_disconnect(DisconnectReason::Error);
                break;
            }
            Err(err) => {
                // Task 11 (R19): сервера нет или он отверг нас -- сон в ядре
                // на маяке присутствия / процессе сервера (не дольше
                // `discovery_fallback`), одинаковая ошибка подряд -- один раз.
                if let Some(failure) =
                    presence::classify(&err, worker.options.namespace.as_deref(), Some(name))
                {
                    busy_backoff.reset();
                    worker.report_once(&mut reported, err);
                    worker.report_evicted();
                    if !worker.await_server(name, failure, &mut presence, &mut reported) {
                        break;
                    }
                    continue;
                }
                // Канал занят (`AlreadyConnected`, до Task 12) -- о каждой
                // попытке, отступ растёт (final review M4). Таймаут ответа
                // сервера и прочее -- прежняя пауза `reconnect_delay`
                // (решение R-M), одинаковая ошибка подряд -- один раз.
                let busy = err == ShmError::AlreadyConnected;
                if busy {
                    reported.note(&err);
                    worker.handler.on_error(err);
                } else {
                    worker.report_once(&mut reported, err);
                }
                worker.report_evicted();
                // Отступ перед новой попыткой; остановка прерывает его сразу
                // (событие `wake`), без сна-циклов.
                let delay = busy_backoff.next(busy, worker.options.reconnect_delay);
                if !worker.pause(delay) {
                    break;
                }
                continue;
            }
        };
        busy_backoff.reset();
        reported.clear();

        gauge.set_layout(*client.layout(), worker.tx);
        gauge.peer_connected(client.peer_pid());
        if let ClientMode::Dedicated(server_pid) = mode {
            server_pid.store(client.peer_pid().unwrap_or(0), Ordering::Release);
        }
        worker.handler.on_connect();
        let client_events = client.events();
        let space = client_events.c2s.space.as_handle();
        let base_handles = [
            client_events.disconnect.as_handle(),
            client_events.s2c.data.as_handle(),
            space,
        ];

        // События набора по индексам -- вернуть сигнал, потреблённый
        // ожиданием уже чужой сессии (см. ниже).
        let base_events = [
            &client_events.disconnect,
            &client_events.s2c.data,
            &client_events.c2s.space,
        ];

        let reason = loop {
            // Final review M1: сессия уже чужая -- сервер отнял claim
            // «не заметившего» клиента (дедлайн по толчку) и принял другого.
            // Ни записи в кольцо, ни ответного `DISCONNECT`, ни дочитывания:
            // кольца принадлежат новой сессии (так же MultiClient).
            if !client.is_session_current() {
                break Some(DisconnectReason::Graceful);
            }
            #[cfg(test)]
            run_worker_hook(name, WorkerPoint::BeforeFlush);
            if !worker.running() {
                // Остановка: принятое до `stop()` (например, прощальное
                // сообщение) дописываем в кольцо, сколько влезет, -- сервер
                // дочитает его до события отключения (Drop клиента).
                let _ = worker.flush(&client);
                break None;
            }
            if !worker.flush(&client) {
                break Some(DisconnectReason::Error);
            }
            let outcome = worker.receive(&client, &mut buffer);
            // Task 6 надёжности (п. 4): сессия сменилась посреди прохода
            // (после сверки выше) -- запись и чтение ответили
            // `NotConnected` молча (сообщение вернулось в голову очереди),
            // решение -- сразу, а не на следующем пробуждении, которого при
            // чужой сессии может не быть до её собственного сигнала.
            if !client.is_session_current() {
                break Some(DisconnectReason::Graceful);
            }
            if outcome.fatal {
                break Some(DisconnectReason::Error);
            }
            if outcome.more_pending {
                continue;
            }
            // Спин -- один бюджет на засыпание (см. `spin_before_wait`).
            let timeout = worker.spin_before_wait(|| client.has_unread_from_server());
            let set = WaitSet::new(
                base_handles,
                client.peer_wait_handle(),
                worker.wake.as_handle(),
            );
            let result = win::wait_any(set.handles(), timeout);
            worker.stats.wake();
            if !client.is_session_current() {
                // Сигнал (автосброс) был для новой сессии -- возвращаем.
                if let Ok(Some(index)) = result
                    && let Some(event) = base_events.get(index)
                {
                    let _ = event.set();
                }
                break Some(DisconnectReason::Graceful);
            }
            if let Some(reason) = worker.handle_wait(&client, &set, space, result, &mut buffer) {
                break Some(reason);
            }
        };
        if let Some(reason) = reason {
            worker.notify_disconnect(reason);
            // `Error` -- полный уход L в `Drop` ниже (final review I1).
            client.end_session(reason);
        }

        // Соединение потеряно: писать некуда, оценка места -- ноль.
        gauge.publish_ring(FreeSpace::ZERO);
        drop(client);
        if dedicated || !worker.running() {
            break;
        }
        // Пауза после конца сессии -- разовая реакция (иначе клиент,
        // отключённый сервером, возвращался бы мгновенно). Сервер умер --
        // паузы нет: возвращаться не к кому, следующая попытка уводит в сон
        // на маяке присутствия (Task 11), и новый сервер принимается сразу.
        if reason != Some(DisconnectReason::PeerDied)
            && !worker.pause(worker.options.reconnect_delay)
        {
            break;
        }
    }
}

/// Пишет очередь в кольцо, пока пишется. `false` -- кольцо испорчено
/// (`Corrupted`): соединение надо разорвать.
///
/// Обычные сообщения -- `write_message` (overwrite-oldest), lossless --
/// `try_write_message`. Если lossless-сообщение в голове очереди не влезает,
/// worker оставляет в кольце заявку (`arm_space_waiter`) и ОДИН раз
/// перепроверяет -- после этого либо запись прошла, либо читатель гарантированно
/// увидит заявку и просигналит `SPACE`, который будит `wait_any` worker-а.
/// Порядок FIFO сохраняется: пока голова ждёт места, остальные тоже ждут.
fn process_send_queue<E>(
    endpoint: &E,
    outbox: &Outbox,
    handler: &Arc<dyn AutoHandler>,
    stats: &Arc<AutoStats>,
    gauge: &ChannelState,
    direction: ChannelKind,
) -> bool
where
    E: SendEndpoint,
{
    // `armed` -- заявка поставлена в ЭТОМ проходе; `cleared` -- заявка,
    // возможно оставшаяся с прошлого прохода (читатель её не снял: места
    // было мало), уже снята. Снимаем на первой же успешной записи -- одна
    // запись в заголовок за проход.
    let mut armed = false;
    let mut cleared = false;
    let mut healthy = true;
    while let Some(msg) = outbox.pop_front() {
        let result = if msg.lossless {
            endpoint.try_write(&msg.data)
        } else {
            endpoint.write(&msg.data)
        };
        match result {
            Ok(outcome) => {
                if armed || !cleared {
                    endpoint.disarm_space_waiter();
                    armed = false;
                    cleared = true;
                }
                // Сначала снимок кольца (уже с этим сообщением), потом
                // списание из pending -- порядок описан у `ChannelState`.
                gauge.publish_ring(endpoint.free_space());
                gauge.release(msg.frame());
                // Relaxed: статистика.
                stats.sent_messages.fetch_add(1, Ordering::Relaxed);
                if outcome.overwritten > 0 {
                    stats
                        .send_overflows
                        .fetch_add(u64::from(outcome.overwritten), Ordering::Relaxed);
                    handler.on_overflow(direction, outcome.overwritten);
                }
            }
            Err(ShmError::QueueFull) if msg.lossless && !armed => {
                endpoint.arm_space_waiter(msg.data.len());
                armed = true;
                outbox.push_front(msg);
            }
            Err(ShmError::QueueFull) => {
                outbox.push_front(msg);
                break;
            }
            Err(ShmError::NotConnected) => {
                // Task 6 надёжности: пир ушёл или сессия уже чужая -- не
                // ошибка канала, а конец сессии, о котором worker решит сам
                // (`DISCONNECT`/смерть пира/сверка `generation`), поэтому
                // без `on_error` (обе стороны). Сообщение -- обратно в
                // голову очереди, а дальше по стороне: клиент отправит его
                // в следующую сессию с тем же сервером (семантика
                // переподключения), сервер в конце сессии очередь
                // сбрасывает (`Worker::discard_session_queue`: она
                // предназначалась ушедшему клиенту, а не следующему).
                outbox.push_front(msg);
                break;
            }
            Err(err @ (ShmError::MessageTooSmall | ShmError::MessageTooLarge)) => {
                // Сообщение невалидно для этого кольца навсегда (клиент
                // принял его до того, как узнал лимит канала) -- не
                // блокировать им очередь.
                handler.on_error(err);
                gauge.release(msg.frame());
            }
            Err(err) => {
                healthy = !matches!(err, ShmError::Corrupted);
                handler.on_error(err);
                outbox.push_front(msg);
                break;
            }
        }
    }
    gauge.publish_ring(endpoint.free_space());
    healthy
}

/// Результат обработки приёмной очереди за один проход.
struct ReceiveOutcome {
    /// Фатальная ошибка — соединение надо сбросить.
    fatal: bool,
    /// Батч упёрся в лимит, в кольце вероятно ещё есть данные — не спать.
    more_pending: bool,
}

/// Обрабатывает до `batch` сообщений за вызов.
///
/// `delivery` -- копия в `buffer` (по умолчанию) или срез прямо из кольца
/// (`Delivery::zero_copy`, контракт подтверждён вызывающим `unsafe`).
fn process_receive_queue<R>(
    endpoint: &R,
    handler: &Arc<dyn AutoHandler>,
    stats: &Arc<AutoStats>,
    buffer: &mut Vec<u8>,
    batch: usize,
    direction: ChannelKind,
    delivery: Delivery,
) -> ReceiveOutcome
where
    R: ReceiveEndpoint,
{
    let deliver = |data: &[u8]| {
        // Relaxed: статистика.
        stats.received_messages.fetch_add(1, Ordering::Relaxed);
        handler.on_message(direction, data);
    };
    for _ in 0..batch.max(1) {
        let received = if delivery.zero_copy {
            // SAFETY: (1) неизменность кадра: `Delivery::zero_copy`
            // создаётся только через `unsafe` (`AutoOptions::zero_copy_delivery`
            // /`Delivery::zero_copy`), его контракт -- пир не меняет кадр, пока
            // идёт `on_message`. (2) единственный читатель: `SharedServer`/
            // `SharedClient` worker создаёт сам и не отдаёт из своего потока;
            // публичного приёма у Auto нет, а `send`/`try_send` пользователя
            // только кладут в очередь (`Outbox`) -- в кольцо пишет и читает
            // его один этот worker, последовательно, не изнутри `on_message`.
            unsafe { endpoint.read_with(deliver) }
        } else {
            endpoint.read(buffer).map(|len| deliver(&buffer[..len]))
        };
        match received {
            Ok(()) => {}
            Err(err @ ShmError::Overrun) => {
                // Писатель с перезаписью вытеснил кадр во время
                // `on_message` (нарушение контракта zero-copy): сообщение
                // потеряно, кольцо читается дальше -- спать с данными в
                // кольце нельзя (новый `DATA` писатель может не дать).
                handler.on_error(err);
            }
            Err(
                ShmError::QueueEmpty
                | ShmError::NotConnected
                | ShmError::NotReady
                | ShmError::Timeout,
            ) => {
                return ReceiveOutcome {
                    fatal: false,
                    more_pending: false,
                };
            }
            Err(err @ ShmError::Corrupted) => {
                handler.on_error(err);
                return ReceiveOutcome {
                    fatal: true,
                    more_pending: false,
                };
            }
            Err(err) => {
                handler.on_error(err);
                return ReceiveOutcome {
                    fatal: false,
                    more_pending: false,
                };
            }
        }
    }
    ReceiveOutcome {
        fatal: false,
        more_pending: true,
    }
}

trait SendEndpoint {
    fn write(&self, data: &[u8]) -> Result<WriteOutcome>;
    fn try_write(&self, data: &[u8]) -> Result<WriteOutcome>;
    fn free_space(&self) -> FreeSpace;
    /// Заявка на место под payload длины `payload_len` (PAD учитывает
    /// кольцо по своей позиции записи).
    fn arm_space_waiter(&self, payload_len: usize);
    fn disarm_space_waiter(&self);
}

trait ReceiveEndpoint {
    fn read(&self, buffer: &mut Vec<u8>) -> Result<usize>;
    /// Zero-copy чтение (`receive_with_shared`).
    ///
    /// # Safety
    /// Контракт `RingBuffer::read_with`: пир не меняет кадр, пока идёт `f`,
    /// и никакого другого чтения этого направления во время `f`.
    unsafe fn read_with<T>(&self, f: impl FnOnce(&[u8]) -> T) -> Result<T>;
}

impl SendEndpoint for SharedServer {
    fn write(&self, data: &[u8]) -> Result<WriteOutcome> {
        self.send_to_client(data)
    }
    fn try_write(&self, data: &[u8]) -> Result<WriteOutcome> {
        self.try_send_to_client(data)
    }
    fn free_space(&self) -> FreeSpace {
        SharedServer::free_space(self)
    }
    fn arm_space_waiter(&self, payload_len: usize) {
        SharedServer::arm_space_waiter(self, payload_len);
    }
    fn disarm_space_waiter(&self) {
        SharedServer::disarm_space_waiter(self);
    }
}

impl ReceiveEndpoint for SharedServer {
    fn read(&self, buffer: &mut Vec<u8>) -> Result<usize> {
        self.receive_from_client(buffer)
    }
    unsafe fn read_with<T>(&self, f: impl FnOnce(&[u8]) -> T) -> Result<T> {
        // SAFETY: контракт `receive_with_shared` -- контракт этой функции.
        unsafe { self.receive_with_shared(f) }
    }
}

impl SendEndpoint for SharedClient {
    fn write(&self, data: &[u8]) -> Result<WriteOutcome> {
        self.send_to_server(data)
    }
    fn try_write(&self, data: &[u8]) -> Result<WriteOutcome> {
        self.try_send_to_server(data)
    }
    fn free_space(&self) -> FreeSpace {
        SharedClient::free_space(self)
    }
    fn arm_space_waiter(&self, payload_len: usize) {
        SharedClient::arm_space_waiter(self, payload_len);
    }
    fn disarm_space_waiter(&self) {
        SharedClient::disarm_space_waiter(self);
    }
}

impl ReceiveEndpoint for SharedClient {
    fn read(&self, buffer: &mut Vec<u8>) -> Result<usize> {
        self.receive_from_server(buffer)
    }
    unsafe fn read_with<T>(&self, f: impl FnOnce(&[u8]) -> T) -> Result<T> {
        // SAFETY: контракт `receive_with_shared` -- контракт этой функции.
        unsafe { self.receive_with_shared(f) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Handler, который в `on_disconnect` дропает контейнер, содержащий сам
    /// `AutoServer` -- воспроизводит паттерн, который вызвал self-join
    /// deadlock в `dispatch/` (аудит 2026-07-10, до фикса): callback
    /// вызывается СИНХРОННО из собственного worker-потока `AutoServer`, и
    /// если внутри него этот же `AutoServer` дропается, `Drop::drop` пытается
    /// заджойнить `worker_handle`, которым и является текущий поток.
    struct SelfDroppingHandler {
        container: Arc<Mutex<Option<AutoServer>>>,
        disconnect_returned: Arc<AtomicBool>,
    }

    impl AutoHandler for SelfDroppingHandler {
        fn on_disconnect(&self) {
            let taken = self.container.lock().unwrap().take();
            drop(taken); // именно тут раньше был self-join deadlock
            self.disconnect_returned.store(true, Ordering::Release);
        }
    }

    struct NoopHandler;
    impl AutoHandler for NoopHandler {}

    /// Регрессия (аудит 2026-07-10): `Drop for AutoServer`/`AutoClient`
    /// раньше безусловно джойнил `worker_handle` -- если Drop вызывается ИЗ
    /// СОБСТВЕННОГО worker-потока (пользовательский `AutoHandler` синхронно
    /// роняет `AutoServer` внутри своего же callback'а), это self-join
    /// deadlock. `auto` -- публичный модуль, доступный любому внешнему
    /// потребителю библиотеки, поэтому фикс должен быть в самом `Drop`, а не
    /// полагаться на то, что каждый вызывающий код (как `dispatch/`) сам
    /// не наступит на эти грабли.
    #[test]
    fn drop_from_own_worker_callback_does_not_self_join_deadlock() {
        let name = format!("TEST_AUTO_SELFDROP_{}", std::process::id());
        let container: Arc<Mutex<Option<AutoServer>>> = Arc::new(Mutex::new(None));
        let disconnect_returned = Arc::new(AtomicBool::new(false));

        let handler = Arc::new(SelfDroppingHandler {
            container: container.clone(),
            disconnect_returned: disconnect_returned.clone(),
        });

        let server = AutoServer::start(&name, handler, AutoOptions::default()).expect("start");
        *container.lock().unwrap() = Some(server);

        let client = AutoClient::connect(&name, Arc::new(NoopHandler), AutoOptions::default())
            .expect("client connect");

        // Дождаться реального подключения (сервер принял клиента), прежде
        // чем отключать: с ревизии 2 `stop()` во время рукопожатия отзывает
        // заявку (соединения и `on_disconnect` тогда нет), а фиксированная
        // пауза под нагрузкой полного прогона не гарантирует подключения.
        let start = std::time::Instant::now();
        while container
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|s| s.is_peer_alive() != Some(true))
            && start.elapsed() < Duration::from_secs(10)
        {
            thread::sleep(Duration::from_millis(5));
        }
        client.stop();

        // Если self-join deadlock всё ещё существует, on_disconnect зависнет
        // НАВСЕГДА внутри Drop for AutoServer, и флаг не станет true.
        let start = std::time::Instant::now();
        while !disconnect_returned.load(Ordering::Acquire)
            && start.elapsed() < Duration::from_secs(10)
        {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            disconnect_returned.load(Ordering::Acquire),
            "on_disconnect не вернулся за 10с -- self-join deadlock"
        );
    }
}

#[cfg(test)]
mod lossless_tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Default)]
    struct Counters {
        overflows: AtomicU32,
        errors: AtomicU32,
    }
    impl AutoHandler for Counters {
        fn on_overflow(&self, _direction: ChannelKind, count: u32) {
            self.overflows.fetch_add(count, Ordering::Relaxed);
        }
        fn on_error(&self, _err: ShmError) {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Лимит длины мок-кольца.
    const MOCK_MAX: usize = 100;

    /// Поддельный endpoint: кольцо «вмещает» `room` сообщений; `try_write`
    /// при нехватке отвечает QueueFull, `write` -- как overwrite (всегда Ok).
    struct MockEndpoint {
        room: Cell<usize>,
        written: RefCell<Vec<Vec<u8>>>,
        armed: Cell<u32>,
        arms: Cell<u32>,
    }
    impl MockEndpoint {
        fn new(room: usize) -> Self {
            Self {
                room: Cell::new(room),
                written: RefCell::new(Vec::new()),
                armed: Cell::new(0),
                arms: Cell::new(0),
            }
        }
        fn check(data: &[u8]) -> Result<()> {
            if data.len() > MOCK_MAX {
                return Err(ShmError::MessageTooLarge);
            }
            Ok(())
        }
        fn accept(&self, data: &[u8]) -> Result<WriteOutcome> {
            Self::check(data)?;
            self.written.borrow_mut().push(data.to_vec());
            self.room.set(self.room.get().saturating_sub(1));
            Ok(WriteOutcome {
                overwritten: 0,
                was_empty: false,
            })
        }
    }
    impl SendEndpoint for MockEndpoint {
        fn write(&self, data: &[u8]) -> Result<WriteOutcome> {
            self.accept(data)
        }
        fn try_write(&self, data: &[u8]) -> Result<WriteOutcome> {
            Self::check(data)?;
            if self.room.get() == 0 {
                return Err(ShmError::QueueFull);
            }
            self.accept(data)
        }
        fn free_space(&self) -> FreeSpace {
            FreeSpace {
                bytes: self.room.get() * 1000,
                max_frame: self.room.get() * 1000,
                messages: self.room.get() as u32,
                max_message_size: MOCK_MAX,
            }
        }
        fn arm_space_waiter(&self, payload_len: usize) {
            self.armed.set(frame_size(payload_len) as u32);
            self.arms.set(self.arms.get() + 1);
        }
        fn disarm_space_waiter(&self) {
            self.armed.set(0);
        }
    }

    struct Harness {
        outbox: Outbox,
        wake: EventHandle,
        running: AtomicBool,
        gauge: ChannelState,
        counters: Arc<Counters>,
        handler: Arc<dyn AutoHandler>,
        stats: Arc<AutoStats>,
    }

    impl Harness {
        fn new() -> Self {
            let counters = Arc::new(Counters::default());
            Self {
                outbox: Outbox::default(),
                wake: EventHandle::create_unnamed(false).unwrap(),
                running: AtomicBool::new(true),
                gauge: ChannelState::default(),
                handler: counters.clone(),
                counters,
                stats: Arc::default(),
            }
        }
        fn send(&self, data: &[u8], lossless: bool, cap: usize) -> Result<()> {
            enqueue(
                &self.outbox,
                &self.wake,
                &self.running,
                &self.gauge,
                data,
                lossless,
                cap,
            )
        }
        fn queued(&self) -> Vec<Vec<u8>> {
            self.outbox.lock().iter().map(|m| m.data.clone()).collect()
        }
        fn flush(&self, ep: &MockEndpoint) -> bool {
            let dir = ChannelKind::ClientToServer;
            process_send_queue(
                ep,
                &self.outbox,
                &self.handler,
                &self.stats,
                &self.gauge,
                dir,
            )
        }
    }

    /// `try_send` ограничен `max_send_queue` на входе; `send` при
    /// переполнении вытесняет только обычные сообщения (сразу, на стороне
    /// отправителя -- память ограничена, даже если worker занят), а если в
    /// очереди одни lossless -- выбрасывается само новое сообщение `send`.
    #[test]
    fn queue_policy_never_evicts_lossless() {
        let h = Harness::new();
        h.send(b"L1", true, 2).unwrap();
        h.send(b"L2", true, 2).unwrap();
        assert_eq!(h.send(b"L3", true, 2), Err(ShmError::QueueFull));
        assert_eq!(
            h.send(b"", true, 2),
            Err(ShmError::MessageTooSmall),
            "длина проверяется синхронно"
        );
        h.send(b"S1", false, 2).unwrap();
        assert_eq!(h.queued(), [b"L1".to_vec(), b"L2".to_vec()]);
        assert_eq!(h.outbox.evicted.load(Ordering::Acquire), 1);
        assert_eq!(h.gauge.pending_msgs.load(Ordering::Acquire), 2);
        assert_eq!(
            h.gauge.pending_bytes.load(Ordering::Acquire),
            2 * frame_size(2)
        );

        // Смешанная очередь: вытесняется самое старое ОБЫЧНОЕ сообщение.
        let h = Harness::new();
        h.send(b"S0", false, 2).unwrap();
        h.send(b"L0", true, 2).unwrap();
        h.send(b"S2", false, 2).unwrap();
        assert_eq!(h.queued(), [b"L0".to_vec(), b"S2".to_vec()]);
        assert_eq!(h.gauge.pending_msgs.load(Ordering::Acquire), 2);
        assert_eq!(h.outbox.evicted.load(Ordering::Acquire), 1);
    }

    /// `max_send_queue = 0` трактуется как 1 одинаково для `send` и
    /// `try_send` (раньше `send` выбрасывал всё).
    #[test]
    fn zero_queue_limit_means_one() {
        let h = Harness::new();
        h.send(b"a1", false, 0).unwrap();
        assert_eq!(h.queued(), [b"a1".to_vec()]);
        assert_eq!(h.send(b"b1", true, 0), Err(ShmError::QueueFull));
    }

    /// Длина проверяется синхронно против лимита кольца, как только он
    /// известен (у сервера -- сразу, у клиента -- после подключения).
    #[test]
    fn send_checks_ring_limit_synchronously() {
        let h = Harness::new();
        h.send(&[0u8; 5000], false, 8).unwrap();
        h.gauge.set_layout(
            ChannelLayout::symmetric(crate::layout::RingConfig::new(8192).max_message_size(1000)),
            ChannelKind::ClientToServer,
        );
        assert_eq!(
            h.send(&[0u8; 1001], false, 8),
            Err(ShmError::MessageTooLarge)
        );
        assert_eq!(
            h.send(&[0u8; 1001], true, 8),
            Err(ShmError::MessageTooLarge)
        );
        h.send(&[0u8; 1000], true, 8).unwrap();
    }

    /// Голова-lossless не влезает: worker ставит заявку ровно один раз,
    /// ничего не теряет и не обгоняет (FIFO); освободилось место --
    /// очередь уходит целиком, заявка снимается, учёт pending обнуляется.
    #[test]
    fn blocked_lossless_head_keeps_fifo_and_arms_waiter() {
        let h = Harness::new();
        for i in 0..5u8 {
            h.send(&[i, i, i], true, 16).unwrap();
        }
        let ep = MockEndpoint::new(2);
        assert!(h.flush(&ep));
        assert_eq!(ep.written.borrow().len(), 2);
        assert_eq!(h.queued().len(), 3);
        assert_eq!(ep.arms.get(), 1, "заявка -- один раз за проход");
        assert_eq!(ep.armed.get(), frame_size(3) as u32);
        assert_eq!(h.gauge.pending_msgs.load(Ordering::Acquire), 3);
        // Оценка места: кольцо пусто (room=0), а 3 сообщения ещё ждут.
        assert_eq!(h.gauge.estimate(16), FreeSpace::ZERO);

        ep.room.set(10);
        assert!(h.flush(&ep));
        assert!(h.queued().is_empty());
        assert_eq!(ep.armed.get(), 0, "успешная запись снимает заявку");
        let written = ep.written.borrow();
        for (i, msg) in written.iter().enumerate() {
            assert_eq!(msg, &vec![i as u8; 3], "порядок нарушен");
        }
        assert_eq!(h.gauge.pending_msgs.load(Ordering::Acquire), 0);
        assert_eq!(h.gauge.pending_bytes.load(Ordering::Acquire), 0);
        assert_eq!(h.stats.sent_messages.load(Ordering::Relaxed), 5);
        assert_eq!(h.counters.overflows.load(Ordering::Relaxed), 0);
    }

    /// Сообщение, принятое клиентом до того, как он узнал лимит канала, и
    /// не влезающее в кольцо, не блокирует очередь: `on_error` и дальше.
    #[test]
    fn oversized_message_does_not_wedge_queue() {
        let h = Harness::new();
        h.send(&[1u8; MOCK_MAX + 1], false, 16).unwrap();
        h.send(b"ok", false, 16).unwrap();
        let ep = MockEndpoint::new(10);
        assert!(h.flush(&ep));
        assert!(h.queued().is_empty());
        assert_eq!(*ep.written.borrow(), [b"ok".to_vec()]);
        assert_eq!(h.counters.errors.load(Ordering::Relaxed), 1);
        assert_eq!(h.gauge.pending_msgs.load(Ordering::Acquire), 0);
    }

    /// Оценка места консервативна: вычитает ещё не записанное и ограничена
    /// свободным местом очереди; `max_frame` без ожидающих -- из снимка
    /// кольца, с ожидающими -- с наихудшим PAD (кадр максимального
    /// сообщения минус 8).
    #[test]
    fn gauge_estimate_is_conservative() {
        let gauge = ChannelState::default();
        assert_eq!(gauge.estimate(8), FreeSpace::ZERO, "до подключения");
        gauge.set_layout(ChannelLayout::DEFAULT, ChannelKind::ServerToClient);
        gauge.publish_ring(FreeSpace {
            bytes: 10_000,
            max_frame: 6_000,
            messages: 100,
            max_message_size: 0,
        });
        assert_eq!(
            gauge.estimate(8),
            FreeSpace {
                bytes: 10_000,
                max_frame: 6_000,
                messages: 8,
                max_message_size: crate::constants::DEFAULT_MAX_MESSAGE_SIZE,
            }
        );
        gauge.acquire(1000);
        gauge.acquire(1000);
        let est = gauge.estimate(8);
        assert_eq!(est.bytes, 8_000);
        assert_eq!(est.messages, 6);
        // Наихудший PAD серии -- кадр 65 535 байт минус 8: больше 8 000.
        assert_eq!(est.max_frame, 0);
        assert!(!est.fits(16));
        assert!(!gauge.try_acquire(10, 2), "очередь 2/2");
        gauge.release(1000);
        assert!(gauge.try_acquire(10, 2));

        // Маленький лимит сообщения: место после ожидающих -- bytes + 8 - M.
        let gauge = ChannelState::default();
        let ring = crate::layout::RingConfig::new(64 << 10).max_message_size(1000);
        gauge.set_layout(ChannelLayout::symmetric(ring), ChannelKind::ServerToClient);
        gauge.publish_ring(FreeSpace {
            bytes: 4_000,
            max_frame: 3_000,
            messages: 100,
            max_message_size: 1000,
        });
        assert_eq!(gauge.estimate(8).max_frame, 3_000, "снимок кольца");
        gauge.acquire(frame_size(1000));
        let est = gauge.estimate(8);
        assert_eq!(est.bytes, 4_000 - 1008);
        assert_eq!(est.max_frame, 4_000 - 1008 + 8 - 1008);
        assert!(est.fits(1000));
        // Проверка оценки: ожидающий кадр 1008 + наихудший PAD 1000 + новый
        // кадр 1008 = 3024 <= 4000.
        assert!(1008 + 1000 + frame_size(1000) <= 4_000);
    }

    #[derive(Default)]
    struct Counting {
        connected: AtomicBool,
        received: AtomicU64,
    }

    impl AutoHandler for Counting {
        fn on_connect(&self) {
            self.connected.store(true, Ordering::Release);
        }
        fn on_message(&self, _direction: ChannelKind, _payload: &[u8]) {
            self.received.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn wait_until(what: &str, limit: Duration, mut cond: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + limit;
        while !cond() {
            assert!(std::time::Instant::now() < deadline, "таймаут: {what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// 0.9: подключённые `AutoServer`/`AutoClient` в простое не просыпаются
    /// ни разу (до 0.9 -- каждые 50 мс), а каждое сообщение -- считанные
    /// пробуждения.
    #[test]
    fn idle_worker_does_not_wake_up() {
        let name = format!("AUTO_IDLE_{}", std::process::id());
        let server_h = Arc::new(Counting::default());
        let client_h = Arc::new(Counting::default());
        let server = AutoServer::start(&name, server_h.clone(), AutoOptions::default()).unwrap();
        let client = AutoClient::connect(&name, client_h.clone(), AutoOptions::default()).unwrap();
        wait_until("подключение", Duration::from_secs(5), || {
            server_h.connected.load(Ordering::Acquire) && client_h.connected.load(Ordering::Acquire)
        });
        client.send(b"ping").unwrap();
        wait_until("доставка", Duration::from_secs(2), || {
            server_h.received.load(Ordering::Acquire) == 1
        });
        thread::sleep(Duration::from_millis(200));

        let (s0, c0) = (server.wakeups(), client.wakeups());
        thread::sleep(Duration::from_secs(1));
        assert_eq!(server.wakeups() - s0, 0, "сервер просыпался в простое");
        assert_eq!(client.wakeups() - c0, 0, "клиент просыпался в простое");

        // Одно сообщение -- ограниченное число пробуждений, не тик.
        client.send(b"ping").unwrap();
        wait_until("доставка", Duration::from_secs(2), || {
            server_h.received.load(Ordering::Acquire) == 2
        });
        thread::sleep(Duration::from_millis(100));
        assert!(
            server.wakeups() - s0 <= 3,
            "сервер: {}",
            server.wakeups() - s0
        );
        assert!(
            client.wakeups() - c0 <= 3,
            "клиент: {}",
            client.wakeups() - c0
        );
        drop(client);
        drop(server);
    }

    /// 0.10: спин включён -- обмен работает (ping-pong через колбэк), а в
    /// простое worker отспинивает бюджет один раз и спит в ядре: ноль
    /// пробуждений, как без спина.
    #[test]
    fn idle_worker_with_spin_does_not_wake_up() {
        spin_ping_pong_then_idle(SpinPolicy::new(20_000));
    }

    /// 0.10: бюджет `u32::MAX` (минуты спина) -- `stop()` прерывает спин по
    /// флагу `running`, а не ждёт конца бюджета; в простое (воркеры ещё
    /// спинят) пробуждений тоже нет.
    #[test]
    fn stop_interrupts_unbounded_spin() {
        spin_ping_pong_then_idle(SpinPolicy::new(u32::MAX));
    }

    fn spin_ping_pong_then_idle(spin: SpinPolicy) {
        /// Сервер отвечает эхом из своего колбэка.
        struct Echo {
            server: Mutex<Option<Arc<AutoServer>>>,
            counting: Counting,
        }
        impl AutoHandler for Echo {
            fn on_connect(&self) {
                self.counting.on_connect();
            }
            fn on_message(&self, direction: ChannelKind, payload: &[u8]) {
                self.counting.on_message(direction, payload);
                if let Some(server) = self.server.lock().unwrap().as_ref() {
                    server.send(payload).unwrap();
                }
            }
        }
        let options = || AutoOptions {
            spin,
            ..AutoOptions::default()
        };
        let name = format!("AUTO_IDLE_SPIN_{}_{}", std::process::id(), spin.iterations);
        let server_h = Arc::new(Echo {
            server: Mutex::new(None),
            counting: Counting::default(),
        });
        let client_h = Arc::new(Counting::default());
        let server = Arc::new(AutoServer::start(&name, server_h.clone(), options()).unwrap());
        *server_h.server.lock().unwrap() = Some(Arc::clone(&server));
        let client = AutoClient::connect(&name, client_h.clone(), options()).unwrap();
        wait_until("подключение", Duration::from_secs(5), || {
            server_h.counting.connected.load(Ordering::Acquire)
                && client_h.connected.load(Ordering::Acquire)
        });
        for round in 1..=100u64 {
            client.send(b"ping").unwrap();
            wait_until("эхо", Duration::from_secs(2), || {
                client_h.received.load(Ordering::Acquire) == round
            });
        }
        thread::sleep(Duration::from_millis(200));

        let (s0, c0) = (server.wakeups(), client.wakeups());
        thread::sleep(Duration::from_secs(1));
        assert_eq!(server.wakeups() - s0, 0, "сервер просыпался в простое");
        assert_eq!(client.wakeups() - c0, 0, "клиент просыпался в простое");

        let t0 = std::time::Instant::now();
        client.stop();
        *server_h.server.lock().unwrap() = None;
        server.stop();
        assert!(t0.elapsed() < Duration::from_secs(2), "stop ждал спина");
    }

    /// Ревизия 2, дефект 4: `stop()`/Drop клиента, ждущего `S2C_CONNECT`
    /// (сервер создал канал, но не принимает), возвращаются сразу -- ожидание
    /// рукопожатия прерывает `wake`, а не `connect_timeout` (2 с). Заявка при
    /// этом отозвана: `client_state` снова `IDLE`.
    #[test]
    fn stop_during_connect_is_prompt() {
        struct Quiet;
        impl AutoHandler for Quiet {}
        const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
        let name = format!("AUTO_STOPCONN_{}", std::process::id());
        // Сервер без `wait_for_client`: заявку никто не примет.
        let server = SharedServer::start(&name).unwrap();
        let client_state = || {
            server
                .view()
                .control_block()
                .client_state
                .load(Ordering::Acquire)
        };
        for use_stop in [true, false] {
            // Лучшая из трёх попыток: единичный всплеск планировщика под
            // нагрузкой полного прогона -- не то, что проверяется; проверяется,
            // что остановка не ждёт `connect_timeout` (30 с).
            let mut best = Duration::MAX;
            for _ in 0..3 {
                let client = AutoClient::connect(
                    &name,
                    Arc::new(Quiet),
                    AutoOptions {
                        connect_timeout: CONNECT_TIMEOUT,
                        ..AutoOptions::default()
                    },
                )
                .unwrap();
                wait_until("заявка подана", Duration::from_secs(5), || {
                    client_state() == crate::constants::HANDSHAKE_CLIENT_HELLO
                });
                // `send` будит worker, но подключение не прерывает.
                client.send(b"queued").unwrap();
                thread::sleep(Duration::from_millis(50));
                assert_eq!(
                    client_state(),
                    crate::constants::HANDSHAKE_CLIENT_HELLO,
                    "`send` прервал рукопожатие"
                );
                let t0 = std::time::Instant::now();
                if use_stop {
                    client.stop();
                }
                drop(client);
                let took = t0.elapsed();
                assert!(
                    took < CONNECT_TIMEOUT / 10,
                    "остановка ждала рукопожатия: {took:?}"
                );
                best = best.min(took);
                assert_eq!(
                    client_state(),
                    crate::constants::HANDSHAKE_IDLE,
                    "заявка не отозвана"
                );
            }
            assert!(
                best < Duration::from_millis(200),
                "stop = {use_stop}: лучшая из трёх {best:?}"
            );
        }
    }
}

#[cfg(test)]
mod claim_tests {
    //! Ревизия 3 (Task 22): исключительная заявка канала Auto.

    use super::*;
    use crate::naming::ObjectPath;
    use crate::shared::SharedView;
    use crate::win::Mapping;
    use std::time::Instant;

    fn unique(tag: &str) -> String {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("AUTO_CLAIM_{tag}_{}_{}", std::process::id(), ts % 1_000_000)
    }

    fn wait_until(what: &str, limit: Duration, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + limit;
        while !cond() {
            assert!(Instant::now() < deadline, "таймаут: {what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[derive(Default)]
    struct Log {
        connected: AtomicBool,
        connects: AtomicU32,
        errors: Mutex<Vec<ShmError>>,
        reasons: Mutex<Vec<DisconnectReason>>,
        /// Ошибки, уже доставленные к моменту каждого `on_disconnect_reason`.
        errors_at_reason: Mutex<Vec<Vec<ShmError>>>,
        overflows: Mutex<Vec<(ChannelKind, u32)>>,
    }

    impl AutoHandler for Log {
        fn on_connect(&self) {
            self.connects.fetch_add(1, Ordering::AcqRel);
            self.connected.store(true, Ordering::Release);
        }
        fn on_disconnect_reason(&self, reason: DisconnectReason) {
            self.connected.store(false, Ordering::Release);
            let errors = self.errors.lock().unwrap().clone();
            self.errors_at_reason.lock().unwrap().push(errors);
            self.reasons.lock().unwrap().push(reason);
        }
        fn on_error(&self, err: ShmError) {
            self.errors.lock().unwrap().push(err);
        }
        fn on_overflow(&self, direction: ChannelKind, count: u32) {
            self.overflows.lock().unwrap().push((direction, count));
        }
    }

    /// Сработало ли событие за `limit` (автосброс: ожидание его потребляет).
    fn fired(event: &EventHandle, limit: Duration) -> bool {
        event.wait(Some(limit)).unwrap()
    }

    /// Final review I1: разрыв по ошибке у `AutoClient` (порча кольца
    /// S2C -- `Corrupted`) -- штатный уход L: `client_state -> IDLE`, потом
    /// `DISCONNECT`, claim снят. До исправления клиент помечал себя
    /// отключённым, `Drop` пропускал L, и канал заклинивал навсегда:
    /// сервер «подключён», а повторное подключение -- `AlreadyConnected`.
    #[test]
    fn client_error_leaves_channel_cleanly() {
        let name = unique("CLIENT_ERR");
        let mut server = SharedServer::start(&name).unwrap();
        let log = Arc::new(Log::default());
        let client = AutoClient::connect(&name, log.clone(), AutoOptions::default()).unwrap();
        server
            .wait_for_client(Some(Duration::from_secs(5)))
            .unwrap();
        wait_until("подключение", Duration::from_secs(5), || {
            log.connected.load(Ordering::Acquire)
        });

        // Порча кольца сервер -> клиент: `write_pos` не кратен 8.
        server
            .view()
            .header_a()
            .write_pos
            .store(3, Ordering::Release);
        server.events().unwrap().s2c.data.set().unwrap();

        assert!(
            fired(&server.events().unwrap().disconnect, Duration::from_secs(3)),
            "клиент ушёл без DISCONNECT"
        );
        let control = server.view().control_block();
        assert_eq!(
            control.client_state.load(Ordering::Acquire),
            crate::constants::HANDSHAKE_IDLE,
            "client_state не сброшен"
        );
        wait_until(
            "причина разрыва",
            Duration::from_secs(2),
            || !log.reasons.lock().unwrap().is_empty(),
        );
        assert_eq!(
            log.reasons.lock().unwrap().as_slice(),
            &[DisconnectReason::Error]
        );
        assert!(log.errors.lock().unwrap().contains(&ShmError::Corrupted));

        // Канал не заклинен: следующее подключение того же клиента проходит.
        server.mark_disconnected();
        server
            .wait_for_client(Some(Duration::from_secs(5)))
            .unwrap();
        wait_until(
            "переподключение",
            Duration::from_secs(5),
            || log.connects.load(Ordering::Acquire) == 2,
        );
        assert!(
            !log.errors
                .lock()
                .unwrap()
                .contains(&ShmError::AlreadyConnected),
            "{:?}",
            log.errors.lock().unwrap()
        );
        drop(client);
    }

    /// Final review I2: разрыв по ошибке у `AutoServer` (порча кольца C2S)
    /// -- уход сервера по R14: `server_state -> IDLE`, потом `DISCONNECT`.
    /// До исправления сигнала не было: клиент оставался «подключённым» к
    /// никому, держал claim и спал без таймаута.
    #[test]
    fn server_error_signals_disconnect() {
        let name = unique("SERVER_ERR");
        let server_log = Arc::new(Log::default());
        let server = AutoServer::start(&name, server_log.clone(), AutoOptions::default()).unwrap();
        let client = SharedClient::connect(&name, Duration::from_secs(5)).unwrap();
        wait_until("подключение", Duration::from_secs(5), || {
            server_log.connected.load(Ordering::Acquire)
        });

        let mapping = Mapping::open(ObjectPath::mapping(None, &name)).unwrap();
        // SAFETY: отображение живо до конца теста, размер -- его собственный.
        let view = unsafe { SharedView::attach(mapping.as_ptr(), mapping.size()) }.unwrap();
        view.header_b().write_pos.store(3, Ordering::Release);
        client.events().c2s.data.set().unwrap();

        assert!(
            fired(&client.events().disconnect, Duration::from_secs(3)),
            "сервер разорвал сессию без DISCONNECT"
        );
        assert!(client.server_left(), "server_state не сброшен до сигнала");
        wait_until(
            "причина разрыва",
            Duration::from_secs(2),
            || !server_log.reasons.lock().unwrap().is_empty(),
        );
        assert_eq!(
            server_log.reasons.lock().unwrap().as_slice(),
            &[DisconnectReason::Error]
        );
        assert!(
            server_log
                .errors
                .lock()
                .unwrap()
                .contains(&ShmError::Corrupted)
        );

        // Клиент уходит, следующий подключается к тому же серверу.
        drop(client);
        let next = SharedClient::connect(&name, Duration::from_secs(5)).unwrap();
        wait_until(
            "второе подключение",
            Duration::from_secs(5),
            || server_log.connects.load(Ordering::Acquire) == 2,
        );
        drop(next);
        drop(server);
    }

    /// Final review M1: клиент Auto сверяет `generation` сессии на
    /// пробуждениях. Сервер провёл рукопожатие с другим клиентом (здесь --
    /// сдвиг `generation` вручную) -- «не заметивший» клиент уходит без
    /// записи в чужое кольцо, без `DISCONNECT` и без смены состояний.
    /// Быстрый модульный вариант; настоящий второй клиент и смена сессии
    /// посреди прохода -- `foreign_session_mid_pass_reports_graceful_without_error`.
    #[test]
    fn client_notices_foreign_session_on_wakeup() {
        let name = unique("FOREIGN_GEN");
        let mut server = SharedServer::start(&name).unwrap();
        let log = Arc::new(Log::default());
        let client = AutoClient::connect(&name, log.clone(), AutoOptions::default()).unwrap();
        server
            .wait_for_client(Some(Duration::from_secs(5)))
            .unwrap();
        wait_until("подключение", Duration::from_secs(5), || {
            log.connected.load(Ordering::Acquire)
        });

        let control = server.view().control_block();
        control.generation.fetch_add(1, Ordering::AcqRel);
        // `send` будит worker (`wake`): он обязан заметить чужую сессию.
        client.send(b"stale-session").unwrap();
        wait_until("уход", Duration::from_secs(3), || {
            !log.reasons.lock().unwrap().is_empty()
        });
        assert_eq!(
            log.reasons.lock().unwrap().as_slice(),
            &[DisconnectReason::Graceful]
        );
        assert!(
            !server.has_unread_from_client(),
            "устаревший клиент писал в кольцо чужой сессии"
        );
        assert!(
            !fired(&server.events().unwrap().disconnect, Duration::ZERO),
            "устаревший клиент сигналил DISCONNECT чужой сессии"
        );
        assert_eq!(
            control.client_state.load(Ordering::Acquire),
            crate::constants::HANDSHAKE_SERVER_READY,
            "устаревший клиент тронул состояние чужой сессии"
        );
        drop(client);
    }

    /// Task 6 надёжности (п. 4): сессия сменяется посреди прохода worker-а
    /// -- между сверкой в начале прохода и записью очереди (хук
    /// `BeforeFlush`): сервер отключил A (`disconnect_local`), B получил
    /// claim через дедлайн по толчку и подключился. A уходит ровно с
    /// `Graceful`, без единого `on_error` (в частности без `NotConnected`),
    /// не пишет в кольцо сессии B, а неотправленное сообщение доставляет
    /// ровно один раз, когда сервер снова принимает A.
    #[test]
    fn foreign_session_mid_pass_reports_graceful_without_error() {
        use std::sync::mpsc;

        let name = unique("MID_PASS");
        let mut server = SharedServer::start(&name).unwrap();
        server.set_reserve_timeout(Duration::from_millis(150));
        let log = Arc::new(Log::default());
        let options = AutoOptions {
            reconnect_delay: Duration::from_millis(50),
            ..AutoOptions::default()
        };
        let a = AutoClient::connect(&name, log.clone(), options).unwrap();
        server
            .wait_for_client(Some(Duration::from_secs(5)))
            .unwrap();
        wait_until("подключение A", Duration::from_secs(5), || {
            log.connected.load(Ordering::Acquire)
        });

        let (entered_tx, entered) = mpsc::channel::<()>();
        let (release, release_rx) = mpsc::channel::<()>();
        WORKER_HOOKS.lock().unwrap().push((
            name.clone(),
            WorkerPoint::BeforeFlush,
            Box::new(move || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }),
        ));
        // `send` будит worker: проход останавливается в хуке до записи "m".
        a.send(b"m").unwrap();
        entered.recv_timeout(Duration::from_secs(5)).unwrap();

        // Смена сессии в окне: сервер ушёл, B подключается через дедлайн.
        server.disconnect_local();
        let waiter = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while let Err(err) = server.wait_for_client(Some(Duration::from_secs(5))) {
                assert!(Instant::now() < deadline, "сервер не принял B: {err:?}");
            }
            server
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        let b = loop {
            match SharedClient::connect(&name, Duration::from_secs(1)) {
                Ok(b) => break b,
                Err(ShmError::AlreadyConnected) => {
                    assert!(Instant::now() < deadline, "B не подключился за 3 с");
                    thread::sleep(Duration::from_millis(50));
                }
                Err(err) => panic!("B: {err:?}"),
            }
        };
        let mut server = waiter.join().unwrap();
        release.send(()).unwrap();

        wait_until("уход A", Duration::from_secs(3), || {
            !log.reasons.lock().unwrap().is_empty()
        });
        assert_eq!(
            log.reasons.lock().unwrap().as_slice(),
            &[DisconnectReason::Graceful]
        );
        assert_eq!(
            log.errors_at_reason.lock().unwrap().as_slice(),
            &[Vec::<ShmError>::new()],
            "ложные ошибки при смене сессии"
        );

        // Сессия B: в C2S только её кадр, обмен в обе стороны.
        let mut buf = Vec::new();
        b.send_to_server(b"b1").unwrap();
        assert_eq!(server.receive_from_client(&mut buf).unwrap(), 2);
        assert_eq!(buf, b"b1", "A писал в кольцо сессии B");
        assert!(!server.has_unread_from_client(), "лишнее в C2S сессии B");
        server.send_to_client(b"s1").unwrap();
        assert_eq!(b.receive_from_server(&mut buf).unwrap(), 2);
        assert_eq!(buf, b"s1");

        // B уходит -- сервер снова принимает A, "m" доставлен один раз.
        drop(b);
        assert_eq!(
            server.poll_client(Some(Duration::from_secs(5))).err(),
            Some(ShmError::NotConnected)
        );
        server
            .wait_for_client(Some(Duration::from_secs(5)))
            .unwrap();
        // `DATA` от "b1" прошлой сессии мог остаться взведённым -- ждём кадр.
        let deadline = Instant::now() + Duration::from_secs(5);
        let len = loop {
            match server.receive_from_client(&mut buf) {
                Ok(len) => break len,
                Err(ShmError::QueueEmpty) => {
                    assert!(Instant::now() < deadline, "\"m\" не доставлен");
                    let _ = server.poll_client(Some(Duration::from_millis(100)));
                }
                Err(err) => panic!("сервер: {err:?}"),
            }
        };
        assert_eq!(len, 1);
        assert_eq!(buf, b"m");
        thread::sleep(Duration::from_millis(100));
        assert!(!server.has_unread_from_client(), "\"m\" доставлен дважды");
        assert!(
            log.errors
                .lock()
                .unwrap()
                .iter()
                .all(|err| *err == ShmError::AlreadyConnected),
            "{:?}",
            log.errors.lock().unwrap()
        );
        drop(a);
    }

    /// Ruling S (Task 6, fix round 1): очередь `AutoServer`, не записанная
    /// к уходу клиента A, предназначалась A -- следующий клиент B её не
    /// получает. Сброс сообщается один раз (`on_overflow(ServerToClient,
    /// n)` и `discarded_messages`), без `on_error`; принятое между сессиями
    /// уходит B.
    #[test]
    fn server_queue_of_departed_client_is_not_delivered_to_next() {
        const RING_MESSAGES: u32 = 500; // `max_messages` раскладки по умолчанию
        const QUEUED: u32 = 10;
        let name = unique("DISCARD");
        let log = Arc::new(Log::default());
        let server = AutoServer::start(&name, log.clone(), AutoOptions::default()).unwrap();
        let a = SharedClient::connect(&name, Duration::from_secs(5)).unwrap();
        wait_until("подключение A", Duration::from_secs(5), || {
            log.connected.load(Ordering::Acquire)
        });

        // A не читает: кольцо заполняется, остаток ждёт в очереди сервера.
        for i in 0..RING_MESSAGES + QUEUED {
            let payload = format!("a-{i:04}").into_bytes();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match server.try_send(&payload) {
                    Ok(()) => break,
                    Err(ShmError::QueueFull) => {
                        assert!(Instant::now() < deadline, "очередь не разгружается");
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(err) => panic!("try_send: {err:?}"),
                }
            }
        }
        // Кольцо держит `max_messages` (+1 по правилу пересчёта §c.5) --
        // ждём, пока запись не встанет, остаток -- в очереди.
        wait_until(
            "кольцо A заполнено",
            Duration::from_secs(5),
            || server.stats().sent_messages >= u64::from(RING_MESSAGES),
        );
        let mut sent = server.stats().sent_messages;
        loop {
            thread::sleep(Duration::from_millis(100));
            let now = server.stats().sent_messages;
            if now == sent {
                break;
            }
            sent = now;
        }
        let queued = u32::try_from(u64::from(RING_MESSAGES + QUEUED) - sent).unwrap();
        assert!(queued > 0, "очередь пуста: кольцо приняло всё");

        drop(a);
        wait_until("уход A", Duration::from_secs(5), || {
            !log.reasons.lock().unwrap().is_empty()
        });
        wait_until("отчёт о сбросе", Duration::from_secs(2), || {
            !log.overflows.lock().unwrap().is_empty()
        });
        assert_eq!(
            log.overflows.lock().unwrap().as_slice(),
            &[(ChannelKind::ServerToClient, queued)],
            "отчёт о сбросе -- ровно один, на всю очередь"
        );
        assert_eq!(server.stats().discarded_messages, u64::from(queued));
        assert!(log.errors.lock().unwrap().is_empty(), "{:?}", log.errors);

        // Принятое между сессиями -- уже для следующего клиента.
        server.send(b"for-b").unwrap();
        let mut b = SharedClient::connect(&name, Duration::from_secs(5)).unwrap();
        let mut buf = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        let len = loop {
            match b.receive_from_server(&mut buf) {
                Ok(len) => break len,
                Err(ShmError::QueueEmpty) => {
                    assert!(Instant::now() < deadline, "B ничего не получил");
                    let _ = b.poll_server(Some(Duration::from_millis(100)));
                }
                Err(err) => panic!("B: {err:?}"),
            }
        };
        assert_eq!(&buf[..len], b"for-b", "B получил сообщение A");
        thread::sleep(Duration::from_millis(100));
        assert_eq!(
            b.receive_from_server(&mut buf).err(),
            Some(ShmError::QueueEmpty),
            "B получил лишнее: {buf:?}"
        );
        assert_eq!(log.overflows.lock().unwrap().len(), 1);
        drop(b);
        drop(server);
    }

    /// Final review M4: проигравший `AutoClient` повторяет с растущим
    /// отступом (удвоение от `reconnect_delay` до `BUSY_RECONNECT_CAP`), а
    /// не каждые `reconnect_delay` бесконечно.
    #[test]
    fn already_connected_backs_off() {
        let name = unique("BACKOFF");
        let server =
            AutoServer::start(&name, Arc::new(Log::default()), AutoOptions::default()).unwrap();
        let holder = SharedClient::connect(&name, Duration::from_secs(5)).unwrap();
        let log = Arc::new(Log::default());
        let options = AutoOptions {
            reconnect_delay: Duration::from_millis(20),
            ..AutoOptions::default()
        };
        let client = AutoClient::connect(&name, log.clone(), options).unwrap();
        thread::sleep(Duration::from_millis(1500));
        let attempts = log.errors.lock().unwrap().len();
        // Без отступа -- около 75 попыток (1,5 с / 20 мс); с удвоением
        // (20 + 40 + 80 + 160 + 320 + 640 мс) -- не больше 7.
        assert!((2..=8).contains(&attempts), "попыток за 1,5 с: {attempts}");
        assert!(
            log.errors
                .lock()
                .unwrap()
                .iter()
                .all(|err| *err == ShmError::AlreadyConnected)
        );
        assert!(holder.is_connected());
        drop(client);
        drop(holder);
        drop(server);
    }

    /// §7.1 п. 13: канал занят `SharedClient` -- `AutoClient` получает
    /// `on_error(AlreadyConnected)` и повторяет через `reconnect_delay`;
    /// держатель ушёл -- подключение в пределах двух интервалов.
    #[test]
    fn auto_client_retries_after_already_connected() {
        let name = unique("RETRY");
        let server_log = Arc::new(Log::default());
        let server = AutoServer::start(&name, server_log.clone(), AutoOptions::default()).unwrap();
        let holder = SharedClient::connect(&name, Duration::from_secs(5)).unwrap();
        wait_until("держатель", Duration::from_secs(5), || {
            server_log.connected.load(Ordering::Acquire)
        });
        let reconnect_delay = AutoOptions::default().reconnect_delay;
        let log = Arc::new(Log::default());
        let client = AutoClient::connect(&name, log.clone(), AutoOptions::default()).unwrap();
        wait_until("AlreadyConnected", Duration::from_secs(3), || {
            log.errors
                .lock()
                .unwrap()
                .contains(&ShmError::AlreadyConnected)
        });
        assert!(!log.connected.load(Ordering::Acquire));
        assert!(
            log.errors
                .lock()
                .unwrap()
                .iter()
                .all(|err| *err == ShmError::AlreadyConnected),
            "{:?}",
            log.errors.lock().unwrap()
        );
        assert!(holder.is_connected());

        let left = Instant::now();
        drop(holder);
        wait_until(
            "подключение после ухода",
            Duration::from_secs(5),
            || log.connected.load(Ordering::Acquire),
        );
        let waited = left.elapsed();
        assert!(waited < reconnect_delay * 2, "{waited:?}");
        drop(client);
        drop(server);
    }

    /// §7.1 п. 10: канал занят чужим claim, никто не толкает -- worker
    /// сервера не просыпается ни разу.
    #[test]
    fn idle_server_with_foreign_claim_does_not_wake_up() {
        let name = unique("IDLE");
        let server =
            AutoServer::start(&name, Arc::new(Log::default()), AutoOptions::default()).unwrap();
        let mapping = Mapping::open_header(ObjectPath::mapping(None, &name)).unwrap();
        let control = SharedView::peek_control(&mapping).unwrap();
        thread::sleep(Duration::from_millis(200));
        let s0 = server.wakeups();
        control.slot_claim.store(0x1D1E_C1A1, Ordering::Release);
        thread::sleep(Duration::from_secs(1));
        assert_eq!(server.wakeups() - s0, 0, "сервер просыпался в простое");
        drop(server);
    }
}

#[cfg(test)]
mod presence_tests {
    //! Task 11 надёжности (R19): клиент вне подключения ждёт появления
    //! сервера на маяке присутствия, а не опрашивает раз в
    //! `reconnect_delay`.

    use super::*;
    use crate::beacon::Beacon;
    use crate::test_support::{LiveChild, dead_process};
    use std::time::Instant;

    fn unique(tag: &str) -> String {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        format!(
            "AUTO_PRES_{tag}_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn wait_until(what: &str, limit: Duration, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + limit;
        while !cond() {
            assert!(Instant::now() < deadline, "таймаут: {what}");
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[derive(Default)]
    struct Log {
        connected: AtomicBool,
        connected_at: Mutex<Option<Instant>>,
        errors: Mutex<Vec<ShmError>>,
    }

    impl AutoHandler for Log {
        fn on_connect(&self) {
            *self.connected_at.lock().unwrap() = Some(Instant::now());
            self.connected.store(true, Ordering::Release);
        }
        fn on_error(&self, err: ShmError) {
            self.errors.lock().unwrap().push(err);
        }
    }

    impl Log {
        fn errors(&self) -> Vec<ShmError> {
            self.errors.lock().unwrap().clone()
        }
    }

    /// Сервера нет: клиент спит в ядре на маяке -- ни пробуждений, ни
    /// повторных `on_error` (до Task 11: попытка и ошибка каждые 250 мс).
    #[test]
    fn absent_server_client_sleeps_without_wakeups() {
        let name = unique("ABSENT");
        let log = Arc::new(Log::default());
        let client = AutoClient::connect(&name, log.clone(), AutoOptions::default()).unwrap();
        thread::sleep(Duration::from_millis(200));
        let w0 = client.wakeups();
        thread::sleep(Duration::from_secs(1));
        let woke = client.wakeups() - w0;
        assert!(woke <= 1, "пробуждений без сервера: {woke}");
        let errors = log.errors();
        assert!(errors.len() <= 1, "ошибки без сервера: {errors:?}");
        drop(client);
    }

    /// Клиент стартовал раньше сервера: подключение сразу после взвода
    /// маяка, а не через `reconnect_delay` (10 с).
    #[test]
    fn client_connects_promptly_when_server_appears() {
        let name = unique("PROMPT");
        let log = Arc::new(Log::default());
        let options = AutoOptions {
            reconnect_delay: Duration::from_secs(10),
            ..AutoOptions::default()
        };
        let client = AutoClient::connect(&name, log.clone(), options).unwrap();
        thread::sleep(Duration::from_millis(300));
        let started = Instant::now();
        let server =
            AutoServer::start(&name, Arc::new(Log::default()), AutoOptions::default()).unwrap();
        wait_until("подключение", Duration::from_secs(15), || {
            log.connected.load(Ordering::Acquire)
        });
        let took = log.connected_at.lock().unwrap().unwrap() - started;
        assert!(
            took <= Duration::from_millis(300),
            "подключение через {took:?}"
        );
        drop(client);
        drop(server);
    }

    /// Маяк упавшего сервера (взведён, секции нет): клиент гасит его
    /// (`clear_if_stale`) и спит на нём, а новый сервер принимается сразу.
    #[test]
    fn stale_beacon_of_dead_server_is_cleared_then_sleeps() {
        let name = unique("STALE");
        let (pid, start) = dead_process();
        let stale = Beacon::open(&name).unwrap();
        stale.raise_as(pid, start).unwrap();
        let log = Arc::new(Log::default());
        let options = AutoOptions {
            reconnect_delay: Duration::from_secs(10),
            ..AutoOptions::default()
        };
        let client = AutoClient::connect(&name, log.clone(), options).unwrap();
        wait_until(
            "маяк упавшего сброшен",
            Duration::from_secs(2),
            || !stale.is_raised().unwrap(),
        );
        thread::sleep(Duration::from_millis(50));
        let w0 = client.wakeups();
        thread::sleep(Duration::from_secs(1));
        let woke = client.wakeups() - w0;
        assert!(woke <= 2, "пробуждений после сброса: {woke}");
        let started = Instant::now();
        let server =
            AutoServer::start(&name, Arc::new(Log::default()), AutoOptions::default()).unwrap();
        wait_until("подключение", Duration::from_secs(15), || {
            log.connected.load(Ordering::Acquire)
        });
        let took = log.connected_at.lock().unwrap().unwrap() - started;
        assert!(
            took <= Duration::from_millis(300),
            "подключение через {took:?}"
        );
        drop(client);
        drop(server);
    }

    fn version_errors(log: &Log) -> usize {
        log.errors()
            .iter()
            .filter(|e| matches!(e, ShmError::VersionMismatch { .. }))
            .count()
    }

    /// Под именем -- сервер другого формата памяти: клиент сообщает
    /// `VersionMismatch` один раз и спит до выхода процесса этого сервера,
    /// а не повторяет раз в 250 мс. Личность (PID и время) -- из
    /// `ControlBlock`, сверенная с состоянием маяка (решение W(2)): обе
    /// указывают на дочерний процесс.
    #[test]
    fn incompatible_server_waits_for_its_exit() {
        let name = unique("VERSION");
        let server = SharedServer::start(&name).unwrap();
        let mut child = LiveChild::spawn();
        let control = server.view().control_block();
        control.version.store(0x0001_0000, Ordering::Release);
        let [l0, l1, l2, l3, h0, h1, h2, h3] = child.start.to_le_bytes();
        control
            .server_start_lo
            .store(u32::from_le_bytes([l0, l1, l2, l3]), Ordering::Release);
        control
            .server_start_hi
            .store(u32::from_le_bytes([h0, h1, h2, h3]), Ordering::Release);
        control.server_pid.store(child.pid, Ordering::Release);
        // Маяк сервера -- с той же личностью (иначе W(2): расхождение, не
        // верим ни одной -- только резервная проверка).
        let beacon = Beacon::open(&name).unwrap();
        beacon.raise_as(child.pid, child.start).unwrap();

        let log = Arc::new(Log::default());
        let client = AutoClient::connect(&name, log.clone(), AutoOptions::default()).unwrap();
        wait_until("VersionMismatch", Duration::from_secs(2), || {
            log.errors()
                .iter()
                .any(|e| matches!(e, ShmError::VersionMismatch { .. }))
        });
        thread::sleep(Duration::from_millis(100));
        let w0 = client.wakeups();
        thread::sleep(Duration::from_secs(1));
        let woke = client.wakeups() - w0;
        assert!(woke <= 2, "пробуждений при несовместимом сервере: {woke}");
        assert_eq!(version_errors(&log), 1, "{:?}", log.errors());

        let w1 = client.wakeups();
        child.kill();
        wait_until(
            "попытка после выхода сервера",
            Duration::from_millis(500),
            || client.wakeups() > w1,
        );
        thread::sleep(Duration::from_millis(300));
        let after = client.wakeups() - w1;
        assert!(after <= 2, "пробуждений после выхода: {after}");
        assert_eq!(
            version_errors(&log),
            1,
            "ошибка повторена: {:?}",
            log.errors()
        );
        assert_eq!(log.errors().len(), 1, "{:?}", log.errors());
        drop(client);
        drop(server);
        drop(beacon);
    }

    /// Опции с короткой резервной проверкой (W(3)) и длинной паузой: всё,
    /// что найдено быстрее паузы, найдено событием или резервом.
    fn fallback_options(fallback: Duration) -> AutoOptions {
        AutoOptions {
            reconnect_delay: Duration::from_secs(10),
            discovery_fallback: Some(fallback),
            ..AutoOptions::default()
        }
    }

    /// Подключение не позже `limit` после `started`.
    fn assert_connected_within(log: &Log, started: Instant, limit: Duration) {
        wait_until("подключение", Duration::from_secs(15), || {
            log.connected.load(Ordering::Acquire)
        });
        let took = log.connected_at.lock().unwrap().unwrap() - started;
        assert!(took <= limit, "подключение через {took:?}");
    }

    /// W(2): сервер, отвергнувший клиента, -- этот же процесс (свой PID в
    /// `ControlBlock`): клиент не ждёт выхода самого себя, а находит
    /// исправленный сервер резервной проверкой.
    #[test]
    fn refusal_by_own_process_is_rechecked_by_fallback() {
        let name = unique("OWNPID");
        let server = SharedServer::start(&name).unwrap();
        server
            .view()
            .control_block()
            .version
            .store(0x0001_0000, Ordering::Release);
        let log = Arc::new(Log::default());
        let client = AutoClient::connect(
            &name,
            log.clone(),
            fallback_options(Duration::from_millis(200)),
        )
        .unwrap();
        wait_until("VersionMismatch", Duration::from_secs(2), || {
            version_errors(&log) == 1
        });
        drop(server);
        let started = Instant::now();
        let server =
            AutoServer::start(&name, Arc::new(Log::default()), AutoOptions::default()).unwrap();
        assert_connected_within(&log, started, Duration::from_millis(700));
        drop(client);
        drop(server);
    }

    /// W(2): маяк взведён с подменённой личностью (живой процесс того же
    /// пользователя, время 0 -- «любой процесс с этим PID»): клиент не спит
    /// на этом процессе вечно -- реальный сервер найден в пределах резервной
    /// проверки, пробуждений -- не больше одного на её интервал.
    #[test]
    fn spoofed_beacon_identity_does_not_block_discovery() {
        let name = unique("SPOOF");
        let child = LiveChild::spawn();
        let spoof = Beacon::open(&name).unwrap();
        spoof.raise_as(child.pid, 0).unwrap();
        let log = Arc::new(Log::default());
        let client = AutoClient::connect(
            &name,
            log.clone(),
            fallback_options(Duration::from_millis(300)),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(200));
        let w0 = client.wakeups();
        thread::sleep(Duration::from_secs(1));
        let woke = client.wakeups() - w0;
        assert!(woke <= 5, "пробуждений при подменённой личности: {woke}");
        let started = Instant::now();
        let server =
            AutoServer::start(&name, Arc::new(Log::default()), AutoOptions::default()).unwrap();
        assert_connected_within(&log, started, Duration::from_millis(700));
        drop(client);
        drop(server);
        drop(spoof);
    }

    /// W(3): сервер без маяка (маяк не создан) -- клиент, стартовавший
    /// раньше, находит его резервной проверкой, а не спит на маяке вечно.
    #[test]
    fn server_without_beacon_is_found_by_fallback() {
        let name = unique("NOBEACON");
        let log = Arc::new(Log::default());
        let client = AutoClient::connect(
            &name,
            log.clone(),
            fallback_options(Duration::from_millis(200)),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(300));
        let started = Instant::now();
        let server = AutoServer::start_with_presence(
            &name,
            Arc::new(Log::default()),
            AutoOptions::default(),
            Presence::Off,
        )
        .unwrap();
        assert_eq!(server.presence(), crate::PresenceStatus::Disabled);
        assert_connected_within(&log, started, Duration::from_millis(600));
        drop(client);
        drop(server);
    }

    /// W(1): маяк занят заранее событием другого типа -- старт сервера
    /// (single-client, Auto, Multi) не отказывает, `presence()` сообщает
    /// причину, клиент всё равно подключается.
    #[test]
    fn squatted_beacon_does_not_fail_start() {
        use crate::naming::ObjectPath;
        let sec = ObjectSecurity::default().attributes().unwrap();
        let squat = |base: &str| {
            EventHandle::create(ObjectPath::scoped(None, base, "_beacon"), &sec).unwrap()
        };
        let mismatch = |status: crate::PresenceStatus| {
            matches!(
                status,
                crate::PresenceStatus::Unavailable(ShmError::WindowsError {
                    code: 0xC000_0024,
                    ..
                })
            )
        };

        let name = unique("SQUAT");
        let _squatter = squat(&name);
        let shared = SharedServer::start(&name).unwrap();
        assert!(mismatch(shared.presence()), "{:?}", shared.presence());
        drop(shared);

        let log = Arc::new(Log::default());
        let server =
            AutoServer::start(&name, Arc::new(Log::default()), AutoOptions::default()).unwrap();
        assert!(mismatch(server.presence()), "{:?}", server.presence());
        let client = AutoClient::connect(&name, log.clone(), AutoOptions::default()).unwrap();
        wait_until(
            "подключение без маяка",
            Duration::from_secs(5),
            || log.connected.load(Ordering::Acquire),
        );
        drop(client);
        drop(server);

        struct QuietMulti;
        impl crate::multi::MultiHandler for QuietMulti {
            fn on_client_connect(&self, _client_id: u32) {}
            fn on_client_disconnect(&self, _client_id: u32) {}
            fn on_message(&self, _client_id: u32, _data: &[u8]) {}
        }
        let base = unique("SQUAT_MULTI");
        let _multi_squatter = squat(&presence::multi_beacon_name(&base));
        let multi = crate::multi::MultiServer::start(
            &base,
            Arc::new(QuietMulti),
            crate::multi::MultiOptions {
                max_clients: 1,
                ..crate::multi::MultiOptions::default()
            },
        )
        .unwrap();
        assert!(mismatch(multi.presence()), "{:?}", multi.presence());
        multi.stop();
    }

    /// M1: одинаковая ошибка подряд -- один раз, но второй «зависший»
    /// сервер после промежуточной ошибки сообщается снова.
    #[test]
    fn different_hung_servers_are_each_reported() {
        let name = unique("HUNG");
        let log = Arc::new(Log::default());
        let timeouts = |log: &Log| {
            log.errors()
                .iter()
                .filter(|e| **e == ShmError::Timeout)
                .count()
        };
        let first = SharedServer::start(&name).unwrap();
        let client = AutoClient::connect(
            &name,
            log.clone(),
            AutoOptions {
                connect_timeout: Duration::from_millis(100),
                reconnect_delay: Duration::from_millis(20),
                ..AutoOptions::default()
            },
        )
        .unwrap();
        wait_until("Timeout первого", Duration::from_secs(5), || {
            timeouts(&log) == 1
        });
        thread::sleep(Duration::from_millis(400));
        assert_eq!(
            timeouts(&log),
            1,
            "повтор подряд сообщён: {:?}",
            log.errors()
        );
        drop(first);
        thread::sleep(Duration::from_millis(300));
        let second = SharedServer::start(&name).unwrap();
        wait_until("Timeout второго", Duration::from_secs(5), || {
            timeouts(&log) == 2
        });
        thread::sleep(Duration::from_millis(400));
        assert_eq!(timeouts(&log), 2, "{:?}", log.errors());
        drop(client);
        drop(second);
    }

    /// Маяк с личностью упавшего сервера в состоянии и сброшенным событием
    /// -- ровно то, что оставляет `clear_if_stale` после падения сервера.
    fn dead_identity_beacon(name: &str) -> Beacon {
        let (pid, start) = dead_process();
        let beacon = Beacon::open(name).unwrap();
        beacon.raise_as(pid, start).unwrap();
        let clearer = Beacon::open(name).unwrap();
        assert!(clearer.clear_if_stale().unwrap(), "маяк упавшего сброшен");
        assert!(!beacon.is_raised().unwrap());
        beacon
    }

    /// Секунда работы клиента против отвергающего сервера: попыток
    /// подключения -- считанные (до исправления C1 -- десятки тысяч в секунду
    /// при `wakeups() == 0`).
    fn assert_no_attempt_storm(client: &AutoClient, log: &Log) {
        wait_until("первая ошибка", Duration::from_secs(5), || {
            !log.errors().is_empty()
        });
        let a0 = client.attempts();
        thread::sleep(Duration::from_secs(1));
        let attempts = client.attempts() - a0;
        assert!(attempts <= 3, "попыток подключения за 1 с: {attempts}");
    }

    /// Self-relative дескриптор с пустым DACL (доступа нет никому).
    fn deny_all() -> ObjectSecurity {
        let mut sd = vec![1u8, 0, 0x04, 0x80];
        sd.extend_from_slice(&[0; 12]);
        sd.extend_from_slice(&20u32.to_le_bytes());
        sd.extend_from_slice(&[2, 0, 8, 0, 0, 0, 0, 0]);
        ObjectSecurity::Descriptor(sd)
    }

    /// C1 повторного ревью: секция отвергает (`AccessDenied` -- личность
    /// секции не читается, `Refused(None)`), а в состоянии маяка -- личность
    /// упавшего сервера. До исправления: личность маяка -> процесс завершился
    /// -> немедленный `Retry` -> та же ошибка -- горячий цикл без единого
    /// ожидания в ядре.
    #[test]
    fn refused_with_dead_beacon_identity_does_not_spin() {
        let name = unique("DEAD_REFUSE");
        let beacon = dead_identity_beacon(&name);
        let server = SharedServer::start_slot(
            None,
            &name,
            ChannelLayout::DEFAULT,
            0,
            &deny_all().attributes().unwrap(),
            Presence::Off,
        )
        .unwrap();
        let log = Arc::new(Log::default());
        let client = AutoClient::connect(&name, log.clone(), AutoOptions::default()).unwrap();
        assert_no_attempt_storm(&client, &log);
        assert_eq!(log.errors(), vec![ShmError::AccessDenied]);
        drop(client);
        drop(server);
        drop(beacon);
    }

    /// C1 + m2: секция другого формата с недоверенной личностью (живой PID,
    /// время 0), маяк -- с личностью упавшего сервера: ни горячего цикла, ни
    /// доверия маяку (PID секции другой).
    #[test]
    fn refused_untrusted_section_with_dead_beacon_does_not_spin() {
        let name = unique("DEAD_VERSION");
        let server = SharedServer::start(&name).unwrap();
        let beacon = dead_identity_beacon(&name);
        let child = LiveChild::spawn();
        let control = server.view().control_block();
        control.version.store(0x0001_0000, Ordering::Release);
        control.server_start_lo.store(0, Ordering::Release);
        control.server_start_hi.store(0, Ordering::Release);
        control.server_pid.store(child.pid, Ordering::Release);
        let log = Arc::new(Log::default());
        let client = AutoClient::connect(&name, log.clone(), AutoOptions::default()).unwrap();
        assert_no_attempt_storm(&client, &log);
        assert_eq!(version_errors(&log), 1, "{:?}", log.errors());
        drop(client);
        drop(server);
        drop(beacon);
    }

    /// m1 повторного ревью: нулевая резервная проверка -- цикл без сна,
    /// отвергается при подключении.
    #[test]
    fn zero_discovery_fallback_is_rejected() {
        let options = AutoOptions {
            discovery_fallback: Some(Duration::ZERO),
            ..AutoOptions::default()
        };
        assert!(matches!(
            AutoClient::connect(&unique("ZERO"), Arc::new(Log::default()), options).err(),
            Some(ShmError::InvalidConfig(_))
        ));
        assert!(matches!(
            crate::multi::MultiClient::connect(
                &unique("ZERO_MULTI"),
                Arc::new(ZeroMulti),
                crate::multi::MultiClientOptions {
                    discovery_fallback: Some(Duration::ZERO),
                    ..crate::multi::MultiClientOptions::default()
                },
            )
            .err(),
            Some(ShmError::InvalidConfig(_))
        ));
        assert!(matches!(
            SharedClient::wait_for_server_with(
                &unique("ZERO_WAIT"),
                Some(Duration::from_millis(10)),
                &crate::DiscoveryOptions::default().fallback(Some(Duration::ZERO)),
            ),
            Err(ShmError::InvalidConfig(_))
        ));
        // `None` -- только события, допустимо.
        let client = AutoClient::connect(
            &unique("NONE"),
            Arc::new(Log::default()),
            AutoOptions {
                discovery_fallback: None,
                ..AutoOptions::default()
            },
        )
        .unwrap();
        drop(client);
    }

    struct ZeroMulti;
    impl crate::multi::MultiClientHandler for ZeroMulti {
        fn on_connect(&self, _slot_id: u32) {}
        fn on_disconnect(&self) {}
        fn on_message(&self, _data: &[u8]) {}
    }
}
