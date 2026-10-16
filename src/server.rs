use std::os::windows::io::{AsHandle, BorrowedHandle};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::constants::{HANDSHAKE_CLIENT_HELLO, HANDSHAKE_IDLE, HANDSHAKE_SERVER_READY};
use crate::error::{Result, ShmError};
use crate::events::SharedEvents;
use crate::layout::ChannelLayout;
use crate::namespace::PrivateNamespace;
use crate::naming::ObjectPath;
use crate::ring::{
    DisconnectWatch, FreeSpace, RingBuffer, SpinPolicy, UnreadProbe, WriteOutcome, deadline_of,
    spin_until,
};
use crate::security::{ObjectSecurity, SecurityAttributes};
use crate::shared::SharedView;
use crate::win::{Mapping, ProcessWatch, wait_any};

/// Дедлайн ожидания клиента: повторные ожидания после ложного
/// `CONNECT_REQ` получают остаток исходного таймаута, а не новый целиком.
struct Deadline(Option<Instant>);

impl Deadline {
    /// `None` -- без таймаута; таймаут, не представимый как `Instant`
    /// (переполнение), -- тоже без таймаута (ждать «вечно» и так).
    fn new(timeout: Option<Duration>) -> Self {
        Self(timeout.and_then(|t| Instant::now().checked_add(t)))
    }

    /// Остаток до дедлайна для очередного ожидания (`None` -- без таймаута).
    /// Истёкший дедлайн даёт ноль: ожидание вернётся сразу (опрос).
    fn remaining(&self) -> Option<Duration> {
        self.0
            .map(|at| at.saturating_duration_since(Instant::now()))
    }
}

/// Параметры канала для [`SharedServer::start_with`]: раскладка, политика
/// безопасности объектов и частное пространство имён.
///
/// `#[non_exhaustive]`: строится от `ChannelConfig::default()` (раскладка
/// `ChannelLayout::DEFAULT`, политика `ObjectSecurity::CurrentUser`, без
/// пространства) и методами [`ChannelConfig::layout`]/
/// [`ChannelConfig::security`]/[`ChannelConfig::namespace`]; новые
/// параметры добавляются без поломки вызывающих.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ChannelConfig {
    /// Раскладка канала (размеры колец, лимиты по направлениям).
    pub layout: ChannelLayout,
    /// Кто получает доступ к секции и событиям канала.
    pub security: ObjectSecurity,
    /// Частное пространство имён канала (0.10): `Some` -- секция и события
    /// создаются в нём под относительными именами (имя канала -- без
    /// `Local\`/`Global\`, иначе `InvalidConfig`), сервер держит ссылку на
    /// пространство, пока жив. `None` (по умолчанию) -- общий
    /// `\BaseNamedObjects`, как до 0.10.
    pub namespace: Option<Arc<PrivateNamespace>>,
}

impl ChannelConfig {
    /// Параметры с раскладкой `layout` и политикой по умолчанию.
    #[must_use]
    pub fn new(layout: ChannelLayout) -> Self {
        Self {
            layout,
            security: ObjectSecurity::default(),
            namespace: None,
        }
    }

    /// Заменить раскладку.
    #[must_use]
    pub const fn layout(mut self, layout: ChannelLayout) -> Self {
        self.layout = layout;
        self
    }

    /// Заменить политику безопасности.
    #[must_use]
    pub fn security(mut self, security: ObjectSecurity) -> Self {
        self.security = security;
        self
    }

    /// Создавать объекты канала в частном пространстве имён `namespace`
    /// (0.10, см. [`PrivateNamespace`]). Клиент подключается через
    /// [`SharedClient::connect_in`](crate::SharedClient::connect_in) с
    /// пространством той же границы.
    #[must_use]
    pub fn namespace(mut self, namespace: Arc<PrivateNamespace>) -> Self {
        self.namespace = Some(namespace);
        self
    }
}

#[derive(Debug)]
pub struct SharedServer {
    // Порядок полей важен для Drop: кольца и view ссылаются на память
    // `_mapping`, поэтому он объявлен последним и размапливается последним.
    view: SharedView,
    events: Option<SharedEvents>, // None для anonymous режима
    ring_tx: RingBuffer,
    ring_rx: RingBuffer,
    connected: bool,
    /// Удерживаемый handle процесса клиента (если клиент 0.8+ передал PID и
    /// процесс удалось открыть) -- для детекции его смерти.
    peer: Option<ProcessWatch>,
    /// Время создания процесса клиента, сообщённое им в рукопожатии и
    /// сверенное при открытии `peer` (`0` -- клиент время не сообщил, сверки
    /// не было; см. `verified_peer_pid`).
    peer_start: u64,
    _mapping: Mapping,
    /// Частное пространство имён канала (0.10): держим, пока живы объекты
    /// -- с последним handle пространство исчезает, и клиент не нашёл бы
    /// канал. Последним: закрывается после секции и событий.
    _namespace: Option<Arc<PrivateNamespace>>,
}

// SAFETY: все поля либо владеющие (`Mapping`), либо синхронизируются через
// атомарные операции в shared memory; привязки к потоку-создателю нет.
unsafe impl Send for SharedServer {}

impl SharedServer {
    /// Сервер канала `name` с раскладкой по умолчанию (`ChannelLayout::DEFAULT`).
    pub fn start(name: &str) -> Result<Self> {
        Self::start_with(name, &ChannelConfig::default())
    }

    /// Сервер канала `name` с параметрами `config`: раскладка каждого
    /// направления и политика безопасности объектов канала (секция и 7
    /// событий).
    ///
    /// Клиент подстраивается сам: раскладка записана в сегменте
    /// (`SharedClient::layout`). `Err(InvalidConfig)` -- `config.layout` не
    /// прошёл `ChannelLayout::validate` или `ObjectSecurity::Descriptor`
    /// некорректен (оба -- до создания объектов).
    ///
    /// ```no_run
    /// use xshm::{ChannelConfig, ChannelLayout, ObjectSecurity, RingConfig, SharedServer};
    ///
    /// let config = ChannelConfig::default()
    ///     .layout(ChannelLayout::symmetric(RingConfig::new(64 << 10)))
    ///     .security(ObjectSecurity::Everyone); // служба SYSTEM <-> пользователь
    /// let server = SharedServer::start_with("Global\\MyService", &config)?;
    /// # Ok::<(), xshm::ShmError>(())
    /// ```
    pub fn start_with(name: &str, config: &ChannelConfig) -> Result<Self> {
        Self::start_slot(
            config.namespace.as_ref(),
            name,
            config.layout,
            0,
            &config.security.attributes()?,
        )
    }

    /// Сервер канала `name`, публикующий в `ControlBlock::slot_count` число
    /// `slot_count` (Multi: `max_clients`; `0` -- не Multi). Дескриптор
    /// `security` -- общий для секции и всех событий канала (Multi строит его
    /// один раз на все слоты).
    pub(crate) fn start_slot(
        namespace: Option<&Arc<PrivateNamespace>>,
        name: &str,
        layout: ChannelLayout,
        slot_count: u32,
        security: &SecurityAttributes,
    ) -> Result<Self> {
        layout.validate()?;
        let root = namespace.map(Arc::as_ref);
        let mapping = Mapping::create(
            ObjectPath::mapping(root, name),
            layout.segment_size(),
            security,
        )?;
        let mut server = Self::from_mapping(mapping, layout, slot_count)?;
        server.events = Some(SharedEvents::create(root, name, security)?);
        server._namespace = namespace.cloned();
        Ok(server)
    }

    /// Создание anonymous сервера без имени (только через handle)
    ///
    /// Anonymous сервер создает section без имени в глобальном namespace.
    /// Доступ к section возможен только через handle, что идеально для
    /// передачи handle в kernel driver. Events не создаются, используется
    /// polling-based handshake через `wait_for_client_noevent()`.
    ///
    /// **ВАЖНО:** Это НЕ то же самое, что `start("")` (пустая строка).
    /// Windows NT API требует именно `ObjectName = NULL` в `OBJECT_ATTRIBUTES`
    /// для создания anonymous section. Пустая строка через `NtName::new("")`
    /// создаст `UNICODE_STRING` с путем `"\\BaseNamedObjects\\"`, что является
    /// именованной секцией, а не anonymous. Поэтому нужна отдельная функция.
    pub fn start_anonymous() -> Result<Self> {
        Self::start_anonymous_with_layout(ChannelLayout::DEFAULT)
    }

    /// Anonymous-сервер с заданной раскладкой (см. `start_anonymous`).
    /// Драйвер читает раскладку из `LayoutBlock` сегмента (INTEROP §c).
    ///
    /// 0.10: дескриптор секции -- политика по умолчанию
    /// (`ObjectSecurity::CurrentUser`) вместо NULL DACL. По имени секцию не
    /// открыть; дубликат handle получает права исходного handle без
    /// проверки DACL, поэтому политика здесь не настраивается.
    pub fn start_anonymous_with_layout(layout: ChannelLayout) -> Result<Self> {
        layout.validate()?;
        let security = ObjectSecurity::default().attributes()?;
        let mapping = Mapping::create_anonymous(layout.segment_size(), &security)?;
        Self::from_mapping(mapping, layout, 0)
    }

    /// Инициализация свежей секции: сегмент публикуется (`magic`) до
    /// создания событий -- клиент, открывший секцию раньше событий, получит
    /// ошибку открытия событий и повторит попытку.
    fn from_mapping(mapping: Mapping, layout: ChannelLayout, slot_count: u32) -> Result<Self> {
        // SAFETY: `Mapping` только что создал отображение размером не меньше
        // `layout.segment_size()` (адрес выровнен на гранулу выделения) и
        // держит его живым, пока жив сам, а он переезжает в возвращаемую
        // структуру; layout проверен вызывающим.
        let view = unsafe {
            SharedView::initialize(
                mapping.as_ptr(),
                layout,
                std::process::id(),
                crate::win::current_process_start_time(),
                slot_count,
            )
        }?;
        let ring_tx = view.ring_a();
        let ring_rx = view.ring_b();
        Ok(Self {
            view,
            events: None,
            ring_tx,
            ring_rx,
            connected: false,
            peer: None,
            peer_start: 0,
            _namespace: None,
            _mapping: mapping,
        })
    }

    /// Раскладка канала (параметры обоих направлений).
    #[must_use]
    pub const fn layout(&self) -> &ChannelLayout {
        self.view.layout()
    }

    /// Получить handles событий для передачи в kernel driver
    ///
    /// Возвращает `None` если сервер создан в anonymous режиме (без событий).
    /// В этом случае используется polling через `wait_for_client_noevent()`.
    #[must_use]
    pub fn get_event_handles(&self) -> Option<crate::events::EventHandles> {
        self.events.as_ref().map(|e| e.get_event_handles())
    }

    pub fn wait_for_client(&mut self, timeout: Option<Duration>) -> Result<()> {
        if self.connected {
            return Err(ShmError::AlreadyConnected);
        }

        // Для anonymous режима используем polling
        let Some(events) = self.events.as_ref() else {
            return self.wait_for_client_noevent(timeout);
        };

        // Находка 21: `CONNECT_REQ` без `CLIENT_HELLO` (толчок Multi,
        // клиент, откатившийся по таймауту, чужой процесс) больше не обрывает
        // ожидание `HandshakeFailed`: автосбросный сигнал поглощён, ждём
        // дальше остаток таймаута.
        let deadline = Deadline::new(timeout);
        loop {
            if !events.connect_req.wait(deadline.remaining())? {
                return Err(ShmError::Timeout);
            }
            if self.hello_pending() {
                return self.complete_handshake();
            }
        }
    }

    /// Клиент выставил заявку (`client_state == CLIENT_HELLO`). Acquire:
    /// PID/время клиента, записанные до заявки, видны `complete_handshake`.
    fn hello_pending(&self) -> bool {
        self.view
            .control_block()
            .client_state
            .load(Ordering::Acquire)
            == HANDSHAKE_CLIENT_HELLO
    }

    /// Ждать клиента ИЛИ стороннее событие `wake` (дескриптор события этого
    /// процесса: команда worker-у, остановка). `Ok(true)` -- клиент
    /// подключён (handshake завершён); `Ok(false)` -- сработал `wake` или
    /// истёк `timeout` (`None` -- без таймаута). Только именованный сервер.
    pub(crate) fn wait_for_client_or(
        &mut self,
        wake: BorrowedHandle<'_>,
        timeout: Option<Duration>,
    ) -> Result<bool> {
        if self.connected {
            return Err(ShmError::AlreadyConnected);
        }
        let Some(events) = self.events.as_ref() else {
            return Err(ShmError::InvalidConfig(
                "anonymous server has no events to wait on",
            ));
        };
        // Находка 21: `CONNECT_REQ` без `CLIENT_HELLO` -- не ошибка и не
        // повод вернуть управление: сигнал поглощён, ждём остаток таймаута
        // (как в `wait_for_client`). Наружу выходят только подключение,
        // `wake`, таймаут и ошибки самого рукопожатия.
        let deadline = Deadline::new(timeout);
        loop {
            // CONNECT_REQ первым: при одновременном сигнале клиент важнее.
            match wait_any(
                &[events.connect_req.as_handle(), wake],
                deadline.remaining(),
            )? {
                Some(0) => {}
                _ => return Ok(false),
            }
            if self.hello_pending() {
                self.complete_handshake()?;
                return Ok(true);
            }
        }
    }

    /// Принять клиента, чей `CONNECT_REQ` уже получен вызывающим (Multi ждёт
    /// события сразу всех слотов в одном наборе). `Err(HandshakeFailed)` --
    /// `client_state` не `CLIENT_HELLO` (сигнал без заявки): здесь ошибка
    /// остаётся -- ожидания нет, а Multi по ней отличает «толчок»
    /// (`nudge_stale_claims`) и откат клиента и молча продолжает свой цикл.
    pub(crate) fn accept_pending(&mut self) -> Result<()> {
        if self.connected {
            return Err(ShmError::AlreadyConnected);
        }
        if !self.hello_pending() {
            return Err(ShmError::HandshakeFailed);
        }
        self.complete_handshake()
    }

    /// Ожидание клиента без событий (polling по shared memory).
    /// Использовать когда нет доступа к именованным событиям.
    pub fn wait_for_client_noevent(&mut self, timeout: Option<Duration>) -> Result<()> {
        if self.connected {
            return Err(ShmError::AlreadyConnected);
        }

        let control = self.view.control_block();
        let start = Instant::now();

        loop {
            let client_state = control.client_state.load(Ordering::Acquire);
            if client_state == HANDSHAKE_CLIENT_HELLO {
                break;
            }

            if let Some(t) = timeout
                && start.elapsed() >= t
            {
                return Err(ShmError::Timeout);
            }

            std::thread::sleep(Duration::from_millis(1));
        }

        self.complete_handshake()
    }

    /// Завершение серверной стороны handshake, общее для событийного и
    /// polling-путей: сброс колец -> публикация нового generation ->
    /// SERVER_READY -> фиксация CAS `client_state: CLIENT_HELLO ->
    /// SERVER_READY` -> сброс `DISCONNECT` -> сигнал connect_ack.
    ///
    /// Порядок критичен: буферы сбрасываются ДО публикации generation
    /// (`Release`), поэтому клиент, прочитавший новый generation (`Acquire`),
    /// гарантированно видит уже очищенные кольца.
    ///
    /// Ревизия 2 (25.09.2026, INTEROP R16): рукопожатие фиксирует CAS
    /// `client_state`, а не store. Клиент, у которого истёк таймаут, отзывает
    /// заявку встречным CAS `CLIENT_HELLO -> IDLE`; проиграл сервер --
    /// рукопожатия нет: `server_state`/заголовки обратно в `IDLE`,
    /// `S2C_CONNECT` НЕ сигналится (раньше он оставался взведённым для
    /// следующего клиента), `Err(HandshakeFailed)`. Новый `generation` при
    /// этом остаётся опубликованным (кольца пусты, клиентов нет) -- клиент
    /// отличает «принят» по смене generation только вместе с проигранным
    /// отзывом, так что лишний инкремент никого не вводит в заблуждение.
    fn complete_handshake(&mut self) -> Result<()> {
        let control = self.view.control_block();
        // Протокол 0.8+: PID клиента, записанный им ДО CLIENT_HELLO (видим
        // благодаря Acquire-чтению client_state в wait_for_client*). `swap(0)`:
        // значение одноразовое -- клиент старой версии, пришедший следом,
        // не унаследует чужой PID. 0 -- клиент старой версии, наблюдения нет.
        let client_pid = control.client_pid.swap(0, Ordering::AcqRel);
        // Находка 23: время создания процесса клиента -- парой с PID и тоже
        // одноразово; половины видны после AcqRel-`swap` выше (клиент пишет
        // их до `client_pid`). `0` -- клиент время не сообщил.
        let client_start = control.take_client_start();
        let new_generation = control.generation.load(Ordering::Acquire).wrapping_add(1);

        let (header_a, header_b) = self.view.headers();
        header_a.reset(new_generation);
        header_b.reset(new_generation);

        control.generation.store(new_generation, Ordering::Release);

        header_a
            .handshake_state
            .store(HANDSHAKE_SERVER_READY, Ordering::Release);
        header_b
            .handshake_state
            .store(HANDSHAKE_SERVER_READY, Ordering::Release);

        control
            .server_state
            .store(HANDSHAKE_SERVER_READY, Ordering::Release);
        // Точка фиксации (ревизия 2): заявка ещё стоит -- рукопожатие наше.
        if control
            .client_state
            .compare_exchange(
                HANDSHAKE_CLIENT_HELLO,
                HANDSHAKE_SERVER_READY,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            // Клиент отозвал заявку (таймаут/остановка) раньше нас.
            control
                .server_state
                .store(HANDSHAKE_IDLE, Ordering::Release);
            header_a
                .handshake_state
                .store(HANDSHAKE_IDLE, Ordering::Release);
            header_b
                .handshake_state
                .store(HANDSHAKE_IDLE, Ordering::Release);
            return Err(ShmError::HandshakeFailed);
        }
        self.peer = ProcessWatch::open_peer(client_pid, client_start);
        self.peer_start = if self.peer.is_some() { client_start } else { 0 };

        // Для anonymous-сервера событий нет -- клиент узнаёт о готовности
        // опросом `server_state`.
        if let Some(events) = &self.events {
            // 0.9: `DISCONNECT` -- одно автосбросное событие на обе стороны;
            // сигнал, оставшийся от прошлой сессии (никто его не дождался),
            // иначе мгновенно «отключил» бы нового клиента. Сбрасываем ДО
            // `S2C_CONNECT`: клиент начнёт ждать `DISCONNECT` только после него.
            let _ = events.disconnect.reset();
            events.connect_ack.set()?;
        }
        self.connected = true;
        Ok(())
    }

    #[must_use]
    pub const fn is_connected(&self) -> bool {
        self.connected
    }

    /// Доступ к событиям сервера (для внутреннего использования)
    ///
    /// ВАЖНО: Для anonymous режима возвращает None - события не создаются.
    /// Используйте только для named режима (SharedServer::start).
    #[must_use]
    pub const fn events(&self) -> Option<&SharedEvents> {
        self.events.as_ref()
    }

    /// Проверка, является ли сервер anonymous (без имени и events)
    #[must_use]
    pub const fn is_anonymous(&self) -> bool {
        self.events.is_none()
    }

    /// Получить raw HANDLE секции для передачи в kernel driver
    /// ВАЖНО: Handle принадлежит Mapping, не закрывать вручную!
    #[must_use]
    pub fn section_handle(&self) -> isize {
        self._mapping.section_handle()
    }

    /// Доступ к shared view (для внутреннего использования)
    pub(crate) const fn view(&self) -> &SharedView {
        &self.view
    }

    pub(crate) fn mark_disconnected(&mut self) {
        self.connected = false;
        // Закрываем handle процесса бывшего клиента (RAII).
        self.peer = None;
        self.peer_start = 0;

        // Сбрасываем состояние в shared memory для возможности reconnect
        let control = self.view.control_block();
        control
            .server_state
            .store(HANDSHAKE_IDLE, Ordering::Release);
        // 0.9: CAS, а не store -- сбрасываем только состояние СВОЕЙ сессии
        // (`SERVER_READY`). Если следующий клиент уже успел написать
        // `CLIENT_HELLO` (лобби: он вошёл сразу, как прежний отпустил
        // мьютекс лобби), безусловный store стёр бы его заявку, и сервер
        // принял бы его `CONNECT_REQ` за ложный.
        let _ = control.client_state.compare_exchange(
            HANDSHAKE_SERVER_READY,
            HANDSHAKE_IDLE,
            Ordering::AcqRel,
            Ordering::Acquire,
        );

        let (header_a, header_b) = self.view.headers();
        header_a
            .handshake_state
            .store(HANDSHAKE_IDLE, Ordering::Release);
        header_b
            .handshake_state
            .store(HANDSHAKE_IDLE, Ordering::Release);
    }

    const fn ensure_connected(&self) -> Result<()> {
        if !self.connected {
            Err(ShmError::NotConnected)
        } else {
            Ok(())
        }
    }

    pub fn send_to_client(&self, payload: &[u8]) -> Result<WriteOutcome> {
        self.ensure_connected()?;
        let result = self.ring_tx.write_message(payload)?;
        // Сигнализируем только если events доступны
        if let Some(ref events) = self.events
            && result.was_empty
        {
            let _ = events.s2c.data.set();
        }
        Ok(result)
    }

    /// Отправка клиенту **без перезаписи** непрочитанных данных.
    ///
    /// Сообщение либо целиком ложится в свободное место кольца
    /// Server -> Client, либо возвращается `Err(ShmError::QueueFull)` и
    /// кольцо не меняется. Подробная семантика -- `RingBuffer::try_write_message`
    /// (атомарность, консервативность проверки, `MessageTooSmall`/`MessageTooLarge`).
    /// Ждать освобождения места -- `wait_for_space`.
    pub fn try_send_to_client(&self, payload: &[u8]) -> Result<WriteOutcome> {
        self.ensure_connected()?;
        let result = self.ring_tx.try_write_message(payload)?;
        if let Some(ref events) = self.events
            && result.was_empty
        {
            let _ = events.s2c.data.set();
        }
        Ok(result)
    }

    /// Свободное место в кольце Server -> Client.
    ///
    /// Из потока, который пишет в это кольцо, -- нижняя граница: если
    /// `free_space().fits(n)`, следующий `try_send_to_client` с payload длины
    /// `n` пройдёт. Вне подключения возвращает `FreeSpace::ZERO`.
    #[must_use]
    pub fn free_space(&self) -> FreeSpace {
        if self.connected {
            self.ring_tx.free_space()
        } else {
            FreeSpace::ZERO
        }
    }

    /// Ждать, пока в кольце Server -> Client освободится место под payload
    /// длины `payload_len`. `Ok(true)` -- место есть, `Ok(false)` -- таймаут;
    /// `timeout = None` -- без ограничения (мёртвый клиент место не
    /// освободит -- лучше задавать таймаут и параллельно следить за живостью).
    ///
    /// 0.9: одно ожидание в ядре на весь `timeout`, без нарезки на срезы, по
    /// набору `[S2C_SPACE, S2C_DISCONNECT, процесс клиента]`:
    /// - `SPACE` -- клиент снял заявку (места хватает) или опустошил кольцо
    ///   (клиент старой версии, не знающий заявку, будит только так);
    /// - `DISCONNECT` при `client_state != SERVER_READY` -- клиент штатно
    ///   ушёл: `Err(NotConnected)`, событие возвращается взведённым;
    /// - процесс клиента сигнален (клиент 0.8+ передал PID) -- `Err(PeerDied)`.
    ///
    /// Клиент без PID (старая версия), упавший без `DISCONNECT`, не будит
    /// ничем: единственная граница тогда -- `timeout` вызывающего. Без
    /// событий (anonymous) -- опрос с шагом 1 мс.
    pub fn wait_for_space(&self, payload_len: usize, timeout: Option<Duration>) -> Result<bool> {
        self.wait_for_space_spin(payload_len, timeout, SpinPolicy::NONE)
    }

    /// `wait_for_space` со спином перед ожиданием в ядре (0.10): до
    /// `spin.iterations` перепроверок места по позициям (заявка
    /// `space_waiter` на время спина не ставится), затем обычное ожидание.
    /// При `timeout = Some(0)` спина нет; дедлайн `timeout` спин сверяет раз
    /// в 64 итерации (перебег -- не больше 64 итераций). `SpinPolicy::NONE` -- то же, что
    /// `wait_for_space`.
    pub fn wait_for_space_spin(
        &self,
        payload_len: usize,
        timeout: Option<Duration>,
        spin: SpinPolicy,
    ) -> Result<bool> {
        self.ensure_connected()?;
        let control = self.view.control_block();
        let peer_left = || control.client_state.load(Ordering::Acquire) != HANDSHAKE_SERVER_READY;
        let disconnect = self.events.as_ref().map(|e| DisconnectWatch {
            event: &e.disconnect,
            peer_left: &peer_left,
        });
        let event = self.events.as_ref().map(|e| &e.s2c.space);
        self.ring_tx.wait_for_space(
            payload_len,
            event,
            self.peer.as_ref(),
            disconnect.as_ref(),
            timeout,
            spin,
        )
    }

    /// Для auto-режима: заявка/снятие ожидания места в исходящем кольце.
    pub(crate) fn arm_space_waiter(&self, frame: u32) {
        self.ring_tx.arm_space_waiter(frame);
    }

    pub(crate) fn disarm_space_waiter(&self) {
        self.ring_tx.disarm_space_waiter();
    }

    pub fn receive_from_client(&self, buffer: &mut Vec<u8>) -> Result<usize> {
        self.ensure_connected()?;
        let len = self.ring_rx.read_message(buffer)?;
        // Сигнализируем только если events доступны. SPACE -- когда кольцо
        // опустело (как раньше) ИЛИ когда писатель оставил заявку и места
        // ему теперь хватает (`try_send`/`wait_for_space` на той стороне).
        if let Some(ref events) = self.events
            && (self.ring_rx.take_space_waiter() || self.ring_rx.is_empty())
        {
            let _ = events.c2s.space.set();
        }
        Ok(len)
    }

    /// Ждать данных от клиента. `Ok(true)` -- в кольце есть сообщение,
    /// `Ok(false)` -- таймаут (или anonymous-режим без данных),
    /// `Err(PeerDied)` -- процесс клиента умер и всё, что он успел записать,
    /// уже прочитано (пока в кольце есть данные, возвращается `Ok(true)`).
    pub fn poll_client(&self, timeout: Option<Duration>) -> Result<bool> {
        self.poll_client_spin(timeout, SpinPolicy::NONE)
    }

    /// `poll_client` со спином перед ожиданием в ядре (0.10): до
    /// `spin.iterations` проверок кольца по позициям, затем обычное
    /// ожидание `DATA`. Спин сокращает задержку, когда клиент отвечает
    /// быстрее пробуждения из ядра, ценой занятого ядра CPU на время спина;
    /// при `timeout = Some(0)` спина нет; дедлайн `timeout` спин сверяет раз
    /// в 64 итерации (перебег -- не больше 64 итераций). `SpinPolicy::NONE` -- то же, что
    /// `poll_client`.
    pub fn poll_client_spin(&self, timeout: Option<Duration>, spin: SpinPolicy) -> Result<bool> {
        self.ensure_connected()?;
        // Для anonymous режима просто проверяем буфер (polling)
        let Some(events) = self.events.as_ref() else {
            if !self.ring_rx.is_empty()
                || (timeout != Some(Duration::ZERO)
                    && spin_until(spin, deadline_of(timeout), || self.ring_rx.has_unread()))
            {
                return Ok(true);
            }
            return match &self.peer {
                Some(peer) if peer.has_exited() && self.ring_rx.is_empty() => {
                    Err(ShmError::PeerDied)
                }
                _ => Ok(false), // Нет данных, но не timeout
            };
        };
        self.ring_rx
            .wait_for_data(&events.c2s.data, self.peer.as_ref(), timeout, spin)
    }

    /// Worker-ы (спин перед сном, 0.10): в кольце от клиента есть
    /// непрочитанное -- по позициям, без барьера.
    pub(crate) fn has_unread_from_client(&self) -> bool {
        self.ring_rx.has_unread()
    }

    /// Multi: проба кольца от клиента для спина worker-а группы.
    pub(crate) const fn client_ring_probe(&self) -> UnreadProbe<'_> {
        self.ring_rx.unread_probe()
    }

    /// PID процесса клиента, если клиент 0.8+ передал его в handshake.
    #[must_use]
    pub fn peer_pid(&self) -> Option<u32> {
        self.peer.as_ref().map(ProcessWatch::pid)
    }

    /// PID клиента, только если он сообщил и время создания процесса
    /// (ненулевое), и оно совпало с процессом, открытым по этому PID
    /// (`ProcessWatch::open_peer`). Это **сверка самоописания** пира с
    /// существующим процессом, а не доказательство личности: PID и время
    /// пир пишет в разделяемую память сам, а время чужого процесса читается
    /// с `PROCESS_QUERY_LIMITED_INFORMATION`. Клиент без времени (`0`) --
    /// `None`, «не сверен» (0.10, Dispatch: сверка PID канала).
    pub(crate) fn verified_peer_pid(&self) -> Option<u32> {
        self.peer
            .as_ref()
            .filter(|_| self.peer_start != 0)
            .map(ProcessWatch::pid)
    }

    /// Жив ли процесс клиента: `Some(true/false)` -- клиент наблюдается через
    /// удерживаемый handle процесса; `None` -- не подключён или наблюдения
    /// нет (клиент старой версии, не передал PID, или handle не открылся --
    /// например, клиент в другой сессии без прав). Один syscall, без блокировки.
    #[must_use]
    pub fn is_peer_alive(&self) -> Option<bool> {
        if !self.connected {
            return None;
        }
        self.peer.as_ref().map(|peer| !peer.has_exited())
    }

    /// Handle процесса клиента для набора ожидания worker-а (auto-режим).
    pub(crate) fn peer_wait_handle(&self) -> Option<BorrowedHandle<'_>> {
        self.peer.as_ref().map(AsHandle::as_handle)
    }

    /// Забрать handle процесса клиента, не закрывая его (Multi: handle может
    /// быть в наборе ожидания worker-а, который ещё спит, -- закрывать
    /// ожидаемый handle нельзя, worker закроет его сам после пробуждения).
    pub(crate) const fn take_peer(&mut self) -> Option<ProcessWatch> {
        self.peer_start = 0;
        self.peer.take()
    }
}

impl Drop for SharedServer {
    fn drop(&mut self) {
        let control = self.view.control_block();
        control
            .server_state
            .store(HANDSHAKE_IDLE, Ordering::Release);
        control
            .client_state
            .store(HANDSHAKE_IDLE, Ordering::Release);
        let (header_a, header_b) = self.view.headers();
        header_a
            .handshake_state
            .store(HANDSHAKE_IDLE, Ordering::Release);
        header_b
            .handshake_state
            .store(HANDSHAKE_IDLE, Ordering::Release);
        if self.connected
            && let Some(ref events) = self.events
        {
            let _ = events.disconnect.set();
        }
    }
}

#[cfg(test)]
mod handshake_tests {
    //! Ревизия 2 (дефект 5): рукопожатие фиксируется CAS-ами `client_state`
    //! с обеих сторон -- откат клиента и ответ сервера не перетирают друг
    //! друга, `S2C_CONNECT` не остаётся взведённым для следующего клиента.

    use super::*;
    use crate::client::SharedClient;

    fn unique(tag: &str) -> String {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("XSHM_HS_{tag}_{}_{}", std::process::id(), ts % 1_000_000)
    }

    /// Клиент отозвал заявку (CAS `HELLO -> IDLE`) раньше, чем сервер её
    /// зафиксировал: рукопожатия нет, `S2C_CONNECT` не сигналится.
    #[test]
    fn server_does_not_commit_a_withdrawn_hello() {
        let name = unique("WITHDRAWN");
        let mut server = SharedServer::start(&name).unwrap();
        let generation = server
            .view()
            .control_block()
            .generation
            .load(Ordering::Acquire);
        // Заявка уже отозвана к моменту фиксации (клиентский CAS выиграл).
        server
            .view()
            .control_block()
            .client_state
            .store(HANDSHAKE_IDLE, Ordering::Release);
        assert_eq!(server.complete_handshake(), Err(ShmError::HandshakeFailed));
        assert!(!server.is_connected());
        let control = server.view().control_block();
        assert_eq!(control.server_state.load(Ordering::Acquire), HANDSHAKE_IDLE);
        assert_eq!(control.client_state.load(Ordering::Acquire), HANDSHAKE_IDLE);
        assert_ne!(control.generation.load(Ordering::Acquire), generation);
        let ack = &server.events().unwrap().connect_ack;
        assert!(
            !ack.wait(Some(Duration::ZERO)).unwrap(),
            "S2C_CONNECT взведён для никого"
        );
    }

    /// Устаревший `S2C_CONNECT` (сервер старой версии ответил клиенту,
    /// который уже откатился) не подтверждает заявку следующего клиента:
    /// клиент сбрасывает событие до `HELLO` и засчитывает подключение
    /// только по смене `generation`.
    #[test]
    fn stale_connect_ack_does_not_confirm_the_next_client() {
        let name = unique("STALEACK");
        let server = SharedServer::start(&name).unwrap();
        server.events().unwrap().connect_ack.set().unwrap();
        let err = SharedClient::connect(&name, Duration::from_millis(100)).unwrap_err();
        assert_eq!(err, ShmError::Timeout);
        let control = server.view().control_block();
        assert_eq!(control.client_state.load(Ordering::Acquire), HANDSHAKE_IDLE);
        assert_eq!(control.client_pid.load(Ordering::Acquire), 0);
    }

    /// Сервер принял клиента и сразу отключил (Drop) до того, как клиент
    /// дождался `S2C_CONNECT`: подключение засчитано по `generation`,
    /// `DISCONNECT` взведён, прощальное сообщение в кольце.
    #[test]
    fn client_sees_a_handshake_even_if_the_server_already_left() {
        let name = unique("GONE");
        let mut server = SharedServer::start(&name).unwrap();
        let accept = std::thread::spawn(move || {
            server
                .wait_for_client(Some(Duration::from_secs(5)))
                .unwrap();
            server.send_to_client(b"bye").unwrap();
            drop(server); // IDLE -> DISCONNECT
        });
        // Даём серверу сесть на `CONNECT_REQ`, затем подключаемся; к
        // моменту проверки сервер может быть уже в IDLE.
        std::thread::sleep(Duration::from_millis(30));
        let client = SharedClient::connect(&name, Duration::from_secs(5)).unwrap();
        accept.join().unwrap();
        assert!(client.is_connected());
        assert!(
            client
                .events()
                .disconnect
                .wait(Some(Duration::from_secs(1)))
                .unwrap()
        );
        let mut buf = Vec::new();
        let len = client.receive_from_server(&mut buf).unwrap();
        assert_eq!(&buf[..len], b"bye");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::SharedClient;

    fn unique(tag: &str) -> String {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("XSHM_SRV_{tag}_{}_{}", std::process::id(), ts % 1_000_000)
    }

    /// Находка 21: `CONNECT_REQ` без `CLIENT_HELLO` (толчок, откатившийся
    /// клиент, чужой процесс) не обрывает ожидание `HandshakeFailed` --
    /// сервер дожидается настоящего клиента в пределах таймаута.
    #[test]
    fn stray_connect_req_does_not_fail_wait() {
        let name = unique("STRAY");
        let mut server = SharedServer::start(&name).unwrap();
        let waiter = std::thread::spawn(move || {
            let result = server.wait_for_client(Some(Duration::from_secs(2)));
            (result, server)
        });
        // Даём серверу сесть на `CONNECT_REQ`, затем ложный сигнал без HELLO.
        std::thread::sleep(Duration::from_millis(30));
        let events = SharedEvents::open(&name).unwrap();
        events.connect_req.set().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let client = SharedClient::connect(&name, Duration::from_secs(2)).unwrap();
        let (result, server) = waiter.join().unwrap();
        assert_eq!(result, Ok(()));
        assert!(server.is_connected());
        assert!(client.is_connected());
    }

    /// То же для `wait_for_client_or` (worker Auto, лобби Dispatch): ложный
    /// сигнал не даёт ни ошибки, ни `Ok(false)` -- ожидание продолжается.
    #[test]
    fn stray_connect_req_does_not_end_wait_or() {
        let name = unique("STRAYOR");
        let mut server = SharedServer::start(&name).unwrap();
        let waiter = std::thread::spawn(move || {
            let wake = crate::win::EventHandle::create_unnamed(false).unwrap();
            let result = server.wait_for_client_or(wake.as_handle(), Some(Duration::from_secs(2)));
            (result, server)
        });
        std::thread::sleep(Duration::from_millis(30));
        let events = SharedEvents::open(&name).unwrap();
        events.connect_req.set().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let client = SharedClient::connect(&name, Duration::from_secs(2)).unwrap();
        let (result, server) = waiter.join().unwrap();
        assert_eq!(result, Ok(true));
        assert!(server.is_connected());
        assert!(client.is_connected());
    }

    /// Ложный сигнал при коротком таймауте: остаток пересчитывается, итог --
    /// `Timeout`, а не `HandshakeFailed` и не ожидание сверх дедлайна.
    #[test]
    fn stray_connect_req_keeps_the_deadline() {
        let name = unique("STRAYDL");
        let mut server = SharedServer::start(&name).unwrap();
        server.events().unwrap().connect_req.set().unwrap();
        let started = Instant::now();
        let result = server.wait_for_client(Some(Duration::from_millis(200)));
        let elapsed = started.elapsed();
        assert_eq!(result, Err(ShmError::Timeout));
        assert!(elapsed >= Duration::from_millis(190), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
    }
}
