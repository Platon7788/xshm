# CHANGELOG — xshm

Журнал изменений (до 0.9.0 вёлся в неотслеживаемом `Docs/CHANGELOG.md`;
теперь канонический — этот файл).

## [0.10.0] — не выпущено (ветка `feat/layout-v2`)

Аудит качества 16.10.2026 (ядро, Auto/Dispatch, Multi/Beacon/тесты) и
настраиваемая раскладка канала. **Разрыв совместимости**: новый формат
разделяемой памяти `0x00020000` — пиры 0.9 и нативные пиры по старому
контракту отвергаются по версии (`ShmError::VersionMismatch`). Нормативно —
[`INTEROP.md`](INTEROP.md) §c и «Изменения 0.10».

### Миграция с 0.9

Все видимые изменения API и поведения, из-за которых код или развёртывание
0.9 требуют правки. Подробности — в разделах ниже.

| Что | Было (0.9) | Стало (0.10) | Что сделать |
|---|---|---|---|
| Формат памяти | `SHARED_VERSION = 0x00010000`, пир с другой версией — `HandshakeFailed` | `0x00020000`; чужой формат — `ShmError::VersionMismatch { found, expected }` | Обновить обе стороны одновременно (Rust и нативные пиры по INTEROP §c, рукопожатие — ревизия 3) |
| Дескриптор объектов | NULL DACL — открыть мог любой процесс | `ObjectSecurity::CurrentUser` по умолчанию (пользователь токена + SYSTEM); чужой пользователь — `ShmError::AccessDenied` | Служба SYSTEM ↔ пользователь (`Global\`), песочницы: у создающей стороны `security: ObjectSecurity::Everyone` (поведение 0.9), `CurrentUserAllowLowIntegrity` или `Descriptor(..)` |
| Раскладка канала | фиксированная: 2 × 2 МиБ, 500 сообщений, до 65 535 байт | `ChannelLayout { server_to_client, client_to_server }` из `RingConfig`; `SharedServer::start_with(name, &ChannelConfig)`, поля `layout`/`channel_layout`/`lobby_layout` в опциях; клиент читает раскладку из сегмента | Ничего, если устраивает раскладка по умолчанию (она та же); лобби Dispatch по умолчанию — 2 × 16 КиБ |
| Константы | `RING_CAPACITY`, `MAX_MESSAGES`, `MAX_MESSAGE_SIZE`, `MESSAGE_HEADER_SIZE` | первые три — `#[deprecated]` псевдонимы значений по умолчанию; `MESSAGE_HEADER_SIZE` удалён | `DEFAULT_RING_CAPACITY`/`DEFAULT_MAX_MESSAGES`/`DEFAULT_MAX_MESSAGE_SIZE`, лимит канала — `FreeSpace::max_message_size`/`layout()`; размер кадра — `frame_size(len)` |
| Предел сообщения | `max_message_size ≤ capacity − 8` | `≤ capacity / 2 − 8` (PAD-кадры), иначе `InvalidConfig`; клиент отвергает такой сегмент (`Corrupted`) | Большие сообщения — кольцо вдвое больше (`max_payload_for(capacity)`) |
| Лимит сообщений | вытеснение по одному `message_count` | при `max_messages ≤ RECOUNT_MAX = 64` — пересчёт по кадрам (точно), выше — кольцо может держать `max_messages + 1` | Ничего (нативному писателю — тот же порог, INTEROP §c.5) |
| `ClientRegistration` | литерал из трёх полей | новое поле `handshake_pid: Option<u32>`, derive `Default`/`PartialEq`/`Eq` | `ClientRegistration::new(pid, revision, name)` или `..Default::default()` |
| Опции | `AutoOptions`, `MultiOptions`, `MultiClientOptions`, `DispatchOptions`, `DispatchClientOptions` без новых полей | поля `layout`/`channel_layout`/`lobby_layout`/`max_clients`, `security`, `namespace`, `spin`, `delivery` (Auto) | Литералы опций — с `..Default::default()` |
| `FreeSpace` | открытая структура `{ bytes, messages }` | `#[non_exhaustive]`, новые поля `max_frame`, `max_message_size`; `fits`/`max_payload` решают по `max_frame` | Литерал `FreeSpace { .. }` заменить на `FreeSpace::ZERO`/`Default` или полученный снимок |
| `SharedServer`/`SharedClient` | `Send + Sync` | `Send + !Sync` (гонка SPSC через `&self`) | Из нескольких потоков — `Mutex<SharedServer>`, Auto/Multi/Dispatch |
| `poll_client`/`poll_server` (и `*_spin`) | `&self`; штатный уход пира не виден — `Ok(true)` на устаревшем `DATA` или `Ok(false)` по таймауту, сторона оставалась «подключённой» | `&mut self`; после дочитывания кольца — `Err(NotConnected)` (уход) / `Err(PeerDied)` (смерть), сторона помечается отключённой: `is_connected() == false`, сервер снова принимает клиента (`wait_for_client`) | `let mut server`/`let mut client`, `&mut` в помощниках; `NotConnected` из `poll_*` — конец сессии (после смерти пира `is_peer_alive()` — `None`) |
| Zero-copy | нет | `send_with(len, f)` (`&mut self`, безопасно), `unsafe fn receive_with(f)` (`&mut self`; новая ошибка `ShmError::Overrun`) | Опционально; копирующие методы не менялись |
| Доставка Auto | `on_message` получает копию | по умолчанию — так же; `AutoOptions::delivery` (`Delivery::COPY`), прямая доставка — `unsafe fn AutoOptions::zero_copy_delivery` | Ничего |
| Спин | нет | `SpinPolicy` (`NONE` по умолчанию), поля `spin` в опциях, `poll_*_spin`, `wait_for_space_spin` | Опционально; без спина — «ноль пробуждений в простое», как в 0.9 |
| Async | нет | `accept_async`/`connect_async`/`recv_async`/`send_async`, все `&mut self`, future — `Send` | Опционально; одновременные приём и отправка одной стороны — через разделение/`Mutex` |
| Занятый single-client канал | второй клиент перетирал заявку или ждал до `Timeout` | `ShmError::AlreadyConnected` сразу (ревизия 3: исключительная заявка `slot_claim`); `AutoClient` — `on_error(AlreadyConnected)` с отступом от `reconnect_delay`, удваивающимся до 2 с | Обрабатывать `AlreadyConnected` как «канал занят» |
| Колбэки `MultiHandler` | поток на каждые 21 слот, паника колбэка роняла группу | один поток-диспетчер на все слоты (медленный `on_message` задерживает всех клиентов); паника колбэка — `on_error(Some(slot), WorkerPanicked)` + локальное отключение слота (при `panic = "unwind"`) | Не блокировать в колбэках; тяжёлую работу — в свой поток |
| Ошибки | `ShmError` без этих вариантов, не `#[non_exhaustive]` | новые `AccessDenied`, `WorkerPanicked { message }`, `Overrun`, `VersionMismatch`; у `AlreadyConnected` — новый случай (занятый канал, строка выше); перечисление `#[non_exhaustive]` | В `match` по `ShmError` — ветка `_` |
| Пространства имён | только `\BaseNamedObjects` (`Local\`/`Global\`) | `PrivateNamespace::open_or_create(boundary, alias, &ObjectSecurity)`, поля `namespace`, `connect_in`, `Beacon::open_in`, `lobby_exists_in` | Опционально (защита от захвата имён) |
| Допуск Dispatch | любой зарегистрировавшийся клиент получал канал | `DispatchHandler::admit` → `Admission::{Accept, AcceptWithLayout, Reject}` (по умолчанию `Accept`), `DispatchOptions::max_clients = 64` | Больше 64 клиентов — поднять `max_clients` |
| `MAX_MULTI_CLIENTS` | 31 | 256 (каждый слот — своя секция; 256 × 4 МиБ ≈ 1 ГиБ) | Для больших `max_clients` — маленький `MultiOptions::layout` |
| Маяк | клиент сбрасывал маяк сам (`lower`) | `clear_if_stale` (секция `<имя>_beacon_state`, живой сервер не трогается); `lower` — только сервер | Клиенту — `clear_if_stale` вместо `lower` |
| Маяк присутствия (R19) | маяк взводило приложение лобби; `AutoClient`/`MultiClient` без сервера опрашивали раз в 250 мс | каждый именованный сервер держит `<имя>_beacon`, база Multi — `<база>_multi_beacon` (лучшее усилие: ошибка маяка старт не отказывает, `presence()`); клиенты без сервера спят на нём не дольше `discovery_fallback` (10 с), одинаковая ошибка подряд — один раз; `SharedClient::wait_for_server*`, `DiscoveryOptions`; `MultiClientOptions::{security, discovery_fallback}`, `AutoOptions::discovery_fallback` | Ручной `raise` лобби можно убрать; литералы `AutoOptions`/`MultiClientOptions` — с `..Default::default()`; одинаковая `ObjectSecurity` у сервера и клиентов; `discovery_fallback` — `None` или ненулевой; не называть канал `<база>_multi` рядом с базой Multi `<база>`; нативный сервер — R19 (INTEROP §e.4) |
| Anonymous-сервер (`start_anonymous*`) | событий нет: `get_event_handles() -> Option<EventHandles>` = `None`, `EventHandles` — `s2c_data`/`c2s_data`; ожидание клиента — `wait_for_client_noevent` (опрос 1 мс); `poll_client` — без ожидания, `wait_for_space` — опрос 1 мс, async — `InvalidConfig` | секция всегда с 7 безымянными событиями: `get_event_handles() -> EventHandles` (все 7, `#[non_exhaustive]`), `events() -> &SharedEvents`; `wait_for_client_noevent` удалён; все ожидания — в ядре | `wait_for_client_noevent` → `wait_for_client`; `get_event_handles()` без `Option`; драйвер ОБЯЗАН сигналить `CONNECT_REQ`/`DATA`/`SPACE`/`DISCONNECT` по INTEROP §b.5 (иначе сервер ждёт до таймаута) |

Потребители, проверенные на этой ветке (Task 19, `cargo check --workspace
--all-targets`):

- `useful-lib/prof-shm` — собирается (правки `ClientRegistration::new` и
  `DEFAULT_MAX_MESSAGE_SIZE` уже сделаны там), release-сборка без
  предупреждений xshm;
- `NxT` — собирается без правок;
- `E-NxT` — `crates/shm-server/tests/roundtrip.rs` (3 места): литерал
  `ClientRegistration { pid, revision, name }` → `ClientRegistration::new(pid,
  revision, name)` (или `handshake_pid: None`);
- `APIScout` — воркспейс целиком не разрешается из-за собственного конфликта
  `dear-imgui-sys` 0.18/0.20; крейты `engine-ipc` и `protocol` собираются без
  правок.

### Добавлено

- **Раскладка канала по направлениям**: `RingConfig { capacity,
  max_messages, max_message_size }` и `ChannelLayout { server_to_client,
  client_to_server }`. Ёмкость — степень двойки от 4 КиБ до 1 ГиБ, сообщение
  — до `capacity / 2 − 8` (больше прежних 64 КиБ; предел — из-за PAD-кадров,
  см. «Изменено»). Выбирает сервер, клиент читает
  её из сегмента сам.
  - `SharedServer::start_with(name, &ChannelConfig)` (раскладка + политика
    безопасности; `ChannelConfig` — `#[non_exhaustive]`, строится от
    `default()`/`new(layout)` методами `layout`/`security`),
    `start_anonymous_with_layout`,
    `SharedServer::layout`, `SharedClient::layout`;
  - `AutoOptions::layout` (сервер), `AutoServer::layout`, `AutoClient::layout`;
  - `MultiOptions::layout` (каждый слот), `MultiClient::layout`;
  - `DispatchOptions::{channel_layout, lobby_layout, max_send_queue,
    max_clients}`; лобби по умолчанию — 2 × 16 КиБ (`DEFAULT_LOBBY_LAYOUT`)
    вместо 2 × 2 МиБ.
- `DispatchHandler::admit(&ClientRegistration) -> Admission`
  (`Accept`/`AcceptWithLayout`/`Reject`): допуск и раскладка канала для
  клиента до создания канала.
- `FreeSpace::max_message_size`, `frame_size()`, `max_payload_for()`,
  константы `DEFAULT_*`, `MIN/MAX_RING_CAPACITY`, `FRAME_HEADER_SIZE`,
  `FRAME_ALIGN`, `SHARED_VERSION`.
- `ShmError::VersionMismatch { found, expected }`; `ShmError` —
  `#[non_exhaustive]`.
- **Политика безопасности объектов** `ObjectSecurity` (`#[non_exhaustive]`):
  `CurrentUser` (по умолчанию — DACL: SID пользователя токена процесса +
  SYSTEM `S-1-5-18`, `GENERIC_ALL`), `CurrentUserAllowLowIntegrity` (+ SACL с
  меткой Low `S:(ML;;NW;;;LW)`), `Everyone` (NULL DACL, как до 0.10),
  `Descriptor(Vec<u8>)` (свой self-relative дескриптор; проверяется
  `RtlValidRelativeSecurityDescriptor` с длиной, иначе `InvalidConfig` до
  создания объектов). Задаётся создателем объектов: `ChannelConfig::security`,
  поля `security` в `AutoOptions` (сервер), `MultiOptions` (каждый слот),
  `DispatchOptions` (лобби, мьютекс лобби и каналы клиентов наследуют),
  `DispatchClientOptions` (мьютекс лобби, если клиент создаёт его первым);
  `Beacon::open_with(name, &ObjectSecurity)` — событие маяка и секция
  `_beacon_state` (`Beacon::open` — политика по умолчанию). Дескриптор
  строится на каждое создание и владеет своими буферами (SID/ACL) до конца
  `NtCreate*`. Anonymous-секция — тоже политика по умолчанию вместо NULL DACL;
  безымянные события — без атрибутов (DACL токена), как раньше.
- `ShmError::AccessDenied` — `STATUS_ACCESS_DENIED` при открытии/создании
  объекта (раньше `WindowsError { code: 0xC0000022 }`). Маяк по-прежнему
  деградирует до режима без состояния, если в секции состояния отказано.
- **Частное пространство имён** `PrivateNamespace::open_or_create(boundary,
  alias, &ObjectSecurity) -> Arc<PrivateNamespace>`: каталог объектов ядра,
  который ищется по дескриптору границы (имя + SID пользователя процесса, для
  `CurrentUserAllowLowIntegrity` — + метка Low), а не в общем
  `\BaseNamedObjects`; создать или открыть его может только тот же
  пользователь. `NtCreatePrivateNamespace` (`ObjectName = NULL`, дескриптор —
  политика `security`), при коллизии — `NtOpenPrivateNamespace`; `alias` —
  только диагностика (как Win32 `lpAliasPrefix`, ядро его не использует).
  Объекты внутри — относительные имена (`RootDirectory` = handle
  пространства); имя с `Local\`/`Global\`/`\` — `InvalidConfig`. Включается
  полями `namespace: Option<Arc<PrivateNamespace>>` в `ChannelConfig`
  (+ метод `namespace`), `AutoOptions`, `MultiOptions`, `MultiClientOptions`,
  `DispatchOptions` (лобби, `_lock` и каналы клиентов — в пространстве
  лобби), `DispatchClientOptions`; `SharedClient::connect_in(&ns, name,
  timeout)`, `Beacon::open_in(&ns, name, &ObjectSecurity)`,
  `DispatchClient::lobby_exists_in`. Пространство живёт, пока открыт хоть
  один handle; `Drop` только закрывает handle — `NtDeletePrivateNamespace` не
  вызывается (пиры остались бы в удалённом каталоге, а перезапущенный сервер
  создал бы другое пространство; закреплено тестом
  `creator_drop_keeps_namespace_for_peers`). Без пространства — поведение
  прежнее.
  Пир-видимо: `INTEROP.md` §b.4, §h п. 7–8, «Изменения 0.10» п. 23.
- **Спин перед ожиданием в ядре** `SpinPolicy { iterations }`
  (`SpinPolicy::NONE` — по умолчанию, спина нет): до `iterations`
  перепроверок кольца по позициям (`std::hint::spin_loop` между ними), затем
  обычное ожидание события. `SharedServer::poll_client_spin`,
  `SharedClient::poll_server_spin`, `wait_for_space_spin` (заявка
  `space_waiter` на время спина не ставится); поля `spin` в `AutoOptions`,
  `MultiOptions` (кольца всех подключённых слотов),
  `MultiClientOptions`, `DispatchOptions`/`DispatchClientOptions` (только
  каналы клиентов, лобби не спинит). Бюджет фиксированный, один на
  засыпание: в простое worker отспинивает его один раз и спит без таймаута
  (ноль пробуждений). Удачный спин не отменяет проверку событий: за ним
  всегда идёт ожидание с нулевым таймаутом (у Multi-сервера — опрос порта
  завершения: болтливый слот не заслоняет `CONNECT_REQ`/смерть других
  слотов).
  Во время спина `DISCONNECT`, `SPACE` и смерть пира замечаются не позже
  конца бюджета; `poll_*_spin`/`wait_for_space_spin` сверяют дедлайн
  вызывающего раз в 64 итерации. Цена — занятое ядро CPU; длительность
  `pause` зависит от CPU. `DATA`, взведённый писателем во время спина,
  вручную не сбрасывается. Пиру не виден: формат и сигналы прежние.
- **Zero-copy** (Task 14): payload кадра в кольце теперь всегда непрерывен
  (PAD-кадры, см. «Изменено»), поэтому его можно читать и писать на месте.
  - `SharedServer::send_with(len, f)` / `SharedClient::send_with(len, f)` —
    запись без перезаписи с заполнением `&mut [u8]` прямо в кольце (место и
    ошибки — как у `try_send_*`). Безопасна: срез — неопубликованная часть
    кольца; враждебный пир, пишущий в неё вопреки протоколу, портит только
    сообщение, которое получит сам. Принимает `&mut self`: вложенная запись
    из `f` и параллельный вызов из другого потока не компилируются (иначе
    вторая запись получала ту же область — потеря сообщения и алиасинг
    `&mut`; `compile_fail`-doctest).
  - `unsafe fn SharedServer::receive_with(f)` / `SharedClient::receive_with(f)`
    — `f` получает `&[u8]` прямо из кольца. `unsafe`: срез указывает в
    разделяемую память, и если пир меняет её во время `f`, это гонка данных
    и TOCTOU в коде потребителя. Контракт — пир доверенный и пишет это
    направление без перезаписи (`try_send*`/`send_with`/`wait_for_space`).
    Перезапись, замеченная после `f` (CAS `read_pos`), — новая ошибка
    `ShmError::Overrun`, результат `f` отбрасывается (защита в глубину).
    Для недоверенных пиров — копирующие `receive_from_*` (ADR 11).
    Принимает `&mut self`: второго чтения этого направления во время `f`
    (вложенного или из другого потока) не бывает по типу.
  - Auto: по умолчанию по-прежнему копирует; доставка `on_message` срезом
    из кольца — `unsafe fn AutoOptions::zero_copy_delivery(self)` (поле
    `AutoOptions::delivery: Delivery`, `Delivery::COPY` по умолчанию,
    `unsafe fn Delivery::zero_copy()` — без `unsafe` не включить). `Overrun`
    приходит в `on_error`, worker читает кольцо дальше.
  - `FreeSpace::max_frame` — наибольший кадр, который помещается прямо
    сейчас с учётом PAD; `fits`/`max_payload` решают по нему (по `bytes`
    больше нельзя: байт может хватать, а непрерывного места — нет). У Auto
    при непустой очереди отправки оценка — с наихудшим PAD (кадр
    максимального сообщения − 8).
  - Константа `FRAME_PAD` (бит 0 `flags`).
  - Бенчмарк: `bench throughput <size> --zero-copy` (читатель через
    `receive_with`).
- **Async API single-client каналов** (исполнитель-агностично: только
  `core::task`, без зависимостей и без потока на канал — ожидание ведёт
  общий реактор IOCP): `SharedServer::{accept_async, recv_async,
  send_async}`, `SharedClient::{connect_async, recv_async, send_async}`.
  - Все методы берут `&mut self` (сторона — `!Sync`), поэтому future —
    `Send` и годится для `tokio::spawn`; одновременные приём и отправка
    одной стороны — через разделение (`Mutex`, задача-владелец).
  - `send_async` пишет без перезаписи и ждёт места (заявка `space_waiter`,
    как `wait_for_space`); `recv_async` ждёт `DATA` по протоколу
    «регистрация -> перепроверка позиций» без потерянного пробуждения.
  - Отключение пира — `Err(NotConnected)`, смерть процесса —
    `Err(PeerDied)`; `recv_async` сообщает их только после дочитанного
    кольца и затем отмечает сторону неподключённой (сервер снова может
    `accept_async`).
  - Таймаутов нет — отмена `Drop`-ом future (таймаут исполнителя). Брошенный
    future снимает регистрации и возвращает событию сигнал, который они
    успели потребить (лишний сигнал протокол допускает, потерянный — нет);
    брошенный `connect_async` отзывает заявку, как синхронный таймаут
    (CAS `CLIENT_HELLO -> IDLE`), а принятый сервером — уходит штатно
    (`IDLE`, затем `DISCONNECT`). `connect_async` следит за процессом
    сервера (PID и время создания из сегмента): сервер умер, не ответив, —
    `Err(PeerDied)` с тем же откатом заявки. Повторно опрашивать future
    после `Ready` нельзя: recv/send выполнили бы ещё одну операцию.
  - Anonymous-сервер — так же (Task 13: безымянные события, см.
    «Изменено (несовместимо)»; до него — `Err(InvalidConfig)`).
  - `SharedClient::connect_async_in(ns: Arc<PrivateNamespace>, name)`
    (надёжность, 17.10.2026) — async-подключение к каналу в частном
    пространстве имён: тот же future, что `connect_async` (заявка,
    ожидание, откат брошенного), объекты канала — как у `connect_in`;
    ошибки — как у `connect_in` (имя с префиксом или `\` — `InvalidConfig`
    на первом опросе). `Arc` по значению — future остаётся `'static`
    (`tokio::spawn`) и держит пространство до готовности. `accept_async`
    сервера, поднятого `start_with` в пространстве, работал и раньше —
    теперь закреплён тестом `async_connect_in_namespace_echo`
    (`tests/namespace.rs`). Пир-невидимо.
  - Протокол и раскладка не меняются: async API — это те же сигналы и
    переходы, что у синхронных `wait_for_client`/`connect`/`poll_*`/
    `wait_for_space`.

- **`examples/interop_peer.rs`** — Rust-сторона межъязыковой проверки
  xhook/xipc: один сценарий single-client канала на запуск (эхо, раскладки,
  backpressure, прощание, смерть пира, занятость, уход по ошибке Auto,
  отказ по версии). Протокол stdout построчный (`READY`, `INFO`, `PASS`,
  `FAIL`), ошибки и причины печатаются именами вариантов. Только публичный
  API, без зависимостей. Оркестраторы и матрица разрядностей живут в
  xhook (`tests_static/test_ipc_interop_*`, `scripts/run_interop.ps1`);
  INTEROP §g. Сценарии Multi и межъязыковых сессий (b7fb341):
  `multi-server` (`kick=` — `disconnect_client`), `multi-client`
  (`mode=echo|ping`), `server-sessions` (передача claim-а между языками),
  `*-stall-leave`/`*-fill-wait`, `client-hello-die`, `client-echo busy=1`.

- **`SharedServer::disconnect()`** (решение T): отключить текущего клиента
  штатно — `IDLE`, затем `S2C_DISCONNECT`; данные в кольце клиент
  дочитывает, claim снимает сам клиент (R18), сервер снова принимает
  следующего. `Err(NotConnected)` — клиента нет. Раньше отключение клиента
  было только внутренним (`disconnect_local`) — синхронному серверу
  приходилось ронять канал целиком. Тест
  `server::disconnect_tests::disconnect_kicks_client_and_accepts_next`.
  Rustdoc (M4 ревью T11b): живой клиент, не заметивший разрыва, держит
  claim до отъёма по `RESERVE_TIMEOUT` от первого толчка; непрочитанное
  клиент → сервер сбрасывает S3 следующей сессии (anonymous — с Task 13
  тоже с сигналом: безымянные события).

- **Маяк присутствия у каждого сервера** (надёжность, 17.10.2026, Task 11;
  INTEROP **R19**, «Изменения 0.10» п. 34). Каждый именованный сервер
  single-client канала (`SharedServer::start`/`start_with`, `AutoServer`,
  лобби `DispatchServer`) держит маяк `<имя>_beacon` (+ `<имя>_beacon_state`,
  тот же `Beacon`), база `MultiServer` — один `<база>_multi_beacon` (не
  слоты; своё имя — канал `X` и база Multi `X` маяк не делят);
  без маяка — только выделенные каналы Dispatch. Тот же namespace (в том
  числе `PrivateNamespace`) и та же политика, что у секции канала. `raise` —
  после публикации секции и всех 7 событий (Multi — всех слотов), `lower` —
  первым шагом `Drop`/`stop`; маяк занят объектом другого типа или с чужим
  DACL — **не** ошибка старта (решение W(1), fix round 1): сервер работает
  без маяка, причину сообщает `presence()` (`PresenceStatus::{Active,
  Disabled, Unavailable(err)}` у `SharedServer`, `AutoServer`,
  `MultiServer`, `DispatchServer`), клиенты находят его резервной
  проверкой `discovery_fallback`. Лобби Dispatch взводит маяк
  само: ручной `raise` приложения (prof-shm) больше не нужен и безвреден.
  Внутреннее: `src/presence.rs` (`ServerPresence`, `PresenceWatch`,
  `Failure`, `classify`), `SharedServer::start_slot(.., presence)`,
  `Beacon::open_at`/`generation`/`server_identity`,
  `SharedView::peek_server` (PID сервера из `ControlBlock` любой версии).
- **`SharedClient::wait_for_server(name, timeout)` и
  `wait_for_server_in(ns, name, timeout)`** -> `Result<bool>`: дождаться
  сервера канала без опроса (сон на маяке присутствия; маяк упавшего
  сервера гасится `clear_if_stale`, ожидание продолжается). `Ok(true)` —
  секция опубликована, 7 событий на месте, процесс сервера не завершился;
  `Ok(false)` — таймаут; сервер есть, но подключиться нельзя, — его ошибка
  (`VersionMismatch`, `AccessDenied`, …). Маяк недоступен — резервная
  проверка раз в 250 мс внутри вызова.
- `MultiClientOptions::security` — политика маяка базы, если его создаёт
  клиент (пришёл раньше сервера; по умолчанию `CurrentUser`). У `AutoClient`
  ту же роль играет `AutoOptions::security`. DACL маяка задаёт первый
  создатель (решение R-L): служба на `Global\` с `Everyone` требует той же
  политики у клиентов (или старта службы первой).

- **Уведомление «claim освобождён»** (надёжность, 17.10.2026, Task 12;
  решения J, R-N, R-O; INTEROP **R20**/**R21**, «Изменения 0.10» п. 39).
  Рядом с маяком присутствия сервер создаёт событие-уведомление
  `<имя>_free` (база `MultiServer` — `<база>_multi_free`; тот же namespace и
  дескриптор, лучшее усилие, как маяк), а в `<имя>_beacon_state` — счётчики
  `free_gen@20` и `free_waiters@24`. Каждое освобождение claim-а
  (`slot_claim: T -> 0` — откат и отзыв заявки, уход клиента, снятие
  claim-а умершего, отъём протухшего захвата, дедлайн и уход в Multi) и
  `raise`/`lower` маяка увеличивают `free_gen` и, только если есть ждущие,
  взводят `<имя>_free`: без ждущих — ни одного системного вызова. Внутреннее:
  `presence::{FreeNotifier, FreeWaiter, release_claim_notify,
  ChannelProbe}` — `release_claim_notify` единственный путь `T -> 0` в
  крейте.
- **`SharedClient::wait_connectable(name, timeout)` и
  `wait_connectable_in(ns, name, timeout)`** -> `Result<bool>`: дождаться,
  пока канал не станет свободным, не подключаясь (`Ok(true)` — claim был
  свободен; проигравший гонку `connect` получит `AlreadyConnected` и зовёт
  снова; `Ok(false)` — таймаут). Сон в ядре на `<имя>_free` и процессах
  держателя и сервера; сервер без `<имя>_free` — проверка раз в 250 мс.
  Fix round 1: `wait_connectable_with(name, timeout, &DiscoveryOptions)` —
  страховка `fallback` (10 с по умолчанию) и пространство имён; занятым
  считается и канал без claim-а с заявкой или сессией.

### Изменено (несовместимо)

- **Anonymous-сервер всегда с 7 безымянными событиями** (надёжность,
  18.10.2026, Task 13; решения J (D), R-P; INTEROP §b.5, «Изменения 0.10»
  п. 40). `start_anonymous*` создаёт вместе с секцией 7 безымянных
  SynchronizationEvent (тот же вид, что у именованного канала) с тем же
  дескриптором, что у секции. API:
  - `SharedServer::get_event_handles()` возвращает `EventHandles` (было
    `Option<EventHandles>`, у anonymous — `None`);
  - `EventHandles` — все 7 handle-ов (`s2c_data`, `c2s_data`, `s2c_space`,
    `c2s_space`, `connect_ack`, `connect_req`, `disconnect`; было — только
    `s2c_data`/`c2s_data`), `#[non_exhaustive]`, derive `PartialEq`/`Eq`;
  - `SharedServer::events()` возвращает `&SharedEvents` (было
    `Option<&SharedEvents>`);
  - **`SharedServer::wait_for_client_noevent` удалён** (опрос `client_state`
    раз в 1 мс) — anonymous-сервер ждёт драйвер `wait_for_client`/
    `accept_async`, как именованный;
  - `is_anonymous()` — по флагу канала, а не по отсутствию событий.

  Пир-видимо: драйвер (роль клиента) ОБЯЗАН сигналить `C2S_CONNECT_REQ`
  после `CLIENT_HELLO`, `DATA`/`SPACE` по §e.2 п. 1–2 и уходить `IDLE` →
  `DISCONNECT` при выгрузке — процесса у него нет, смерть сервер не
  наблюдает. Миграция: `wait_for_client_noevent(t)` → `wait_for_client(t)`,
  `get_event_handles().unwrap()` → `get_event_handles()`. Потребители
  (поиск по `D:\GitHub`): API anonymous-канала не использует никто.

- **`SharedServer::poll_client`/`poll_client_spin` и
  `SharedClient::poll_server`/`poll_server_spin` берут `&mut self`** (было
  `&self`): на завершающем исходе (`Err(NotConnected)` после штатного ухода
  пира, `Err(PeerDied)`) сторона помечается отключённой — `is_connected()`
  становится `false`, повторный `poll_*` сразу даёт `NotConnected`, сервер
  снова принимает клиента. Claim штатно ушедшего клиента сервер не трогает
  (его снимает клиент), умершего — снимает (R18); клиент, увидевший уход
  сервера, при `Drop` только снимает claim, без ответного `DISCONNECT`.
  `wait_for_space*` остаются `&self` и сторону не помечают (сообщают
  `NotConnected`, событие взведено) — сессию закончит следующий `poll_*`.
  Миграция: `let mut` у стороны, `&mut` в помощниках.
- **`SharedServer`/`SharedClient` — `!Sync`** (остаются `Send`): параллельные
  `&self`-вызовы `try_send_*`/`send_*`/`receive_from_*` из двух потоков были
  гонкой данных на одной стороне SPSC-кольца в безопасном коде (UB). Теперь
  компилятор не даёт поделить ссылку между потоками; для нескольких потоков —
  Auto/Multi/Dispatch или `Mutex<SharedServer>`. Без блокировок: цена нулевая.
- **Кадр больше не разрывается границей кольца** (INTEROP §c.4, «Изменения
  0.10» п. 26; пир-видимо): если кадр не помещается до конца кольца,
  писатель занимает остаток PAD-кадром (`flags` бит 0 = `FRAME_PAD`, `len` =
  остаток − 8, может быть 0) и кладёт кадр с начала кольца; PAD и кадр
  публикуются одной записью `write_pos` (сигнал `DATA` — по позиции до PAD).
  PAD не сообщение: не доставляется, не входит в `message_count`,
  `max_messages`, `drop_count`, вытесняется перезаписью без учёта. Место под
  запись и заявка `space_waiter` — PAD + кадр. Читатель отвергает
  (`Corrupted`) кадр сообщения через границу, PAD не ровно до границы или за
  `write_pos`, неизвестные биты `flags`. Нативный пир обязан реализовать PAD
  на чтении и на записи.
- **`max_message_size ≤ capacity / 2 − 8`** (было `capacity − 8`):
  `max_payload_for(capacity)` и `RingConfig::new` дают новый предел,
  `RingConfig::validate` отвергает больший (`InvalidConfig`), клиент —
  сегмент с большим (`Corrupted`). Вывод: PAD + кадр занимают до `2F − 8`
  байт, и только при `F ≤ capacity / 2` такая запись помещается в пустое
  кольцо при любой позиции (INTEROP §c.2). Значения по умолчанию (2 МиБ /
  65 535) и лобби Dispatch (16 КиБ / 1 024) — в пределах.
- `FreeSpace` получил поле `max_frame` и стал `#[non_exhaustive]`: поля
  по-прежнему читаются, строит снимок только крейт (снаружи —
  `FreeSpace::ZERO`/`Default`); литералы `FreeSpace { .. }` у потребителей
  не собираются, зато следующее поле ничего не сломает.
- PAD-кадр на индексе 0 (на всё кольцо) — `Corrupted`: честный писатель его
  не пишет, а принятый он позволял бы цепочку PAD подряд (INTEROP §c.4).

- **NULL DACL больше не по умолчанию** (несовместимо по поведению): все
  именованные объекты создаются с `ObjectSecurity::CurrentUser` — открыть их
  могут только процессы того же пользователя и SYSTEM; остальные получают
  `ShmError::AccessDenied`. Сервер Dispatch теперь сам создаёт мьютекс лобби
  `<лобби>_lock` в `start` (`OBJ_OPENIF`, политика лобби) и держит его, чтобы
  DACL мьютекса задавал сервер, а не первый клиент; имя, занятое объектом
  другого типа, — ошибка `DispatchServer::start`. INTEROP §b.2, «Изменения
  0.10» п. 22.

  **Миграция с 0.9**: `Global\`-канал служба SYSTEM (сессия 0) ↔ процесс
  пользователя, работавший на NULL DACL, при создании объектов службой с
  политикой по умолчанию даёт пользователю `AccessDenied` (DACL — только
  SYSTEM). Создающей стороне (серверу, а для маяка — каждой стороне, которая
  может создать его первой) нужна `ObjectSecurity::Everyone` или свой
  `ObjectSecurity::Descriptor` (например, SYSTEM + группа `Users`). То же для
  песочниц Low integrity — `CurrentUserAllowLowIntegrity`, для AppContainer —
  `Descriptor` с SID пакета. Новые публичные поля `security` в
  `AutoOptions`, `MultiOptions`, `DispatchOptions` и `DispatchClientOptions`:
  литералы этих структур без `..Default::default()` больше не собираются.
  `MultiClient`: слот с отказом в доступе перебор не обрывает, итог без
  захвата — `AccessDenied` (приоритет над `NoFreeSlot` и `NotConnected`),
  а не вечный `NotConnected` «сервера нет» (INTEROP §i.1); при
  `AccessDenied` клиент всё равно толкает протухшие захваты
  (`nudge_stale_claims`, слоты с отказом пропускает): захватчик, заранее
  создавший `B_N` с запрещающим DACL, не отключает освобождение слота
  упавшего клиента. Поля `namespace` (частное пространство имён) — так же
  новые публичные поля этих структур и `MultiClientOptions`.
  `SharedServer::start_with_layout` (не выпущен)
  заменён на `start_with(name, &ChannelConfig)`; `SharedEvents::create` —
  внутренний.
- Формат памяти `0x00020000`: `LayoutBlock` (смещение 64) с дескрипторами
  колец; поля `ControlBlock` именованы (`slot_claim`, `slot_owner_pid`,
  `server_pid`, `client_pid` — те же смещения, что `reserved[0..3]`); кадр
  `u32 len + u32 flags`, выровнен на 8; `MIN_MESSAGE_SIZE` = 1.
- `AutoServer::stop`/`AutoClient::stop`/`MultiClient::stop` — синхронные: после
  возврата колбэков нет (из своего колбэка — без ожидания себя).
- `AutoHandler::on_overflow`/`MultiClientHandler::on_overflow` — раз на
  пачку вытесненных сообщений, а не на каждое.
- `send` в Auto/Multi-клиенте проверяет длину синхронно (против лимита
  кольца, когда он известен).
- `RING_CAPACITY`, `MAX_MESSAGES`, `MAX_MESSAGE_SIZE` — устаревшие псевдонимы
  значений по умолчанию; `MESSAGE_HEADER_SIZE` удалён (кадр изменился —
  `frame_size`).
- (находка 24) `MAX_MULTI_CLIENTS` — 256 вместо 31: предел задаёт протокол
  перебора, а не лимит 64 handle (сервер ждёт на порту завершения одним
  потоком на все слоты, см. «Изменено (внутреннее)»). Клиент перебирает слоты `0..256` до первого
  отсутствующего. Сервер Multi публикует число слотов в
  `ControlBlock::slot_count`@60 каждого слота (до `magic`) — только
  подсказка: клиенты перебор по ней не ограничивают (поле пишет любой
  клиент; `slot_count = 1` в слоте 0 иначе закрыл бы остальные слоты для
  всех). `SharedView::initialize` получил параметр `slot_count` (`0` — не
  Multi). 256 слотов × раскладка по умолчанию (2 × 2 МиБ) ≈ 1 ГиБ секций —
  для больших `max_clients` задавайте маленький `MultiOptions::layout`.
  Пир-видимо: `INTEROP.md` §c.1, §i.1, «Изменения 0.10» п. 17.
- `ClientRegistration` получил поле `handshake_pid: Option<u32>` (PID
  рукопожатия лобби — самоописание пира, сверенное с процессом по ненулевому
  времени создания, не аутентификация; заполняет
  сервер) и derive `Default`/`PartialEq`/`Eq`. Литерал структуры без нового
  поля не собирается — строить через `ClientRegistration::new(pid, revision,
  name)` (или `..Default::default()`).
- Dispatch: протокол лобби строгий — `decode_request`/`decode_response`
  отвергают (`Corrupted`) `name_len > 64`, хвостовые байты и невалидный
  UTF-8 (раньше хвост отбрасывался, UTF-8 чинился через U+FFFD). Пир-видимо:
  `INTEROP.md` §d.1, «Изменения 0.10» п. 18.
- **`AutoClient`/`MultiClient` без сервера не опрашивают** (надёжность,
  17.10.2026, Task 11; решения J, K, M; INTEROP R19, п. 34). Было: вне
  подключения попытка раз в `reconnect_delay`/`retry_delay` (250 мс) — 4
  пробуждения и 4 `on_error` в секунду, пока сервера нет, до 250 мс лишней
  задержки, когда он появился. Стало: **сервера нет** (секции/событий нет,
  сегмент не опубликован, Multi — нет слота 0, процесс сервера завершился —
  `PeerDied`) — сон в ядре `[wake, маяк присутствия]` (не дольше
  `discovery_fallback`),
  подключение сразу после взвода; маяк упавшего сервера клиент гасит
  (`clear_if_stale`) и спит дальше; маяк взведён живым сервером, а канала нет,
  — одна повторная попытка на поколение взвода, затем сон до выхода его
  процесса. **Сервер отверг** (`VersionMismatch`, `Corrupted`, `AccessDenied`,
  `InvalidConfig`; Auto) — сон до выхода процесса этого сервера (PID из
  `ControlBlock`, сверенный с состоянием маяка; если секция не открылась —
  из состояния маяка). Пауза осталась: при занятом канале
  (`AlreadyConnected`, Multi `NoFreeSlot`/`AccessDenied` — до уведомления
  «claim освобождён»), после `Timeout`/ошибки рукопожатия, разово после
  конца сессии и как **резерв** (решение R-M: маяк недоступен — нет прав или
  привилегии `Global\`; процесс сервера не открылся; состояние маяка
  испорчено). После `PeerDied` паузы нет. `on_error` — **одинаковая ошибка
  подряд — один раз** (кроме `AlreadyConnected` и `NoFreeSlot` — о каждой
  попытке, как раньше); другая ошибка между ними — и повтор сообщается
  снова (fix round 1, M1: второй «зависший» сервер после промежуточной
  ошибки виден); ошибки маяка — свой ряд. Счётчики пробуждений (`wakeups()`, тесты) считают и эти
  ожидания. Клиент, стартовавший раньше сервера **без** маяка (нативный пир
  без R19, отказ создания маяка), находит его резервной проверкой
  `discovery_fallback` (решение W(3): `AutoOptions`/`MultiClientOptions`/
  `DiscoveryOptions`, по умолчанию 10 с, `None` — только события; каждое
  ожидание обнаружения ограничено ею, резерв R-M — тоже). Личность сервера
  (недоверенные данные секции и маяка, решение W(2)): без времени создания
  или своя — не ждётся; личности секции и маяка расходятся — ни одной;
  маяк в наборе ожидания только невзведённым (иначе горячий цикл). Тесты
  `auto::presence_tests::{absent_server_client_sleeps_without_wakeups,
  client_connects_promptly_when_server_appears,
  stale_beacon_of_dead_server_is_cleared_then_sleeps,
  incompatible_server_waits_for_its_exit}`,
  `multi::tests::{absent_server_multi_client_sleeps,
  multi_client_connects_when_server_appears}`,
  `server::presence_tests::presence_raised_after_publish_lowered_first_in_drop`,
  `dispatch::tests::lobby_raises_beacon_automatically`,
  `client::presence_tests::wait_for_server_returns_on_start`,
  `tests/peer_death.rs::auto_client_reconnects_after_server_crash_without_polling`,
  `tests/namespace.rs::presence_inside_private_namespace`,
  `tests/security.rs::presence_objects_use_channel_policy`;
  `tests/security.rs::multi_client_reports_access_denied` теперь ждёт ровно
  одну ошибку. Миграция: литерал `MultiClientOptions { .. }` — с
  `..Default::default()` (новое поле `security`); обработчик, считавший
  повторы `on_error` как признак «сервера всё ещё нет», — ждать `on_connect`;
  имена каналов и баз Multi не должны совпадать (`X` и база `X` делят маяк
  `X_beacon`).
- **Занятый канал и `NoFreeSlot` — без опроса** (надёжность, 17.10.2026,
  Task 12; INTEROP R20/R21, п. 39). Было: `AutoClient` при
  `AlreadyConnected` повторял попытку с отступом `BusyBackoff`
  (`reconnect_delay` -> 2 с, `on_error` на каждой попытке), `MultiClient`
  при `NoFreeSlot` — раз в `retry_delay` (250 мс) с толчком всех слотов.
  Стало: `on_error(AlreadyConnected)`/`on_error(NoFreeSlot)` — **раз на
  эпизод** занятости, затем сон в ядре на `[wake, <имя>_free, процесс
  держателя, процесс сервера]` (Multi — `[wake, <база>_multi_free, процесс
  сервера]`) без таймаута; держатель ушёл — подключение сразу (в тестах ≤
  200 мс). Захват без сессии ждущий толкает (`CONNECT_REQ`) один раз на
  токен и разово повторно через `RESERVE_TIMEOUT + 1 с` (решение R-N) —
  сервер отнимает протухший захват на нём; подключённая сессия держателя ни
  толчков, ни таймаута не даёт. `BusyBackoff`/`retry_delay` остались
  **резервом**: сервер без `<имя>_free` (пир без R20, подменённый тип
  события), заголовок канала не открылся, claim свободен, а канал занят
  клиентом без claim. Пир, освобождающий claim без уведомления (нативный
  клиент без R20 при Rust-сервере), держит ждущих до смерти процесса
  держателя/сервера или до следующего освобождения (INTEROP §h п. 12).
  Тесты: `auto::free_tests::{busy_channel_waiter_sleeps_without_wakeups,
  busy_waiter_wakes_on_every_release_path}`,
  `multi::tests::{no_free_slot_waiter_sleeps_and_wakes_on_release,
  slot_deadline_reclaim_notifies_waiters}`,
  `presence::tests::{free_waiter_interleavings,
  free_waiters_never_miss_release_stress,
  release_without_waiters_makes_no_syscall, free_flood_does_not_starve_stop,
  stale_claim_is_renudged_once}`, `client::tests::{wait_connectable_returns_on_release,
  wait_connectable_falls_back_without_free_event}`, `tests/peer_death.rs`
  `busy_waiter_wakes_on_holder_crash_in_session`; резерв —
  `auto::claim_tests::already_connected_backs_off` (`<имя>_free` занят
  автосбросным событием).
- Dispatch: мьютекс лобби `<лобби>_lock` сервер создаёт **до** секции лобби
  (было — после): лобби взводит маяк присутствия, и разбуженный им клиент
  застаёт мьютекс с политикой сервера. INTEROP §b.2.

### Исправлено (аудит 16.10.2026)

- **Low** Паника на переполнении `Instant` при огромном таймауте
  (`Duration::MAX`) в `SharedClient::connect`/`connect_in` и в ожидании места
  (`wait_for_space*`): таймаут, непредставимый как `Instant`, теперь значит
  «без дедлайна» (`ring::deadline_of`), как уже было в `poll_*`.

- **High** Ложный `Corrupted` при `max_messages = 1`: читатель уменьшает
  `message_count` после CAS `read_pos`, и писатель в этом окне видел пустое
  по позициям кольцо с `count == max_messages` — `write_message` отвечал
  `Corrupted` (в Auto — фатальный разрыв), `try_write_message` — `QueueFull`.
  Теперь лимит сообщений вытесняет/отказывает только при `used > 0`, пустое
  кольцо принимает кадр при любом счётчике (`RingBuffer::admits`,
  `free_space`); `Corrupted` — только по позициям и кадрам. Тест
  `ring::try_write_tests::lagging_count_does_not_reject_write_into_empty_ring`.
  Пир-видимо: `INTEROP.md` §e.2 п. 1.
- **High** (регрессия формата 0.10, найдена бенчмарком `bench throughput 16`)
  Потерянный сигнал `DATA`: писатель решал «сигналить» по прежнему
  `message_count == 0`, а читатель с 0.10 считает кольцо пустым по
  `write_pos == read_pos` и уменьшает счётчик ПОСЛЕ сдвига `read_pos`.
  Интерливинг «читатель сдвинул `read_pos` → писатель `message_count += 1`
  (видит 1 — без сигнала) → читатель уменьшил счётчик и увидел пусто до
  публикации `write_pos` → сон на `DATA`» оставлял кадр в кольце без
  сигнала — обе стороны ждали вечно (Auto/Multi/Dispatch/`poll_*(None)`).
  Теперь протокол «публикация → перепроверка» (Dekker, как у
  `space_waiter`): писатель после `write_pos` ставит `fence(SeqCst)` и
  сигналит при `read_pos ==` прежнему `write_pos`
  (`WriteOutcome::was_empty`); читатель перед `QueueEmpty` (и `is_empty` →
  `true`) ставит `fence(SeqCst)` и перечитывает `write_pos`. `message_count`
  и `sequence` — только лимит и статистика. Тесты:
  `ring::try_write_tests::{data_signal_survives_reader_draining_concurrently,
  reader_recheck_sees_frame_published_after_empty_snapshot}`,
  `tests/wakeup.rs` (10⁶ сообщений по 16 байт в обе стороны, ожидание только
  по событиям, дедлайн 60 с). Пир-видимо: `INTEROP.md` §c.5, §e.2 п. 1, R1,
  «Изменения 0.10» п. 24.
- **High** (находка 11) Маяк терял пробуждение при нескольких клиентах:
  клиент, не нашедший лобби при взведённом маяке, делал `Beacon::lower` —
  и мог погасить маяк, только что взведённый новым сервером; другой клиент
  засыпал навсегда. Теперь рядом с маяком — секция `<имя>_beacon_state`
  (`magic` `'XBCN'`, `generation`, PID и время создания сервера);
  `Beacon::raise` публикует их и увеличивает поколение до `NtSetEvent`;
  клиент вызывает новый `Beacon::clear_if_stale() -> Result<bool>`: живой
  сервер не трогается, маяк упавшего сбрасывается с перепроверкой поколения
  (взвод нового сервера возвращается). `Beacon::lower` — только для сервера.
  Испорченное состояние (`magic`) — маяк не сбрасывается. `lower` сервера
  обнуляет PID/время до сброса (иначе маяк, возвращённый клиентом после
  штатного ухода живого сервера, оставался взведённым без лобби). Секция
  сначала открывается и создаётся только при отсутствии; нет привилегии/прав
  (`Global\` из сессии != 0) — маяк без состояния, как в 0.9 (гонка в этом
  режиме и с нативным сервером без состояния остаётся). Пир-видимо:
  `INTEROP.md` §b.2, §b.3, «Изменения 0.10» п. 16.
- **Medium** (находка 21) `SharedServer::wait_for_client` возвращал
  `HandshakeFailed` на `C2S_CONNECT_REQ` без `CLIENT_HELLO` — любой процесс
  (или «толчок» клиента Multi, или клиент, откатившийся по таймауту) одним
  сигналом обрывал ожидание; worker Auto сообщал это в `on_error`. Теперь
  ложный сигнал поглощается, ожидание (`wait_for_client`,
  `wait_for_client_or`) продолжается на остаток таймаута. `accept_pending`
  (Multi) по-прежнему отличает сигнал без заявки и молча его пропускает.
- **Medium** (находка 22) Ошибка `NtQueryInformationProcess` при
  определении сессии давала сессию `0`: имя `Local\X` молча уходило в
  глобальный `\BaseNamedObjects`. Теперь `to_nt_path`/`NtName::new`
  возвращают `WindowsError { context: "NtQueryInformationProcess(session)" }`.

- **Medium** (находка 23) Наблюдение за пиром шло только по PID: процесс,
  получивший PID умершего пира (или пир, записавший чужой PID), считался
  живым пиром — смерть не замечалась, захват слота Multi не освобождался.
  Теперь рядом с PID в `ControlBlock` лежит время создания процесса
  (`server_start_*`, `client_start_*`, `owner_start_*` на смещениях 36..56,
  `slot_count`@60 — число слотов Multi, см. находку 24; `reserved`
  удалён); процесс с другим временем создания — не пир. `0` — время не сообщено, сверки нет.

- **High** `MultiServer`/`DispatchServer`, отпущенные без `stop()`, жили
  вечно: поток держал сильную ссылку на сервер. Теперь ручка
  (`Arc<MultiServer>`/`Arc<DispatchServer>`) отделена от ядра, которое держат
  потоки; `Drop` последней ручки останавливает сервер.
- **High** Dispatch: регистрации без ограничения (секция + потоки на каждую)
  — исчерпание памяти любым локальным процессом. Лимит `max_clients`
  (64) и `admit` до создания канала.
- **High** (security review) Писатель брал `write_pos` из разделяемой памяти
  без проверки выравнивания: читатель-пир, записав невыровненную позицию у
  конца кольца, заставлял `commit` писать заголовок кадра за пределы кольца.
  Теперь обе позиции проверяются до записи (`writer_used`), а индекс
  заголовка кадра проверяется и в release.
- **Medium** Гонка инициализации: `magic` писался в начале и не атомарно —
  клиент Multi мог захватить слот, а `reset` стирал его claim. Теперь сегмент
  публикуется `magic`-ом последним (release).
- **Medium** Читатель доверял `message_count` и не сверял кадр с
  `write_pos`: враждебный пир зацикливал чтение мусором. Теперь границы,
  выравнивание и длина проверяются, ошибка — `Corrupted`; счётчик не уходит
  ниже нуля; запись с перезаписью не зацикливается.
- **Medium** Dispatch: `on_client_disconnect_reason` мог прийти после
  `stop()` (поток закрытия канала не отслеживался); `disconnect_client` из
  колбэков двух клиентов друг на друга — взаимная блокировка; `expect` при
  создании потоков — паника в библиотеке.
- **Medium** Auto: очередь `send` росла без предела, пока worker занят
  колбэком (лимит применялся только в worker-е). Теперь лимит соблюдает
  отправитель.
- **Medium** Multi: очередь клиента росла без предела без подключения;
  `Corrupted` от клиента оставлял слот «подключённым» навсегда (теперь
  разрыв с причиной `Error`); после `stop()` из колбэка доставлялся остаток
  пачки.
- **Low** События канала открываются с проверкой типа (подменённое событие
  с ручным сбросом гоняло worker на 100% CPU). `SPACE` больше не голодает при
  непрерывном `DATA`. Копирование из кольца — без `&[u8]` на разделяемую
  память. Перебор слотов Multi отображает только заголовок сегмента.
  `compile_error!` на не-Windows.
- **Low** Чужая секция под именем канала проверяется на тип
  (`NtQuerySection`): `SEC_RESERVE` (страницы не выделены — access violation
  при первом обращении), `SEC_IMAGE`, `SEC_FILE` и всё без `SEC_COMMIT`
  отвергаются `Corrupted` до отображения — в `Mapping::open` и в
  `Mapping::open_header` (перебор слотов Multi).
- **Medium** Dispatch: сверка согласованности самоописания клиента
  выделенного канала с самоописанием в лобби. Если PID рукопожатия лобби
  сверен (пир сообщил PID и ненулевое время создания процесса, и они
  совпали с живым процессом), пир канала обязан сообщить тот же PID и
  ненулевое совпадающее время; иначе канал закрывается до
  `on_client_connect`, обработчику — `on_error(Some(id),
  ShmError::HandshakeFailed)`. Пир без времени создания в лобби — не сверен
  (`handshake_pid == None`), сверки нет. **Это не аутентификация**: PID и
  время пир пишет в память сам, время чужого процесса читаемо, секция лобби
  (NULL DACL) читаема — имя канала не секрет; внутри одной учётной записи
  границы нет. Защиту дают политика безопасности объектов (ObjectSecurity)
  и приватное пространство имён. Пир-видимо: `INTEROP.md` §d.2 шаг 9, §h
  п. 6, «Изменения 0.10» п. 21.
- **Medium** Dispatch: мусор в лобби (не регистрация, хвост, битый UTF-8)
  оставлял клиента без ответа до `response_timeout` — теперь сразу
  `STATUS_REJECTED`. Клиент лобби ждёт ответ по `[S2C_DATA,
  S2C_DISCONNECT, процесс сервера]`, а сервер закрывает каждую сессию лобби
  сигналом `DISCONNECT`: сервер, ушедший без ответа (остановка посреди
  допуска, ошибка, смерть процесса), больше не держит клиента весь
  `response_timeout`; остановленный во время `admit` сервер канал не
  создаёт. Пир-видимо: `INTEROP.md` §d.2 шаги 6–7, «Изменения 0.10» п. 19–20.
- **Low** Раннее вытеснение (Task 18, наблюдение Task 21): счётчик
  `message_count` законно отстаёт от `read_pos` на 1 (читатель уменьшает его
  после CAS), и при `max_messages > 1` запись с перезаписью вытесняла
  сообщение, хотя в кольце было `max_messages − 1` сообщений, а `try_send`
  получал ложный `QueueFull`. Теперь (решение 28, константа
  `RECOUNT_MAX = 64`, часть протокола) при `max_messages ≤ 64` счётчик на
  пределе в непустом кольце пересчитывается по кадрам того же снимка
  (`RingBuffer::messages_between`: без PAD, ≤ 129 заголовков) — лимит
  точный; при `max_messages > 64` «полно» только при `message_count >
  max_messages` (O(1); кольцо может держать `max_messages + 1`). Первая
  версия пересчитывала без потолка — O(`max_messages`) на запись на пределе
  (release, 100 000 записей: 2,1 мкс при 500, 38 мкс при 10 000, 339 мкс при
  100 000; враждебный читатель с `message_count = u32::MAX` усиливал это до
  всего окна); с потолком — 76 нс при 16 и 32–37 нс при 500–100 000.
  Гарантия лимита — при честном читателе (§h п. 9). Читателю не видно.
  `INTEROP.md` §c.5, §h п. 9, «Изменения 0.10» п. 27.
- **Low** `SharedClient::connect_async`: сервер, умерший между открытием
  сегмента и открытием его процесса (Task 16 N1), не наблюдался
  (`open_peer` отвечает «неизвестно» за мёртвый процесс), и future ждал
  `S2C_CONNECT` вечно. Теперь подтверждённо ушедший процесс сервера
  (`is_process_gone`: PID не существует, процесс завершён или PID занят
  процессом с другим временем создания) — `Err(PeerDied)` с откатом заявки,
  как при таймауте. Пиру не видно.
- **Medium** i686: `SharedView::attach` считал концы колец (`offset + 64 +
  capacity`) до проверки границ, и смещение кольца из `LayoutBlock` у
  верхней границы `u32` переполняло 32-битный `usize` — паника в debug-сборке
  клиента (враждебный сервер ронял процесс клиента), обёрнутое значение в
  release (отказ всё равно давал `ring_ok`). Концы считаются только после
  проверки `offset + 64 + capacity <= segment_size` (`checked_add`). Найдено
  тестом порчи `tests/hostile.rs` на i686 (Task 18); регрессия —
  `attach_rejects_ring_offset_near_u32_max`. Пиру не видно.
- **Important** **Конкурирующие клиенты single-client канала (ревизия 3
  рукопожатия, INTEROP R15/R16/R18, Task 22).** Заявка стала исключительной:
  клиент захватывает `ControlBlock::slot_claim` токеном (уникален и внутри
  процесса), второй получает `ShmError::AlreadyConnected` сразу
  (`AutoClient` — повтор через `reconnect_delay`). `CLIENT_HELLO` ставится
  CAS-ом из `IDLE`; принятие — по фиксации своей заявки (`client_state ==
  SERVER_READY`, или `IDLE` при сменившемся `generation`; сигнал при стоящем
  `CLIENT_HELLO` — устаревший); уход — только при неизменном `generation`
  сессии, claim освобождается последним. Сервер сбрасывает
  `S2C_DISCONNECT` до фиксации (прощальный `DISCONNECT` быстро ушедшего
  клиента не терялся), снимает заявку подтверждённо умершего клиента до
  фиксации, потребляет самоописание клиента (`client_pid`/`client_start`) и
  берёт claim сессии, только застав владельца в сессии, освобождает claim
  умершего в сессии сразу, а протухший — по толчку проигравшего клиента
  через `RESERVE_TIMEOUT` (без таймера: «ноль пробуждений в простое»
  сохранён). Устранены: два писателя в одном SPSC-кольце, сессия «подключён
  к никому», разрыв чужой сессии уходящим «призраком» (Concern 1 ревью
  Task 18; `handshake_storm`: принятий клиентами было 1600 при 1593 сессиях
  сервера, теперь ровно столько же). Токены Multi — тот же генератор
  (`claim_token`: `fmix32(seed + n)`, раньше в процессе мог повториться).
  Клиент лобби Dispatch, получивший мьютекс брошенным, перехватывает claim
  умершего держателя — только при `client_state == IDLE` (стоящую заявку
  умершего снимает сервер). Снятие заявки умершего (S2) перепроверяет
  `slot_claim` прямо перед CAS: устаревшие чтения не снимают заявку живого
  следующего клиента. Доказательство — исчерпывающая модель
  `handshake_model` (≈1,9 млн состояний двух клиентов и сервера; она же
  нашла два пробела дизайна — правило C10 и потребление самоописания в S6 —
  и фиксирует, что без отклонений инварианты нарушаются).
  API: `SharedClient::connect`/`connect_async` при занятом канале —
  `AlreadyConnected` вместо `Timeout`/перетирания чужой заявки;
  `AutoHandler::on_error(AlreadyConnected)` — с растущим отступом, пока
  канал занят; выделенный канал Dispatch — `on_error` +
  `on_disconnect_reason(Error)` (канал занят чужим). Формат памяти не
  менялся (`0x00020000`): черновые нативные пиры обязаны перейти на
  ревизию 3 к выпуску (смешанный режим с клиентом без claim не защищён —
  INTEROP §h п. 10). `INTEROP.md` §b.2, §c.1, §d.2, §e.2, §e.4, §g, §h, §i,
  «Изменения 0.10» п. 28.

### Исправлено (финальное ревью ветки, 17.10.2026)

- **Important** Разрыв по ошибке у клиента Auto (и выделенного канала
  Dispatch) заклинивал канал. После `DisconnectReason::Error` (кольцо
  `Corrupted` при чтении или записи, ошибка ожидания) клиент помечал себя
  отключённым, и `Drop` пропускал уход L. Claim освобождался, но
  `client_state` оставался `SERVER_READY`, а `DISCONNECT` не взводился.
  Сервер считал себя подключённым и ждал без таймаута. Каждое повторное
  подключение получало `AlreadyConnected` (C6), и так до перезапуска
  одного из процессов. Теперь при `Error` клиент выполняет полный уход L:
  `IDLE`, затем `DISCONNECT`, claim освобождается последним. Это общий
  `SharedClient::end_session`, тот же путь, что у Multi. Тесты:
  `auto::claim_tests::client_error_leaves_channel_cleanly`,
  `dispatch::tests::dedicated_channel_client_error_reaches_server`.
- **Important** Разрыв по ошибке у `AutoServer` (и серверной стороны
  канала Dispatch) не сигналил `DISCONNECT`: сервер ставил `IDLE`, а клиент
  спал «подключённым» к никому. `DispatchClient` оставался в состоянии
  `is_connected() == true` на уже закрытом канале. Теперь сервер вызывает
  `SharedServer::disconnect_local`: сначала `IDLE`, затем `DISCONNECT`
  (R14), как Multi `force_disconnect`. Конец сессии лобби идёт через тот же
  помощник. Тесты: `auto::claim_tests::server_error_signals_disconnect`,
  `dispatch::tests::dedicated_channel_server_error_reaches_client`.
  Поведение видно пиру: нативная сторона теперь получает `DISCONNECT` от
  Rust при ошибке в своём кольце. Описано в `INTEROP.md` §c.5, §d.2 шаг 7,
  §e.2 п. 5, R7 и «Изменения 0.10» п. 29.
- **Minor** Клиент Auto сверяет `generation` сессии на каждом проходе и
  пробуждении worker-а, как MultiClient. Сессия чужая — уход без записей и
  без `DISCONNECT`. Сигнал, потреблённый ожиданием, возвращается.
  `SharedClient::send_to_server`, `try_send_to_server`, `send_with`,
  `receive_*`, `poll_server*` и `wait_for_space*` при сменившемся
  `generation` отвечают `NotConnected`. Раньше «не заметивший» клиент,
  у которого сервер отнял claim по дедлайну и принял другого, писал в
  кольцо новой сессии (второй писатель SPSC). Тесты:
  `auto::claim_tests::client_notices_foreign_session_on_wakeup`,
  `client::tests::stale_session_refuses_to_send`. INTEROP «Изменения 0.10»
  п. 30.
- **Minor** Проигравший `AutoClient` больше не шумит
  `on_error(AlreadyConnected)` 4 раза в секунду без конца. Пауза при занятом
  канале удваивается от `reconnect_delay` до `BUSY_RECONNECT_CAP` (2 с, или
  `reconnect_delay`, если он больше). Первый повтор по-прежнему идёт через
  `reconnect_delay`. Любой другой исход попытки сбрасывает отступ. Тест
  `auto::claim_tests::already_connected_backs_off`.
- **Minor** Задокументировано (rustdoc `DispatchClient::connect`, INTEROP
  «Изменения 0.10» п. 28.12): если держатель мьютекса лобби умер с
  поставленной заявкой или в сессии, `connect` сразу возвращает
  временный `AlreadyConnected`. Заявку умершего снимает сервер
  (наблюдение процесса или S2), и повтор вызывающего проходит. Внутри
  `lobby_register` повтора нет намеренно: ждать пришлось бы опросом под
  мьютексом лобби.
- Документация: в INTEROP поля `ControlBlock` в нормативном тексте теперь
  названы по именам (`slot_claim`, `slot_owner_pid`, `server_pid`,
  `client_pid`), а не `reserved[N]`. Из преамбулы и §a убраны устаревшие
  фразы про `feat/beacon` и ссылки на номера строк.
- **Minor** Отказ сигнала `S2C_CONNECT` в конце рукопожатия сервера (S7)
  оставлял сессию полуоткрытой: `complete_handshake` возвращал ошибку, но
  `server_state` и `client_state` уже стояли `SERVER_READY` (фиксация S5
  состоялась), а `DISCONNECT` не взводился. Клиент, засчитавший подключение
  по проигранному отзыву, спал «подключённым» к никому. Теперь сессия
  закрывается с сигналом, как `disconnect_local`: сначала `IDLE`, затем
  `DISCONNECT`, ошибка возвращается вызывающему. Так же поступает нативная
  реализация (xipc). Тест
  `server::handshake_tests::failed_connect_ack_closes_the_session`. INTEROP
  §d.2 шаг 4 (S7) и «Изменения 0.10» п. 29.
- Тесты: `tests/ordering.rs::test_bidirectional_ordering_stress` больше не
  синхронизируется фиксированным `sleep`. Клиент повторяет подключение до
  дедлайна, а его ошибка проверяется раньше ошибки сервера. Раньше на
  загруженной машине тест падал с `server error: Timeout`, скрывая
  настоящую причину.

### Исправлено (надёжность, 17.10.2026)

- **Major** Чужая секция с атрибутом кэширования под именем канала
  принималась. Проверка `NtQuerySection` отвергала только `SEC_RESERVE`,
  `SEC_IMAGE`, `SEC_FILE` и отсутствие `SEC_COMMIT`, поэтому процесс,
  занявший имя секцией `SEC_COMMIT | SEC_WRITECOMBINE` (`0x40000000`),
  подсовывал память без порядка записей: TSO, на котором стоят все
  Release/Acquire-публикации кольца, там не действует. С `SEC_NOCACHE`
  (`0x10000000`) каждая атомарная операция шла мимо кэша (DoS по
  латентности). Теперь `Mapping::check_commit` отвергает и эти два
  атрибута (`ShmError::Corrupted`, до отображения) на всех путях открытия
  чужой секции: `Mapping::open` (канал), `Mapping::open_header` (перебор
  слотов Multi) и `Mapping::open_or_create` при занятом имени (состояние
  маяка). Проверка остаётся запрет-списком: `SEC_LARGE_PAGES` и
  неизвестные служебные биты ядра допускаются, чтобы честная секция не
  получила ложный отказ на другой сборке Windows. Константы `SEC_NOCACHE`
  и `SEC_WRITECOMBINE` добавлены в `ntapi::types`. Тесты:
  `win::tests::{open_rejects_uncached_sections,
  own_section_attributes_pass_check}`,
  `tests/hostile.rs::client_rejects_uncached_squatter`. Поведение видно
  пиру, нативная реализация (xipc) обязана повторить. INTEROP §b.2,
  §c.5 п. 1, §g п. 14 и «Изменения 0.10» п. 31.
- **Major** Синхронный читатель не видел штатного ухода пира.
  `poll_client`/`poll_server` ждали только `[DATA, процесс пира]`: клиент,
  ушедший штатно (`IDLE`, потом `DISCONNECT`), оставлял сервер
  «подключённым» — `poll_client` отвечал `Ok(true)` на устаревший `DATA` или
  `Ok(false)` по таймауту бесконечно, а `wait_for_client` давал
  `AlreadyConnected`. Теперь набор ожидания — `[DATA, S2C_DISCONNECT,
  процесс пира]` (`RingBuffer::wait_for_data` с `DisconnectWatch`, как у
  `wait_for_space`): уход пира (сервер — `client_state != SERVER_READY`,
  клиент — `server_state != SERVER_READY` или сменился `generation`) —
  сначала дочитать кольцо (`Ok(true)`), потом `Err(NotConnected)` и пометка
  отключения; событие возвращается взведённым; устаревший `DISCONNECT`
  поглощается, ожидание продолжается остатком таймаута. Пир, ушедший до
  вызова, распознаётся по памяти без ожидания и без спина. Anonymous-сервер
  (с Task 13) — тот же набор на безымянных событиях. Простой —
  по-прежнему одно ожидание в ядре. Тесты:
  `ring::try_write_tests::{wait_for_data_wakes_on_disconnect_and_ignores_stale_signal,
  wait_for_data_blocked_reader_wakes_on_leave}`,
  `ring::tests::departed_peer_skips_spin`,
  `server::poll_tests::{poll_client_sees_graceful_leave_after_drain,
  blocked_poll_client_wakes_on_client_leave,
  poll_client_idle_is_single_kernel_wait}` (`anonymous_poll_sees_client_state_idle`
  заменён `server::anonymous_tests::anonymous_driver_leave_is_seen`, Task 13),
  `client::tests::{poll_server_sees_server_leave,
  poll_server_spin_sees_server_leave, blocked_poll_server_wakes_on_server_leave}`,
  `tests/peer_death.rs::shared_server_sees_client_death` (после `PeerDied`
  канал принимает нового клиента). Новых обязанностей у пира нет — только
  R14 (состояние `IDLE` до `DISCONNECT`); INTEROP §e.1, §i.6, §f, R14.
- **Minor** Сервер, сбрасывая сессию (`mark_disconnected`), писал
  `handshake_state = IDLE` в оба заголовка колец безусловно и мог затереть
  `CLIENT_HELLO` (шаг C7) следующего клиента, вошедшего сразу после ухода
  прежнего. Теперь — CAS `SERVER_READY → IDLE`, как у `client_state`. Поле
  информационное (Rust его не читает, следующий S3 перезаписывает), на
  рукопожатие не влияло. INTEROP §e.2 п. 5.
- **Major** `connect_async`/`connect_async_in` могли украсть подтверждение
  следующего клиента. На готовых исходах future не снимал свои регистрации
  реактора (в отличие от `recv_async`/`send_async`/`accept_async`): когда
  сервер умирал во время рукопожатия (`PeerDied`) или регистрация не
  удавалась, отзыв заявки отпускал claim, а регистрация на автосбросном
  `S2C_CONNECT` оставалась живой до `Drop` future. Следующий клиент (любой
  процесс) захватывал канал, сервер фиксировал его заявку, а сигнал
  `S2C_CONNECT` потребляла брошенная регистрация — клиент ждал до таймаута
  (сколь угодно долго, если исполнитель держит future после `Ready`). На
  успехе живой оставалась регистрация на процессе сервера (и его handle).
  Теперь на каждом исходе (подключение, смерть сервера, отказ регистрации,
  отказ сигнала, `Drop`) регистрации снимаются и наблюдение за сервером
  отпускается ДО `finish`/`withdraw` — то есть до отпуска claim-а;
  потреблённый `S2C_CONNECT` не возвращается (пока claim наш, сигнал
  адресован нам). Тесты: `client::tests::{connect_async_peer_death_frees_ack_before_claim,
  connect_async_register_failure_frees_ack_before_claim,
  connect_async_ready_clears_all_registrations}`. Пир-невидимо (только
  async API Rust; синхронный путь регистраций не имеет).
- **Major** Клиент писал в сессию, которую сервер уже закрыл. Сверка
  `generation` (final review M1) отвергала только запись в сессию
  следующего клиента; после ухода сервера (`server_state = IDLE`,
  `DISCONNECT`) и до рукопожатия следующего клиента «не заметивший»
  клиент продолжал успешно писать в кольцо, которое S3 следующей сессии
  сбросит, а запись, начатая до сброса и законченная во время него, могла
  оставить кадр в кольце новой сессии. Теперь все пути записи
  `SharedClient` (`send_to_server`, `try_send_to_server`, `send_with`,
  `wait_for_space(_spin)`, а через них `send_async` и очередь `AutoClient`)
  требуют ещё и `server_state == SERVER_READY` (`ensure_writable`: одна
  Acquire-загрузка на запись, без системных вызовов) — иначе
  `Err(NotConnected)`. Чтение (`receive_from_server`, zero-copy
  `receive_with`, `recv_async`, доставка Auto) сверяет `generation` ещё раз
  прямо перед фиксацией `read_pos` (`RingBuffer::read_message_checked`/
  `read_with_checked`): сессия сменилась после проверки на входе — кадр
  новой сессии не забирается и не отдаётся (`f` не вызывается), `out`
  очищается. `Corrupted` от колец, сброшенных следующей сессией, при уже
  ушедшем сервере — `NotConnected` (только путь ошибки). Остаток —
  документированная граница (INTEROP §h п. 11): у записи — застрявшая
  между проверкой и публикацией дольше `RESERVE_TIMEOUT` и рукопожатия
  следующего клиента (вытеснение потока или столь же долгое замыкание
  `send_with`); у чтения — дочитывание старых кадров, совпавшее со сбросом
  колец S3, охраняет CAS `read_pos` (вред — только при ABA `read_pos ==
  0`), у zero-copy окно включает `f` (Auto zero-copy может получить
  `on_error(Overrun)`). Тесты (с
  настоящим вторым клиентом, захватившим канал через дедлайн по толчку):
  `client::tests::{client_refuses_to_write_after_server_left,
  stale_client_neither_writes_nor_signals_into_new_session,
  stale_reader_does_not_deliver_foreign_frame}` (последний — с хуком
  `BEFORE_READ`, сменяющим сессию между сверкой и чтением). Формат и
  обязанности пира не меняются; нативному клиенту (xipc) — ЖЕЛАТЕЛЬНО так
  же. INTEROP «Изменения 0.10» п. 30, §h п. 10–11.
- **Minor** `AutoClient`/`AutoServer`: смена сессии посреди прохода worker-а (между
  сверкой `generation` в начале прохода и записью очереди) давала ложный
  `on_error(NotConnected)` и откладывала уход до следующего пробуждения —
  при чужой сессии его могло не быть до её собственного сигнала. Теперь
  `NotConnected` при записи очереди — конец сессии, а не ошибка канала:
  сообщение возвращается в голову очереди (уйдёт в следующую сессию) без
  `on_error`, а после записи и приёма проход сверяет `generation` и сразу
  уходит с `Graceful`. Тест `auto::claim_tests::foreign_session_mid_pass_reports_graceful_without_error`
  (хук `WorkerPoint::BeforeFlush` останавливает проход, пока сервер
  отключает клиента и принимает другого): причина ровно `[Graceful]`,
  ошибок к моменту ухода нет, в кольце сессии B нет кадра A, после ухода B
  сообщение A доставлено серверу ровно один раз. Пир-невидимо.
  Ветка `NotConnected` общая (`process_send_queue`): у `AutoServer`
  клиент, ушедший посреди записи очереди, тоже больше не даёт
  `on_error(NotConnected)` на сообщение — меняется только шум, судьбу
  очереди сервера решает конец сессии (следующий пункт). `AutoClient`
  сохраняет очередь для следующей сессии с тем же сервером (семантика
  переподключения).
- **Minor** `AutoServer` отдавал следующему клиенту сообщения, принятые
  для ушедшего: неотправленная к концу сессии очередь (`send`/`try_send`,
  которым не хватило места в кольце) уходила тому, кто подключится
  следующим. Теперь в конце сессии (уход, смерть клиента, разрыв по
  ошибке) worker сбрасывает очередь — она предназначалась ушедшему
  клиенту (Ruling S) — и сообщает об этом один раз, ДО
  `on_disconnect_reason`: `on_overflow(ServerToClient, n)` и новый счётчик
  `AutoStatsSnapshot::discarded_messages`, без `on_error` и не на
  сообщение. Граница — момент, когда worker ОБНАРУЖИЛ конец сессии:
  `send` после ухода клиента, но до разбора его `DISCONNECT`, тоже
  сбрасывается; отправленное из колбэка отключения и вообще вне
  подключения (до первого клиента, между сессиями) уходит следующему
  клиенту. Это касается и lossless `try_send`: его гарантия — в пределах
  одного подключения (doc `AutoServer::try_send`,
  `AutoHandler::on_overflow`). Каналы Dispatch (поверх `AutoServer`):
  сброс тот же, а отчёт доходит до нового колбэка
  `DispatchHandler::on_overflow(client_id, count)` (по умолчанию пустой,
  добавление не ломает реализации) — последним колбэком клиента перед
  `on_client_disconnect_reason`; прежде прокси `on_overflow` не
  передавал вовсе (вытеснение из кольца перезаписью `send_to` и из
  переполненной очереди теперь тоже сообщается). Тесты
  `auto::claim_tests::server_queue_of_departed_client_is_not_delivered_to_next`
  (кольцо A заполнено, остаток в очереди; A ушёл — один отчёт на весь
  остаток, B получает только сообщение, принятое после ухода A),
  `dispatch::tests::dispatch_reports_discarded_queue_before_disconnect`
  (журнал сервера — ровно `[overflow n, disconnect]`). Пир-невидимо.
  `AutoStatsSnapshot` стал `#[non_exhaustive]` (как `FreeSpace`): поля
  читаются как прежде, строит снимок только крейт (снаружи — `Default`),
  новые счётчики больше не ломают потребителей.
- **Minor** `MultiClient`: в окне между уходом сервера (`server_state =
  IDLE`, теперь запись отвергается — пункт выше) и разбором `DISCONNECT`
  запись очереди давала `on_error(NotConnected)` на КАЖДОЕ сообщение и
  теряла их (`max_send_queue` ошибок подряд). Теперь `NotConnected` в
  `flush_queue` возвращает сообщение в голову очереди и прекращает запись
  без `on_error` (как клиент Auto), чтение (`receive_all`) на
  `NotConnected` тоже молчит, а после прохода worker сверяет `generation`
  и при чужой сессии сразу уходит с `Graceful`. Очередь уходит в следующую
  сессию с сервером. Тест
  `multi::tests::flush_after_server_left_keeps_queue_without_errors`
  (64 сообщения в окне: ни одной ошибки, очередь цела и в прежнем порядке,
  в кольцо закрытой сессии ничего не записано). Пир-невидимо.
- **Major** Claim клиента, умершего после того, как сервер сам закрыл его
  сессию, держался до `RESERVE_TIMEOUT` (10 с). Сервер, закрывший сессию
  (Multi `disconnect_client` и разрыв по порче кольца/панике колбэка;
  single-client `disconnect_local`: порча кольца, отказ ожидания, отказ
  S7), claim не трогает — его снимает клиент, получив `DISCONNECT` (L). Но
  клиент, который этого не заметил и умер, claim уже не снимет: слот Multi
  освобождался по дедлайну протухшего захвата, single-client канал — по
  толчку не раньше 10 с от первого наблюдения. Теперь сервер держит handle
  процесса такого клиента («задержавшийся владелец», не больше одного на
  канал/слот) и по подтверждённой смерти (handle сигнален, процесс открыт
  с проверкой времени создания, `slot_owner_pid` = его PID, claim — всё ещё
  токен сессии) снимает claim сразу одним `CAS токен → 0`
  (`server::release_if_holder_dead`). Multi — процесс привязан к порту
  диспетчера у неподключённого слота (событие, не таймер: в простое ноль
  пробуждений); single-client — проверка на толчке `CONNECT_REQ` без
  `HELLO`, без новых ожиданий. Живой держатель claim не теряет (никогда два
  писателя); наблюдение снимается, когда claim сменился (клиент ушёл сам),
  при новом рукопожатии и остановке. `RESERVE_TIMEOUT` остаётся только для
  владельца, чья смерть не подтверждена. Тесты
  `peer_death::{multi_client_dying_after_error_disconnect_frees_slot_at_once,
  auto_server_client_dying_after_error_frees_channel_at_once}` (дочерний
  процесс, порча кольца извне, claim снят ≤ 1 с после убийства, новый
  клиент подключается ≤ 1 с), `multi::tests::client_dying_after_local_disconnect_frees_slot_at_once`
  (после `disconnect_client` владелец подменён на дочерний `cmd`). Пиру
  видно только время освобождения claim-а; нативному серверу (xipc) —
  ЖЕЛАТЕЛЬНО так же. INTEROP «Изменения 0.10» п. 32, §i.2, §i.3, §e.5, R18.
- **Major** `MultiServer`: испорченный кадр (или сдвиг `read_pos` пиром) при
  дочитывании кольца ушедшего клиента рвал слот с `DisconnectReason::Error`
  (`force_disconnect`): причина ухода (`PeerDied`/`Graceful`/причина
  осиротевшего слота) подменялась, ушедшему клиенту взводился лишний
  `DISCONNECT`, а путь ухода, увидев уже отключённый слот, не снимал claim
  умершего — слот ждал `RESERVE_TIMEOUT`. Теперь (решение C) порча при
  дочитывании — отдельный `on_error(Some(slot), Corrupted)` и конец
  дочитывания, а отключение и снятие claim-а — за путём ухода, как уже было
  для паники колбэка при дочитывании. Порча у подключённого клиента —
  по-прежнему разрыв с `Error`. Тесты
  `peer_death::multi_dead_client_with_corrupted_frame_frees_slot_at_once`
  (причина `PeerDied`, один `on_error(Corrupted)`, слот свободен ≤ 1 с),
  `multi::tests::departure_drain_corruption_keeps_graceful_reason` (причина
  `Graceful`, лишнего `DISCONNECT` нет); `hostile_rewind_during_drain_is_corrupted_and_delivers_once`
  проверяет, что само дочитывание слот не рвёт. INTEROP п. 32, §i.2.
- **Minor** `MultiServer`: отказ рукопожатия слота, кроме сигнала без
  заявки, глотался молча — `handle_slot_connect` считал любую ошибку
  `accept_pending` «толчком», и отказ сигнала `S2C_CONNECT` (S7, сессия при
  этом закрывается с `DISCONNECT`) не доходил до обработчика. Теперь
  `HandshakeFailed` (толчок, откат клиента) — молча, остальное —
  `on_error(Some(slot), err)` вне блокировок; клиент закрытой сессии слот
  наблюдает как «задержавшегося владельца» (пункт выше). Тест
  `multi::tests::failed_connect_ack_is_reported_on_slot` (инъекция отказа S7
  на потоке-диспетчере: ровно один `on_error(Some(0), WindowsError)`, клиент
  сходится и подключается). Пир-невидимо.
- **Major** Поток пустых сигналов пира голодил локальную остановку там,
  где цикл поглощает пустой сигнал и ждёт снова, не возвращаясь к
  вызывающему: событие пира стояло в наборе раньше `wake`/остановки, а NT
  отдаёт меньший индекс. Процесс с доступом к имени, взводящий
  `C2S_CONNECT_REQ` чаще, чем сервер ждёт, держал `AutoServer::stop()` и
  `DispatchServer::stop()` (лобби) вечно; поток `S2C_CONNECT` держал
  `stop()` подключающегося клиента Auto/Multi/Dispatch до
  `connect_timeout`/`slot_timeout`/`channel_timeout`. Теперь (Task 10,
  взамен паузы по потоку пустых сигналов; INTEROP §e.1 П1–П3, п. 33):
  `SharedServer::wait_for_client_or` ждёт `[wake, CONNECT_REQ]` и после
  любого возврата сначала проверяет заявку по состоянию
  (`client_state == CLIENT_HELLO` → рукопожатие, даже если проснулся по
  `wake`); ожидание `S2C_CONNECT` (`Hello::wait_ack`) — `[wake,
  S2C_CONNECT, процесс сервера]`, а после `wake` без остановки (`send`
  приложения) — одно нулевое ожидание `[S2C_CONNECT, процесс]`, так что
  поток отправок не голодит подтверждение; чтение регистрации лобби —
  `[стоп, C2S_DATA, процесс клиента]` (здесь голода не было: цикл и так
  сверял `running` на каждом проходе; порядок выровнен по П1; но поток
  пустых `C2S_DATA` скрывал смерть клиента лобби до `lobby_timeout` --
  теперь на каждом пустом проходе одна проверка его процесса). Подключённые
  worker-ы Auto и `MultiClient` порядок сохраняют — решают по состоянию.
  Тесты (с «противником, всегда выигрывающим гонку» — метка
  `ADVERSARY_MARK` в `ControlBlock::slot_count`, сигнал снова взведён к
  каждому ожиданию; без неё гонка двух потоков воспроизводит голод лишь
  изредка): `server::stray_tests::{wake_not_starved_by_connect_req_flood,
  hello_accepted_under_wake_storm, stray_connect_req_costs_one_wakeup}`,
  `client::tests::{connect_stop_not_starved_by_ack_flood,
  connect_accepts_under_send_wake_storm}`,
  `dispatch::tests::{stop_not_starved_by_lobby_connect_req_flood,
  stop_not_starved_by_registration_data_flood}`,
  `tests/hostile.rs::connect_req_flood_does_not_starve_auto_server`
  (атакующие — kernel32 `OpenEventW`/`SetEvent`),
  `tests/async.rs::accept_async_under_flood_accepts`. Пиру ничего не
  обязательно; нативному — ЖЕЛАТЕЛЬНО (п. 33).
- **Minor** Синхронное ожидание `S2C_CONNECT` (`SharedClient::connect`,
  клиенты Auto/Multi/Dispatch) не наблюдало процесс сервера: сервер,
  умерший, не ответив, стоил ожидания всего таймаута (`Timeout`). Теперь
  процесс сервера (PID и время создания из сегмента) открывается при
  заявке (`Hello::post`) и стоит в наборе ожидания: смерть — `PeerDied` с
  отзывом заявки (W); процесс проверяется и на каждом устаревшем
  `S2C_CONNECT` (поток устаревших сигналов не скрывает смерть). Тот же
  handle затем наблюдает сервер подключённого клиента (`Hello::finish`) и
  `connect_async` (второго открытия нет). Сервер, подтверждённо завершившийся
  ещё до заявки (процесса с PID нет либо он сигнален и время создания
  совпало с объявленным — `win::is_announced_process_exited`, уже, чем
  правило `connect_async`: живой процесс с чужим временем — не «завершился»),
  — `PeerDied` сразу, без ожидания (решение U; раньше — весь таймаут, с
  `Duration::MAX` — вечно). Остаток: PID умершего сервера занят процессом
  с другим временем — граница только таймаут вызывающего; обратная сторона
  — `server_pid`, испорченный в PID без процесса, клиент тоже читает как
  «сервер завершился» (`PeerDied`; писатель сегмента и так может сорвать
  подключение, например через `slot_claim`). Тесты
  `client::tests::{connect_sees_server_death_before_timeout,
  connect_sees_server_dead_before_open,
  connect_sees_server_dead_before_open_with_handle_held,
  server_death_seen_under_ack_flood}` (`PeerDied` ≤ 1 с вместо `Timeout`,
  claim свободен, `client_state == IDLE`);
  `peer_tests::corrupted_server_start_hi_is_not_a_live_server` — по-прежнему
  подключение без наблюдения. INTEROP §e.1, п. 33.
- **Minor** `MultiServer`: пустой `CONNECT_REQ` одного слота стоил прохода
  всех слотов (`reclaim_stale_claims(0..max_clients)` в начале каждого
  витка диспетчера — 256 слотов ≈ 257 блокировок на сигнал). Теперь
  (решение R) полный проход — только на ближайшем дедлайне
  (`Dispatcher::sweep_due`) или, оппортунистически на пробуждении, не чаще
  `LIVENESS_CHECK_INTERVAL` (3 с); первое наблюдение claim-а свободного
  слота и закрытие handle отключённых клиентов (`ClientSlot::retired`) —
  в сверке привязок этого слота (`Dispatcher::reconcile`), дедлайн проверки
  рукопожатия назначает обработка `CONNECT_REQ`. Осиротевший слот
  по-прежнему гарантированно ловится дедлайном `HANDSHAKE_VERIFY_DELAY`
  («на ближайшем проходе» — теперь «на ближайшем полном проходе»). Тесты
  `multi::tests::{stray_connect_req_work_is_constant (блокировок слотов на
  пробуждение ≤ 2; до — ≈ 258), connect_req_flood_does_not_starve_dispatcher}`;
  `idle_server_with_foreign_claim_does_not_wake_up` и прочие `idle_*` не
  изменились. Потерянный пост пробуждения слота (`wake_slot`, отказ
  `NtSetIoCompletion`) — отметка `wake_lost`: ближайший виток диспетчера
  сверяет все слоты, handle отключённых клиентов не ждут события своего
  слота (`multi::tests::lost_wake_post_closes_retired_on_next_pass`).
  Пир-невидимо (INTEROP §i.2, §i.3).
- **Minor** Поток пустых сигналов скрывал смерть пира в синхронных
  ожиданиях (ревью Task 10, N1). `poll_client`/`poll_server`
  (`RingBuffer::wait_for_data`), `wait_for_space` и ответ лобби
  (`lobby_register`) поглощали пустой `DATA`/`SPACE` и устаревший
  `DISCONNECT`, а процесс пира стоял в наборе после них: пир, взводящий их
  непрерывно, держал вызов до таймаута (`poll_*` — отвечал `Ok(true)` на
  каждый пустой `DATA`). Теперь на каждом пустом сигнале — одна проверка
  процесса пира нулевым ожиданием (сигнал с данными или местом её не
  стоит): процесс завершился — `PeerDied` (после дочитывания кольца). Тесты
  `ring::stale_flood_tests::{data_flood_does_not_hide_writer_death,
  stale_disconnect_flood_does_not_hide_writer_death,
  space_and_disconnect_floods_do_not_hide_reader_death}` (каждая из четырёх
  проверок подтверждена мутацией). INTEROP §e.1 П1 (формулировка теперь
  верна для всех перечисленных наборов).
- **Minor** `AutoClient` сообщал `on_error(PeerDied)` каждые
  `reconnect_delay` (250 мс), когда секцию мёртвого сервера держал кто-то
  ещё (ревью Task 10, N2: после решения U `PeerDied` приходит сразу, а не
  через `connect_timeout`). Теперь `PeerDied` вне подключения — «сервера
  нет»: одна ошибка на ряд и сон на маяке присутствия до взвода нового
  сервера (п. 34).
- **Important** (ревью Task 11, решение W) Маяк присутствия первой редакции:
  (1) занятый или недоступный `<имя>_beacon` отказывал старт сервера — в том
  числе без злоумышленника, если маяк первым создал клиент другого
  пользователя со своей политикой; теперь маяк — лучшее усилие,
  `presence()` сообщает причину; (2) клиент ждал процесс по личности из
  секции или состояния маяка, которые пишет любой с доступом к ним: время
  `0` («любой процесс с этим PID»), свой PID, подмена — ожидание без конца;
  теперь такие личности отвергаются, секция сверяется с маяком, процесс,
  который не открылся, — резерв без подмены источника, маяк в наборе
  ожидания — только невзведённым; (3) не было предела ожиданию
  обнаружения — теперь `discovery_fallback` (10 с). Маяк базы Multi —
  `<база>_multi_beacon` (M2). Тесты:
  `presence::tests::{absent_beacon_down_retries_on_raise_and_times_out_on_fallback,
  absent_stale_beacon_is_cleared,
  live_beacon_retries_once_per_generation_then_waits_process,
  untrusted_beacon_identity_waits_only_fallback,
  refused_identity_is_cross_checked_with_beacon, wake_is_index_zero,
  reported_dedupes_runs_not_episodes}`,
  `auto::presence_tests::{refusal_by_own_process_is_rechecked_by_fallback,
  spoofed_beacon_identity_does_not_block_discovery,
  server_without_beacon_is_found_by_fallback,
  squatted_beacon_does_not_fail_start, different_hung_servers_are_each_reported}`,
  `multi::tests::{multi_base_beacon_is_distinct_from_channel_beacon,
  multi_client_finds_beaconless_server_by_fallback}` (ключевые проверки
  подтверждены мутацией). Имя канала `<база>_multi` даёт тот же маяк, что у
  базы Multi `<база>`, — так каналы не называть.
- **Critical** (повторное ревью Task 11, решение Y) Горячий цикл попыток
  `AutoClient` на пути отказа: секция отвергает (`AccessDenied`, другой
  формат), а единственная личность сервера — личность уже завершившегося
  процесса в состоянии маяка (её оставляет `clear_if_stale` после падения
  сервера, или подмена). `wait_server` отвечал немедленным `Retry`, попытка
  снова отвергалась — около 50–100 тыс. попыток в секунду при нулевом
  счётчике пробуждений. Теперь такая личность — «личности нет»: ожидание
  маяка (если не взведён) и резервной проверки. PID секции с недоверенной
  личностью (время 0) сверяется с маяком (m2). `discovery_fallback =
  Some(Duration::ZERO)` — `ShmError::InvalidConfig` при подключении (m1).
  `DispatchServer::presence()` — `Disabled` после `stop()` и неудачного
  пересоздания лобби (m4). Тесты
  `auto::presence_tests::{refused_with_dead_beacon_identity_does_not_spin
  (считает попытки подключения: без исправления — 100 821 за 1 с),
  refused_untrusted_section_with_dead_beacon_does_not_spin,
  zero_discovery_fallback_is_rejected}`,
  `presence::tests::{refused_dead_identity_waits_instead_of_retry,
  untrusted_section_pid_still_cross_checks_beacon}`,
  `dispatch::tests::lobby_raises_beacon_automatically` (статус после
  `stop`); исправления C1 и m2 подтверждены мутацией.

- **Minor** (решение X) Синхронный `connect`, увидевший в `Hello::post`, что
  процесс сервера уже завершился, отзывает заявку; если отзыв проигран
  (сервер успел зафиксировать рукопожатие и умер), клиент подключён — а
  `finish()` отбрасывал признак «сервер завершился», и клиент оставался без
  наблюдения процесса: `poll_server`/worker ждали бы вечно. Теперь такое
  подключение получает наблюдение «уже завершился»
  (`ProcessWatch::exited` — всегда сигнальный объект): первое ожидание
  видит смерть, кольцо дочитывается (прощание сервера не теряется), затем
  `PeerDied` — как при любой смерти пира после фиксации; `connect`
  возвращает `Ok`, потому что сессия зафиксирована. Тест
  `client::exited_server_tests::committed_then_dead_server_is_reported_after_drain`
  (хук `AFTER_HELLO`: «сервер» фиксирует и умирает до открытия его
  процесса). Пир-невидимо. Fix round (ревью T11b): наблюдение «уже
  завершился» назначает путь, бросивший заявку как `PeerDied` (у
  `connect_async` — и по широкому `server_gone`: PID сервера занят живым
  процессом с другим временем; M1), а не `finish` по признакам. Живой
  сервер с испорченным временем в сегменте: синхронный `connect` по
  `S2C_CONNECT` по-прежнему подключается без наблюдения; `connect_async`
  (смерть выведена по несовпадению времени) отвечает `PeerDied` (решение
  V), но уход — полный L (CAS пары и `DISCONNECT`), и сервер принимает
  следующего клиента (N1 повторного ревью; тест
  `exited_server_tests::async_inferred_death_still_leaves_live_server_fully`). Тесты `exited_server_tests::{committed_then_dead_server_is_reported_to_recv_async,
  async_gone_server_with_lost_withdraw_is_reported_after_drain}` (M8, M1).
  Rustdoc `SharedClient::connect` описывает исход (`Ok`, затем `PeerDied`
  после дочитывания) и остаток синхронного пути.

- **Minor** (n1 ревью Task 11) `discovery_fallback` короче 1 мс (не только
  ноль) превращал ожидание обнаружения почти в опрос — тысячи попыток в
  секунду. Теперь любой интервал меньше 1 мс — `ShmError::InvalidConfig` в
  `AutoClient::connect`, `MultiClient::connect` и `wait_for_server*`;
  `None` и интервалы от 1 мс допустимы. Тест
  `auto::presence_tests::sub_millisecond_discovery_fallback_is_rejected`.

- **Minor** (T11b, поведение клиента C) `MultiClient` после неудачного
  рукопожатия на слоте снова перебирал слоты с нуля и брал тот же слот:
  слот, рукопожатие на котором не удаётся (испорченное состояние, сервер
  его не обслуживает), держал клиента вечно, хотя другие свободны. Теперь
  следующий перебор начинается со следующего слота и идёт по кругу
  (`claim_free_slot(.., start)`), после подключения — снова с нуля. Тесты
  `multi::tests::{failed_handshake_moves_to_next_slot,
  claim_scan_wraps_around}`. INTEROP §i.1, «Изменения 0.10» п. 36.

- **Minor** (T11b) Клиент регистрации Dispatch (`lobby_register`), увидевший
  штатный уход лобби (`DISCONNECT` при закрытой сессии) или смерть его
  процесса, уходил полным L: CAS состояния не проходил (сервер уже
  `IDLE`), но `DISCONNECT` всё равно взводился — поздний сигнал доставался
  следующему клиенту лобби. Теперь — только снятие claim-а (INTEROP §d.2
  шаг 7; так же клиент xipc). Тест
  `dispatch::tests::lobby_client_leaves_without_disconnect_after_server_died`
  (смерть лобби без изменения состояния — пункт в изоляции) и
  `lobby_client_after_server_disconnect_sends_no_disconnect` (сторожит
  вместе с уходом `SharedClient` ниже). INTEROP «Изменения 0.10» п. 35.

- **Minor** (T11b) Устаревший `DISCONNECT` от `Drop` `SharedClient`: сервер
  закрыл сессию сам (`disconnect`, `Drop` сервера — `client_state` уже
  `IDLE`, `generation` прежний), клиент этого ещё не разобрал и уходил: CAS
  `SERVER_READY → IDLE` не проходил, а `DISCONNECT` всё равно взводился —
  сигнал оставался для следующей сессии канала. Теперь уход сигналит
  `DISCONNECT` только при удачном CAS (сессия была открыта и с точки зрения
  сервера); claim снимается, как прежде. Пир-видимо: INTEROP R15, §d.2
  шаг 7, «Изменения 0.10» п. 35 (нативному клиенту — ЖЕЛАТЕЛЬНО: старое
  поведение безвредно, S4 следующей сессии сбрасывает `DISCONNECT`). Тест
  `client::stale_leave_tests::drop_after_server_closed_session_sends_no_disconnect`.
  Fix round (M5 ревью T11b): claim сверяется прямо перед CAS, `generation`
  — ещё раз после удачного CAS (окно «claim живого задержавшегося отнят по
  `RESERVE_TIMEOUT`, следующий клиент прошёл рукопожатие» сужено, остаток
  — граница решения G, INTEROP §h п. 11); тест
  `client::reclaimed_leave_tests::leave_after_claim_was_taken_writes_nothing`.

- **Minor** (решение AB, M5 ревью T11b) Гонка ухода L закрыта: сверка
  `generation` и 32-битный CAS `client_state: SERVER_READY → IDLE` — два
  шага, и если между ними сервер отнял claim «не заметившего» клиента (по
  `RESERVE_TIMEOUT`) и зафиксировал сессию следующего, CAS снимал чужую
  сессию. Теперь уход — один 64-битный CAS пары `(client_state,
  slot_claim)`: `(SERVER_READY, T) → (IDLE, T)` (`ControlBlock::leave_session`,
  `AtomicU64::from_ptr` над смещением 16; на x86 — `lock cmpxchg` /
  `lock cmpxchg8b`, атомарно относительно 4-байтовых операций пира); у
  следующей сессии claim другой, и её уход не трогает при любом
  вытеснении. Формат памяти не меняется. Заменяет сужение окна из
  предыдущего пункта (сверка claim-а до CAS и `generation` после). Модель
  рукопожатия: поздний уход «забытого» клиента — многошаговый L (≈2,0 млн
  состояний), с 32-битным CAS перебор находит нарушение I5; окно между
  удачным CAS и сигналом `DISCONNECT` — вне модели (допущение A1': лишний
  сигнал в новой сессии, не заклинивание). Переписанный враждебным
  писателем `slot_claim` делает уход тихим — сервер ждёт выхода процесса
  клиента (INTEROP §h п. 11). Тест
  `client::reclaimed_leave_tests::leave_racing_reclaim_and_next_commit_spares_new_session`
  (хук `BEFORE_LEAVE_CAS` между сверкой и CAS; с 32-битным CAS — падает).
  INTEROP R15, §d.2 шаг 7, §h п. 11, «Изменения 0.10» п. 37 (нативному
  клиенту — ЖЕЛАТЕЛЬНО).

- **Important** (T11c, решение AD) Устаревший `DISCONNECT` закрывал живую
  сессию у worker-ов: Auto-сервер и Auto-клиент (`Worker::handle_wait`,
  значит, и выделенные каналы Dispatch), диспетчер `MultiServer`
  (`handle_slot_disconnect`) и `MultiClient` считали **любой** сигнал
  `DISCONNECT` штатным уходом пира. Сигнал при живой сессии возможен:
  окно между удачным CAS ухода прежнего клиента и его сигналом (допущение
  A1', п. 37), пир чужой реализации, посторонний процесс того же
  пользователя. Теперь `DISCONNECT` — уход, только если пир покинул ЭТУ
  сессию: сервер — `client_state != SERVER_READY`
  (`SharedServer::client_left`; `generation` меняет только сам сервер),
  клиент — `server_state != SERVER_READY` или `generation != G`
  (`SharedClient::server_left`). Те же два предиката — у всех
  потребителей (синхронные `poll_*`/`wait_for_space`, async
  `recv_async`/`send_async`, ответ лобби уже следовали правилу; клиентский
  `wait_for_space` сверял только `server_state` — теперь и `generation`).
  Устаревший сигнал поглощается (возвращать некому: пир сигналит, только
  уйдя, R14), сессия живёт; worker-ы Auto и `MultiClient` после него
  проверяют остальные объекты набора одним нулевым ожиданием
  (`ring::rest_after_stale_disconnect`): поток таких сигналов на индексе 0
  не скрывает `SPACE` и смерть пира, данные и остановку проход решает по
  состоянию — работа на сигнал O(1). Диспетчер Multi — одно завершение
  порта и сверка одного слота, привязка `DISCONNECT` ставится снова.
  `MultiClient`, разбуженный `DISCONNECT` чужой сессии (`generation`
  сменился), возвращает сигнал новому клиенту слота (как `DATA` и клиент
  Auto). Настоящий уход — по-прежнему `Graceful` с дочитыванием кольца.
  Тесты: `auto::stale_disconnect_tests::{server_keeps_session_on_stale_disconnect,
  client_keeps_session_on_stale_disconnect,
  stale_disconnect_flood_starves_nothing_on_server,
  stale_disconnect_flood_does_not_hide_server_death}`,
  `multi::tests::{dispatcher_keeps_slot_on_stale_disconnect,
  multi_client_keeps_session_on_stale_disconnect,
  multi_client_sees_server_death_under_stale_disconnect_flood,
  stale_disconnect_flood_does_not_starve_dispatcher}`,
  `dispatch::tests::dedicated_channel_survives_stale_disconnect`,
  `client::stale_disconnect_tests::{sync_poll_absorbs_stale_disconnect,
  client_wait_for_space_sees_foreign_session}`, `tests/async.rs`
  `async_{server_recv,client_recv,send}_absorbs_stale_disconnect`; потоки —
  атакующий без пауз и «противник» `ADVERSARY_MARK` (сигнал к каждому
  ожиданию); без нулевого ожидания тесты потока падают. Тесты,
  моделировавшие уход одним сигналом без смены состояния
  (`hostile_disconnect_flood_does_not_stall_dispatcher`,
  `panic_while_draining_keeps_reason_and_releases_claim`), теперь уходят
  по R14. Пир-видимо: INTEROP §e.1, §e.2 п. 5, §e.4 (R14), §i.2, §i.4,
  §i.6, «Изменения 0.10» п. 37 и **п. 38**. Нативному пиру: сигналить по
  R14 (состояние, потом сигнал) — ОБЯЗАН, иначе любой потребитель Rust
  примет уход за устаревший сигнал; поглощать устаревший `DISCONNECT`
  самому — ЖЕЛАТЕЛЬНО.

- **Important** (Task 12, решение R-O) Claim держателя, умершего до
  `CLIENT_HELLO` (захватил claim и упал, не выставив заявки), single-client
  сервер снимал только через `RESERVE_TIMEOUT` (10 с) от первого толчка, и
  то лишь благодаря повторным толчкам опрашивающих клиентов; Multi — по
  дедлайну диспетчера. Теперь на толчке (`CONNECT_REQ` без заявки) сервер
  (single-client `observe_idle_claim`, диспетчер Multi
  `handle_slot_connect`) при `client_state != CLIENT_HELLO` и
  подтверждённо умершем владельце (`slot_owner_pid` + `owner_start`,
  `server::release_if_owner_gone`) снимает claim сразу и уведомляет ждущих.
  ABA (окно C1–C2 нового держателя, поля владельца ещё прежние): живой
  держатель теряет claim; двух писателей нет (C6 — CAS из `IDLE`, отъём
  только при `IDLE`, перечитанном прямо перед CAS). Первая редакция
  утверждала, что ограбленный «получает `AlreadyConnected` и повторяет», —
  неверно, если его заявку успели зафиксировать (fix round 1 ниже). Тесты:
  `tests/peer_death.rs` `holder_dead_before_hello_reclaimed_on_first_nudge`
  (было 13,4 с, стало ≤ 500 мс), `multi::tests::dead_owner_claim_released_on_nudge`.
  Пир-видимо: INTEROP R18, §e.5, §i.3, п. 39 (нативному серверу —
  ЖЕЛАТЕЛЬНО).
- **Important** (ревью Task 12 I1, решение AE) Сессия-призрак после отъёма
  R-O: claim держателя снят в окне C1–C2 (поля владельца — прежнего,
  умершего), а его заявку сервер зафиксировал по чужому толчку до C8.
  Отзыв на C8 проигрывал, клиент уходил CAS пары `(SERVER_READY, T)` с
  отнятым токеном — CAS не проходил, ни `IDLE`, ни `DISCONNECT`; сервер
  оставался в сессии с ушедшим клиентом до выхода его процесса, каждый
  следующий клиент — `AlreadyConnected`. Исправлено с двух сторон:
  (а) клиент на проигранном отзыве C8 при `SERVER_READY` возвращает себе
  claim (`CAS 0 -> T`, ожидание занятого — до 100 мс), пишет поля владельца и
  уходит обычным L (`Hello::leave_robbed`); 32-битный CAS `client_state` при
  `generation == G` отвергнут — после отъёма I2 не держится, довод решения
  AB. (б) Сервер: быстрый путь перечитывает `client_state == IDLE` и PID
  владельца прямо перед CAS и очищает поля умершего CAS-ом
  (`ControlBlock::forget_owner`) — так же перед каждым снятием claim-а
  подтверждённо умершего (S2, S6 `abort_session`, `mark_disconnected`,
  «задержавшийся», дедлайн протухшего, Multi). Клиент очищает свои поля
  владельца перед каждым освобождением своего claim-а
  (`ControlBlock::clear_owner`, `presence::release_own_claim`) — устаревшая
  личность остаётся только после пира, этого не делающего. Модель
  `handshake_model`: поля владельца, C2 отдельным шагом, отъём R-O тремя
  шагами, свойство W (нет сессии-призрака, когда все клиенты ушли); без
  возврата claim-а и без перепроверки `client_state` перебор находит
  призрак. Тесты: `client::tests::{robbed_committed_hello_leaves_no_ghost,
  own_release_clears_owner_fields}` (воспроизведение ревью),
  `server::rob_tests::*`, `handshake_model_rob_without_retake_leaves_ghost`,
  `handshake_model_rob_of_posted_hello_leaves_ghost`. Пир-видимо: INTEROP
  §d.2 C2/C8, R18, §h п. 12, §g п. 17, п. 39 (очистка полей перед
  освобождением — ОБЯЗАН; возврат claim-а на C8, перечитывание и очистка в
  R-O — ЖЕЛАТЕЛЬНО).
- **Important** (ревью Task 12 I2, решение AE) Ожидание занятого канала без
  страховки: держатель-пир без R20 (клиент xipc до зеркала, сборка ветки до
  Task 12 с тем же форматом памяти), освободивший claim без уведомления,
  держал Rust-ждущих до выхода своего процесса. Теперь ожидание
  `AutoClient` (занятый канал), `MultiClient` (`NoFreeSlot`) и
  `wait_connectable` — не дольше `discovery_fallback` (по умолчанию 10 с;
  `None` — только события), затем попытка; `on_error` — по-прежнему раз на
  эпизод. Только пока клиент ждёт занятый канал — простой подключённого
  клиента не задет (тесты `idle_*`). Новое: `SharedClient::wait_connectable_with`
  (`DiscoveryOptions`: `fallback`, `namespace`). Тесты:
  `auto::free_tests::silent_release_found_within_discovery_fallback`,
  `client::tests::wait_connectable_fallback_finds_silent_release` (с `None` —
  не находит). Пир-видимо: INTEROP R21, §e.1, §e.2 п. 7, §e.5, §h п. 12.
- **Minor** (повторное ревью Task 12, N1–N5, решение AG) Остатки после
  отъёма R-O. N1: ограбленный, не вернувший claim за 100 мс, уходил без
  записей — сессия-призрак навсегда (модель с веткой отказа нарушала W);
  теперь возврат claim-а ждёт по протоколу R21 на `<имя>_free` и процессе
  держателя до остатка таймаута подключения (`connect_async` — 100 мс),
  подтверждённо умершего держателя перехватывает (`CAS held -> T`), а не
  дождавшись — запасной уход 32-битным CAS при `generation` сессии
  (`Hello::leave_by_generation`; остаток — ложный конец следующей сессии,
  не заклинивание; модель — свойство W с веткой отказа,
  `handshake_model_retake_give_up_without_leave_leaves_ghost`). N2
  (воспроизведено): заявка умершего при свободном claim-е занимала канал
  навсегда — теперь клиент, захвативший claim при стоящей заявке, сразу его
  отпускает и толкает сервер (не перезаписывая C2/C5), откат C6, увидевший
  заявку, толкает, ждущий получает ненулевой ключ толчка, а S2 судит
  заявку без держателя claim-а по самоописанию и уведомляет ждущих. N3:
  откат C6 и отзыв снимают своё самоописание CAS-ом от своих значений
  (`ControlBlock::retract_client`). N4: поля владельца очищаются CAS-ом от
  личности, прочитанной вместе с токеном (сервер), или своей (клиент) —
  `ControlBlock::forget_owner(pid, start)`. N5: тесты очистки полей в
  отъёме по `RESERVE_TIMEOUT`, дедлайне Multi, смерти в сессии Multi и
  `mark_disconnected`. Тесты: `client::tests::{robbed_leave_takes_over_dead_holder,
  robbed_leave_falls_back_by_generation, dead_hello_with_free_claim_is_cleared_by_nudge,
  wait_connectable_nudges_dead_hello, withdraw_keeps_next_claimant_self_description,
  own_release_keeps_foreign_owner_fields}`, `server::rob_tests::{reserve_reclaim_forgets_owner,
  mark_disconnected_of_dead_client_forgets_owner}`,
  `multi::tests::{deadline_reclaim_forgets_owner, peer_died_in_session_forgets_owner}`.
  Пир-видимо: INTEROP §d.2 C1/C2/C6/C8/S2, §e.2 п. 7, §h п. 12, §g п. 17,
  п. 39.
- **Minor** (ревью Task 12, m2–m9) `wait_connectable` и ждущий `AutoClient`
  считают занятым и канал без claim-а с заявкой или сессией (`client_state
  != IDLE`): «свободен» уже не возвращает вызывающего в горячий цикл с
  `AlreadyConnected` (m2); лобби в `wait_connectable` задокументировано (m3);
  заголовок секции канала ждущий открывает на каждую попытку, а не держит
  всё ожидание (имя секции упавшего сервера не мешает перезапуску, m4);
  ждущий Multi собирает занятость тем же перебором, что и захват (один
  проход вместо двух; ~1,5 мс на 256 слотов, оценка стада — INTEROP §h п.
  12, m5); запас временных границ тестов под нагрузкой (m6); тесты
  уведомления на `raise`/`lower`, откате C6, S2 и `abort_session` (m7);
  INTEROP §d.2 C8, §h п. 12 (переносы, сброс события захватчиком) (m8);
  лишнее пробуждение ждущих при своём откате C6 задокументировано (m9).
- **Major** (Task 13, решения J (D), R-P) Anonymous-канал жил опросом:
  событий не было, и `wait_for_client_noevent` будил поток раз в 1 мс,
  `wait_for_space` спал по `SPACE_POLL_INTERVAL` (1 мс), `poll_client`
  возвращал `Ok(false)` сразу (ждать было нечего: вызывающий крутил его
  сам), async API отвечал `InvalidConfig`, а драйвер не мог ни разбудить
  сервер, ни дождаться его (`get_event_handles()` — `None`). Теперь секция
  создаётся вместе с 7 безымянными событиями (`SharedEvents::create_unnamed`,
  `EventHandle::create_unnamed_secured` — дескриптор секции), все пути
  опроса удалены (`SPACE_POLL_INTERVAL`, ветки «нет событий» в
  `wait_for_client`, `poll_client`, `wait_for_space`, async, worker-е Auto,
  диспетчере Multi, лобби Dispatch): рукопожатие — одно пробуждение по
  `CONNECT_REQ`, `poll_client`/`wait_for_space` — одно ожидание в ядре на
  весь таймаут, уход сервера драйвер видит по `DISCONNECT`. Тесты (до
  исправления — не компилировались или падали: ноль ожиданий в ядре, возврат
  `poll_client` через 3,5 мкс, `InvalidConfig`):
  `server::anonymous_tests::{anonymous_server_exposes_all_events,
  anonymous_handshake_signalled_by_driver,
  anonymous_poll_client_sleeps_in_kernel,
  anonymous_wait_for_space_is_single_kernel_wait,
  anonymous_driver_leave_is_seen, anonymous_server_leave_signals_driver}`,
  `tests/async.rs::anonymous_accept_and_recv_async`,
  `tests/security.rs::anonymous_events_share_section_descriptor`;
  `ring::tests::wait_for_space_polls_without_event` заменён
  `wait_for_space_timeout_clears_waiter`. Пир-видимо: INTEROP §b.2, §b.5,
  §e.5, §i.6, «Изменения 0.10» п. 40.

### Изменено (надёжность, внутреннее, 17.10.2026)

- Пул пакетов ожидания реактора. `Reactor::register` создавал объект ядра
  (`NtCreateWaitCompletionPacket`) на каждую регистрацию, а снятие его
  закрывало: на горячем пути async (`recv_async`/`send_async` на каждом
  `Pending`) — три системных вызова вместо одного и лишний режим отказа:
  под давлением ресурсов (`STATUS_INSUFFICIENT_RESOURCES`, квота handle-ов)
  ожидание падало посреди живой сессии. Теперь снятые пакеты возвращаются в
  пул таблицы реактора (`POOL_LIMIT` = 64 простаивающих, сверх — закрываются)
  и переиспользуются: в установившемся режиме регистрация объектов ядра не
  создаёт. В пул — только пакет, которого заведомо нет в очереди порта:
  доставленный реактором, не привязавшийся или снятый отменой с «чистым»
  кодом (`WaitPacket::cancel_for_reuse`: `STATUS_SUCCESS`, `STATUS_PENDING`
  → `STATUS_SUCCESS`, `STATUS_CANCELLED`); иной код — пакет закрывается.
  Завершение, извлечённое реактором до снятия записи, несёт старое
  поколение слота и отбрасывается — переиспользованный пакет его не
  получает. Диспетчер Multi (свой порт, пакет на привязку — пул не нужен)
  применяет то же правило: «грязный» код отмены — пакет закрывается, а не
  перепривязывается бесконечно. Отказ привязки
  (`NtAssociateWaitCompletionPacket`) возвращает пакет в пул только при
  доказанной вине объекта ожидания: код её допускает (`STATUS_INVALID_HANDLE`,
  `STATUS_OBJECT_TYPE_MISMATCH`, `STATUS_ACCESS_DENIED`,
  `STATUS_INVALID_PARAMETER_3`) **и** проба пакета отменой отвечает
  `STATUS_CANCELLED` (`WaitPacket::survives_arm_failure`; первые два кода
  ядро отвечает и на негодный пакет). Иначе пакет закрывается, и
  регистрация с пакетом из пула (привязка Multi с прежним пакетом) сразу
  повторяется один раз с новым — «грязный» пакет не отказывает каждой
  следующей регистрации и не перепривязывается через `REARM_BACKOFF`
  бесконечно. Пул не сжимается: после всплеска до 64 простаивающих handle-ов
  остаются у общего реактора до конца процесса. Простой реактора — по-прежнему ноль
  пробуждений. Тесты: `reactor::tests::{registrations_reuse_pooled_packets,
  pool_is_bounded, pooled_packet_is_clean_after_every_outcome,
  warm_pool_survives_create_failure, unexpected_cancel_status_is_not_reused,
  target_failure_keeps_pooled_packet, packet_failure_does_not_poison_pool,
  bound_packet_in_pool_is_replaced, survives_arm_failure_requires_idle_packet}`,
  `multi::tests::broken_arm_packet_is_replaced`. Пир-невидимо.

### Сборка

- CI (Task 19): `cargo clippy --release --all-targets -- -D warnings` на x64 и
  i686 — release-профиль давал предупреждение, которое видел только
  потребитель (`let_underscore_drop`, `e046606`).
- Тесты раскладки (Task 19): `layout::tests::initialized_segment_bytes_match_interop`
  (байты сегмента после `SharedView::initialize` по смещениям таблиц INTEROP
  §c.1–§c.3), `wire_constants_are_stable`, `default_and_canonical_layouts_match_interop`;
  у каждой таблицы INTEROP §c — ссылка на свой тест. В INTEROP больше нет
  ссылок на номера строк кода — только имена функций.
- rustdoc `Beacon::open`: маяк без состояния не подхватывает секцию
  `<имя>_beacon_state`, созданную позже, — переоткрыть.
- `ntdll` импортируется через `#[link(name = "ntdll", kind = "raw-dylib")]`:
  `ntdll.lib` из Windows SDK больше не нужна, `build.rs` ничего не линкует.
  Импорты под возможности 0.10: `NtQuerySection` (атрибуты чужой секции),
  токен и ACL (`ObjectSecurity`), частные пространства имён
  (`PrivateNamespace`), IOCP и пакеты ожидания завершения (реактор async и
  диспетчер Multi).

### Изменено (возможности Rust 1.99, внутреннее)

- **Политика отравления блокировок**: все `Mutex`/`RwLock` библиотеки
  захватываются через `LockExt::lock_clean` / `RwLockExt::{read_clean,
  write_clean}` (`unwrap_or_else(PoisonError::into_inner)`) вместо
  `lock().unwrap()`. Паника в колбэке обработчика больше не превращается в
  каскад паник в `stop()`/`Drop` (паника при раскрутке — `abort`):
  `panic = "abort"` манифеста xshm потребителя не касается.
- **`ShmError::WorkerPanicked { message }`** (новый вариант): паника
  рабочего потока не теряется в `let _ = handle.join()`. Куда сообщается:
  - `DispatchHandler::on_error(Some(client_id), ..)` — паника worker-а канала
    клиента (там идут его `on_client_connect`/`on_message`/
    `on_space_available`/`on_client_disconnect_reason`). Сообщается, когда
    канал закрывается (join его worker-а), по пути закрытия:
    - остановка сервера (`stop`/Drop): в хвосте потока лобби, который
      закрывает оставшиеся каналы; `stop()` из чужого потока дожидается
      этого потока, т.е. `on_error` приходит до возврата из `stop()`;
      порядок для клиента — `on_error`, затем
      `on_client_disconnect_reason(Local)`;
    - `disconnect_client` из чужого (не серверного) потока: на этом потоке,
      до возврата; `on_error`, затем `on_client_disconnect_reason(Local)`;
    - `disconnect_client` из колбэка обработчика и отключение самого
      клиента (его `DISCONNECT`/смерть): канал закрывается в фоновом потоке
      закрытия (его дожидается `stop()`), `on_error` приходит оттуда —
      ПОСЛЕ `on_client_disconnect_reason`. Поток закрытия считается потоком
      сервера: `stop()` из `on_error` там отложенный, как в колбэках
      worker-ов каналов;
  - `DispatchHandler::on_error(None, ..)` — паника потока лобби или
    фонового потока (ожидания/закрытия канала), в `DispatchServer::stop` на
    потоке, вызвавшем `stop()`, до возврата;
  - `MultiHandler::on_error(None, ..)` — паника потока-диспетчера, в
    `MultiServer::stop` (на вызвавшем потоке, до возврата).

  `message` — литерал паники (иначе общий текст; полный текст печатает
  panic hook). У самостоятельных `AutoServer`/`AutoClient` и `MultiClient`
  обработчик живёт только в worker-е — там паника по-прежнему видна только
  в panic hook.
- Раскладка разделяемой памяти (INTEROP §c: размеры, выравнивания и все
  смещения `ControlBlock`/`LayoutBlock`/`RingDescriptor`/`RingHeader`)
  проверяется `const`-утверждениями при каждой сборке, в том числе у
  потребителя; без `target_has_atomic_primitive_alignment = "32"` крейт не
  собирается.
- Значения, уходящие в `LayoutBlock` (`segment_size`, дескрипторы колец), и
  размер секции пишутся через `try_from` — переполнение даёт
  `InvalidConfig`, а не молчаливое усечение; `SharedView::initialize` и
  `RingBuffer::new` возвращают `Result` (`Corrupted` на нулевой указатель)
  вместо `expect`.
- Наборы ожидания (`NtWaitForMultipleObjects`) — `&[BorrowedHandle<'_>]`:
  время жизни дескрипторов проверяет компилятор. Владелец по-прежнему
  `win::Handle` с `NtClose` в `Drop` (`OwnedHandle` закрывал бы через
  `CloseHandle` kernel32). `Waitable: AsHandle + Sealed` с
  `#[diagnostic::on_unimplemented]`; `Beacon`/`ProcessExit` реализуют
  `AsHandle`. Публичный API `EventHandle` не расширялся: тип по-прежнему
  не именуем снаружи (`#[expect(unnameable_types)]`), его конструкторы
  остаются внутренними.
- Разбор протокола лобби (`dispatch::protocol`) — `split_first_chunk` и
  деструктуризация заголовка вместо индексов.
- Линты: rust — `raw_borrows_via_references`, `trivial_casts`,
  `let_underscore_drop`, `unnameable_types`; clippy —
  `multiple_unsafe_ops_per_block`, `cast_sign_loss`, `cast_possible_wrap`,
  `borrow_as_ptr`, `allow_attributes(_without_reason)`, `unwrap_used`,
  `expect_used` (тестам `unwrap`/`expect` разрешены в `clippy.toml`).
- Внутренний реактор `reactor` (IOCP + Wait Completion Packets, один поток
  `xsr` на процесс, ключи — индекс слота + поколение): основа async API и
  Multi на реакторе. Публичный API и протокол не меняются.
- **Multi-сервер — один поток на все слоты** (Task 17): вместо worker-а на
  каждые 21 слот (`SLOTS_PER_WORKER`, `NtWaitForMultipleObjects`, 256 слотов
  — 13 потоков) один поток-диспетчер на своём порту завершения
  (`reactor::CompletionPort`, не общий реактор). Объекты ожидания слотов
  (`CONNECT_REQ`/`DISCONNECT`/`DATA`/процесс клиента) привязаны пакетами
  ожидания с ключом (слот, вид) и меткой-поколением: завершение снятой или
  перепривязанной привязки отбрасывается и не попадает в новую сессию
  слота. Привязки одноразовые: `DATA` перепривязывается до чтения кольца
  («пусто» — по позициям с `confirm_empty`), снятая привязка, потребившая
  сигнал, возвращает его (`NtSetEvent`) или перечитывает кольцо.
  `disconnect_client`/`stop` будят диспетчер постом `NtSetIoCompletion`
  вместо события `wake` группы; дедлайны — по-прежнему таймаут ожидания
  порта; спин — один бюджет на засыпание, после удачного спина порт
  опрашивается нулевым ожиданием. Данные читаются по кругу: за круг — не
  больше `recv_batch` сообщений слота. **Колбэки всех слотов теперь
  выполняются последовательно на одном потоке: медленный `on_message`
  одного клиента задерживает всех** (раньше — только клиентов своей группы
  из 21 слота; см. документацию `MultiHandler`). Публичный API Multi и
  поведение для пира не меняются (INTEROP §i.2).
  - **Дочитывание ушедшего клиента ограничено** снимком `write_pos` в
    момент обнаружения ухода (`DISCONNECT`, смерть процесса, снятый
    claim) и физической ёмкостью кольца (`min(max_messages, capacity /
    16) + 1` кадров — не настройкой): кадры до снимка доставляются до
    колбэка отключения, дописанное после — нет. Дочитывание ведёт свою
    позицию чтения: пир, сдвинувший `read_pos` во время дочитывания
    (перемотка назад, перезапись), — `Corrupted` и разрыв с `Error`, ни
    один кадр не доставляется дважды. Раньше `drain_slot` читал,
    пока пачки выбирались целиком: враждебный клиент, сигналивший
    `DISCONNECT` (или снявший claim) и писавший дальше, держал поток
    группы (теперь — единственный поток сервера) вечно.
  - **Паника колбэка изолирована**: паника `on_message`/`on_client_connect`/
    `on_client_disconnect_reason` перехватывается (`catch_unwind`),
    `on_error(Some(slot), WorkerPanicked)`, слот отключается локально (как
    `disconnect_client`), остальные слоты обслуживаются; паника самого
    `on_error` отбрасывается (без второго `on_error` и без смены причины
    разрыва). Паника при дочитывании уходящего клиента не меняет причину
    (`Graceful`/`PeerDied`) и обычное снятие claim-а, остаток дочитывания не
    доставляется. Только при `panic = "unwind"`.
  - Неудавшаяся привязка объекта слота к порту повторяется через 100 мс
    (момент повтора — в таймауте ожидания порта), ошибка сообщается один раз
    на серию; раньше свободный слот оставался глухим навсегда.
  - Спам `CONNECT_REQ` без `HELLO` стоит серверу только CPU и никого не
    заслоняет; ограничения частоты нет.

## [0.9.0] — не выпущено (ветка `feat/beacon`)

### Зависимости (2026-10-06)

- Проверено на Windows x64: fmt, Clippy -D warnings, тесты/doctests
  с all/default/no-default features и rustdoc -D warnings.

- Toolchain закреплён на 1.99.0; thiserror обновлён до последней стабильной
  версии 2.0.21. Контракт IPC и раскладка памяти не изменились.

### Rust 1.99 (2026-10-06)

- Минимальная версия Rust — 1.99; устаревший Atomic::fetch_update заменён
  на Atomic::try_update с прежними порядками памяти и обработкой результата.

Запросы транспорта профайлера (`prof-shm`) и его аудита 2026-09-25: строго
событийная модель — ни одного пробуждения в простое, ни опроса по таймеру.

Контракт совместимости для нативных (C/C++) реализаций — что видно пиру, какие
события он обязан сигналить, чтобы Rust 0.9 не ждал вечно, объекты ядра маяка —
[`INTEROP.md`](INTEROP.md).

### Добавлено
- `set_thread_start_hook(Option<fn()>)` / `thread_start_hook()` (26.09.2026,
  запрос аудита фазы 6 профайлера) — хук старта рабочих потоков: потоки,
  которые создаёт библиотека (worker-ы Auto/Dispatch/Multi, лобби и канал
  клиента `DispatchServer`, поток ожидания канала, отложенный `Drop`),
  первым делом зовут хук, поставленный на создавшем их потоке, и наследуют
  его (потоки, порождённые рабочими, — тоже). Хук одного потока не касается
  xshm-объектов других потоков программы. Для `prof-shm`: транспортные потоки
  «тихие» для учёта памяти (`prof_core::mem::quiet_thread`). **Rust-only**:
  не пир-видимо, формат памяти и объекты ядра те же (INTEROP §f).
- `Beacon` — именованное событие-уведомление с ручным сбросом
  `<имя>_beacon` (`Local\` по умолчанию, `Global\` — как у каналов): `open`
  (создать или открыть — порядок запуска любой, `OBJ_OPENIF`), `raise`,
  `lower`, `wait(timeout)`, `wait_any(&[&Beacon], timeout)`, `is_raised`.
  Порядок сервера: `raise` строго после создания лобби, `lower` до остановки;
  клиент, не нашедший лобби при взведённом маяке, сам сбрасывает маяк и
  перепроверяет лобби (см. документацию модуля).
- `Beacon::unnamed()` — безымянное событие того же вида (`NtCreateEvent` с
  `ObjectAttributes = NULL`, `win::EventHandle::create_unnamed`): сигнал
  внутри процесса, по имени не открыть и не занять.
- `ProcessExit` — удерживаемый handle процесса (`open(pid)`, `pid`,
  `has_exited`), срабатывает при завершении процесса; запечатанный трейт
  `Waitable` и `xshm::wait_any(&[&dyn Waitable], timeout)` — ждать маяки и
  завершения процессов в одном `NtWaitForMultipleObjects`.
- `DispatchClient::lobby_exists(name)` — есть ли лобби (разовая проверка без
  подключения).
- `DispatchClientHandler::on_space_available()` и
  `DispatchHandler::on_space_available(client_id)` (по умолчанию ничего) —
  проброс события места: отправитель без потерь, получивший `QueueFull`, спит
  до него, а не в цикле со сном. `AutoHandler::on_space_available`
  документирован.
- `MultiHandler::on_client_disconnect_reason(id, reason)` и
  `MultiClientHandler::on_disconnect_reason(reason)` (по умолчанию зовут
  прежние методы; worker-ы зовут только их): `Graceful` / `PeerDied` /
  `Local` / `Error`. `MultiServer::is_client_alive(id)`.
- Мьютекс лобби Dispatch `<лобби>_lock` (новый объект ядра — Mutant,
  `OBJ_OPENIF`, NULL DACL; `win::NamedMutex`, импорты `NtCreateMutant`,
  `NtReleaseMutant`): клиент держит его всё рукопожатие с лобби.

### Изменено (breaking)
- `AutoOptions::poll_timeout`, `DispatchOptions::poll_timeout`,
  `DispatchClientOptions::poll_timeout`: `Duration` (50 мс) →
  `Option<Duration>`, по умолчанию `None` — только события. `Some(t)` —
  необязательный страховочный таймаут.
- `MultiOptions::poll_timeout`, `MultiClientOptions::poll_timeout`:
  `Duration` (50 мс) → `Option<Duration>`, по умолчанию `None`. Новое поле
  `MultiClientOptions::retry_delay` (250 мс) — пауза между попытками захвата
  слота вне подключения (раньше её роль играл `poll_timeout`, 50 мс).
- `SharedServer/SharedClient::wait_for_space`: штатное отключение пира
  теперь `Err(NotConnected)` (раньше — ожидание до таймаута вызывающего).

### Изменено
- Auto: у `AutoServer`/`AutoClient` безымянное событие `wake` (автосброс) в
  наборе ожидания worker-а; `send`/`try_send`/`stop`/`Drop` его взводят.
  Раньше worker видел новые команды только по тику `poll_timeout` (до 50 мс
  задержки отправки и 20 пробуждений в секунду в простое). Сервер вне
  подключения ждёт `CONNECT_REQ` или `wake` (`SharedServer::wait_for_client_or`,
  внутренний); отступ переподключения клиента — ожидание `wake` с таймаутом
  вместо сна по 10 мс (`wait_delay_or`).
- Auto: на событие `SPACE` worker сначала дописывает очередь в
  освободившееся место, потом зовёт `on_space_available`.
- Auto: штатное отключение пира (`DISCONNECT`) — сначала дочитать кольцо
  (как при смерти пира), потом `on_disconnect_reason(Graceful)`: последние
  сообщения пира (прощальное с причиной) больше не теряются, когда
  `DISCONNECT` и `DATA` взведены одновременно.
- Auto: при остановке worker дописывает в кольцо принятое до `stop()`
  (сколько влезет) — прощальное сообщение перед `disconnect_client`/`stop`
  доходит.
- Dispatch: лобби создаётся синхронно в `DispatchServer::start` — занятое
  имя теперь ошибка `start`, а не `Ok` и молчаливые повторы раз в 50 мс в
  фоне (второй вьюер с тем же лобби гасил маяк первого).
- Dispatch: клиент регистрируется и объявляется (`on_client_connect`) в
  worker-е своего канала при подключении — строго до его первого
  `on_message`; раньше объявление шло из отдельного потока, и первые
  сообщения приходили от неизвестного обработчику клиента (у `prof-shm` —
  терялись заголовок и определения). Сообщения, ошибки и события места
  клиента пересылаются только между `on_client_connect` и
  `on_client_disconnect_reason`.
- Dispatch: у сервера безымянное событие остановки; поток лобби ждёт
  `CONNECT_REQ` или его, чтение регистрации — `DATA` или его, поток ожидания
  подключения к каналу — «зарегистрирован» или его (раньше: `Condvar` со
  срезами `poll_timeout`). `stop()` возвращается сразу, не дожидаясь
  `channel_connect_timeout` зависшего клиента.
- Dispatch: при остановке клиентские каналы останавливаются и джойнятся вне
  лога карты клиентов, `on_client_disconnect_reason` — после join (ни один
  колбэк клиента не идёт параллельно его отключению).
- `lobby_register`: ожидание ответа без нарезки по `poll_timeout`.
- Multi-client — строго событийный (контракт — `INTEROP.md` §i). Сервер:
  worker на каждые 21 слот (предел 64 handle на ожидание; 31 слот — два
  worker-а), набор `[DISCONNECT, DATA, процесс клиента]` подключённых слотов,
  `CONNECT_REQ` свободных и безымянное `wake` группы (`disconnect_client`,
  `stop`); ожидание — до ближайшего дедлайна (протухший захват:
  `RESERVE_TIMEOUT` от первого наблюдения; разовая проверка брошенного
  рукопожатия через 2 с), а не тик 50 мс. Смерть клиента — по handle
  процесса (PID из handshake, для клиентов без него — PID владельца
  claim-а), без проверки PID раз в 3 с; клиент без наблюдаемого процесса
  проверяется только при пробуждении по другой причине. `DISCONNECT` и
  смерть — сначала дочитать кольцо, потом колбэк. Handshake слота — общий
  `complete_handshake` (PID клиента, handle процесса). `CONNECT_REQ` без
  `CLIENT_HELLO` больше не сбрасывает claim. `stop()` будит worker-ы
  событием и возвращается сразу.
- Multi-client — клиент: `[DISCONNECT, DATA, процесс сервера, wake]` без
  таймаута, `send`/`stop` будят `wake`; пауза между попытками прерывается
  остановкой; не нашедший свободного слота клиент «толкает» сервер
  (`C2S_CONNECT_REQ` занятых неподключённых слотов), чтобы тот назначил
  дедлайн протухшему захвату. `stop()` теперь сам гасит `running`.
- `wait_for_space` — одно ожидание `[SPACE, DISCONNECT, процесс пира]` на
  весь таймаут без срезов по 50 мс; anonymous (без событий) — прежний опрос
  1 мс внутри вызова.
- `complete_handshake` сбрасывает устаревший `S2C_DISCONNECT` до
  `S2C_CONNECT`; `mark_disconnected` сбрасывает `client_state` CAS
  `SERVER_READY → IDLE` (не затирает `CLIENT_HELLO` следующего клиента).
- Dispatch: имя канала наследует namespace лобби (`Global\<hex>`,
  `Local\<hex>`, каталог NT-пути) — межсессионный Dispatch через `Global\`
  работает; сервер с NT-каталогом длиннее 48 байт не стартует
  (`InvalidConfig`). Пример имени в документации модуля исправлен.
- Dispatch: клиенты лобби сериализованы мьютексом `<лобби>_lock` (брошенный
  мьютекс упавшего клиента забирается сразу); поток лобби ждёт запрос
  вместе с handle процесса клиента; `CONNECT_REQ` без `HELLO` больше не
  пересоздаёт лобби.
- `Beacon::open` отвергает существующее событие не того типа
  (`NtQueryEvent`, `STATUS_OBJECT_TYPE_MISMATCH`) — подменённый маяк с
  автосбросом не открывается; ограничивающий DACL отсекался и раньше.

### Исправлено
- Тесты маяка: имена `unique()` строились из `Instant::now().elapsed()` (≈ 0)
  и совпадали — теперь PID + счётчик.
- Multi: сообщения сверх `recv_batch` за один сигнал `DATA` ждали тика;
  теперь добор нулевым ожиданием. Сообщение, которое клиент не может
  записать (длина вне пределов), больше не застревает в голове очереди.
- Multi: сигнал `DISCONNECT`, адресованный клиенту при `disconnect_client`
  и поглощённый спящим worker-ом сервера, возвращается клиенту.
- Лобби Dispatch: два одновременных клиента могли оба пройти рукопожатие и
  прочитать чужой ответ (с 0.8; маяк делал это типичным).
- `tests::server_client_roundtrip`: подключение с повтором вместо
  фиксированной паузы 50 мс (падал под нагрузкой полного прогона).

### Тесты
- `beacon`: безымянные маяки независимы и ждутся вместе с именованным;
  `ProcessExit` живого процесса не срабатывает, завершившегося — будит
  `wait_any`; уникальность имён.
- `dispatch`: второй сервер на занятом лобби — ошибка, имя освобождается
  после остановки; `on_message` никогда не раньше `on_client_connect`
  (медленный `on_client_connect`, пачка сообщений сразу после подъёма
  канала, 8 клиентов); `stop()` быстр при зависшей регистрации
  (`channel_connect_timeout` 30 с); прощальное сообщение доходит до
  `on_disconnect` (5 раундов); отправитель без потерь на событиях места
  (очередь 8, 3000 × 30 КиБ, ожидание только `on_space_available`).
- `tests/backpressure.rs`: Dispatch-тесты без `poll_timeout` (по событиям);
  `wait_for_space` будится штатным отключением пира в обе стороны.
- «Ноль пробуждений в простое» — счётчики пробуждений worker-ов
  (`cfg(test)`): `auto::tests::idle_worker_does_not_wake_up`,
  `dispatch::tests::idle_dispatch_does_not_wake_up` (лобби + каналы обеих
  сторон), `multi::tests::idle_server_and_client_do_not_wake_up` — 0 за 1 с.
- `multi`: протухший захват снимается ровно на дедлайне (2–3 пробуждения:
  толчок + дедлайн), клиент получает слот после дедлайна сам; быстрый
  `stop()` (два worker-а) и Drop клиента в паузе; добор сверх `recv_batch`;
  `disconnect_client` доходит до клиента; 24 клиента на двух worker-ах;
  `tests/peer_death.rs`: смерть клиента и сервера Multi (граница 500 мс).
- `ring`: `wait_for_space` — ровно одно ожидание в ядре до места / до
  смерти пира, отключение и устаревший `DISCONNECT`.
- `dispatch`: 16 одновременных регистраций (мьютекс лобби, ответы не
  перепутаны), канал в namespace лобби; `win`: мьютекс и брошенный
  мьютекс, мьютекс/маяк на имени чужого типа; `beacon`: подменённый маяк.

### Ревизия 2 (25.09.2026, повторный аудит)

Пир-видимые изменения — правила R15–R17 и журнал в `INTEROP.md` («Изменения
после ревизии 2»); формат памяти, имена и типы объектов ядра не менялись.

#### Исправлено
- Клиент канала застревал, если сервер отключал его сразу после
  рукопожатия (`disconnect_client`/`stop`/Drop из `on_client_connect`):
  `SharedClient::connect` после `S2C_CONNECT` требовал `server_state ==
  SERVER_READY`, получал `HandshakeFailed` и откатывался; `DISCONNECT`
  оставался ничьим, `AutoClient` переподключался к исчезнувшему каналу раз в
  250 мс вечно, `on_disconnect` не приходил, `is_connected()` оставался
  `true`, прощальное сообщение (`BYE` отказа prof-shm) терялось. Теперь
  рукопожатие засчитывается по смене `generation` (запоминается до
  `CLIENT_HELLO`), даже если сервер уже `IDLE`: worker дочитывает кольцо и
  видит `DISCONNECT`. Флакующий
  `dispatch::tests::farewell_message_is_delivered_before_disconnect` —
  отсюда.
- Dispatch: выделенный канал больше не переподключается и при неудачном
  ПЕРВОМ подключении (`AutoClient::connect_dedicated`, внутренний): `on_error`
  + `on_disconnect_reason(Error)`, клиент завершён.
- Multi: `disconnect_client` освобождал claim раньше, чем клиент получал
  `DISCONNECT`; следующий клиент захватывал слот, его `complete_handshake`
  сбрасывал `DISCONNECT` прежнего — два клиента на одном SPSC-кольце.
  Теперь claim снимает сам клиент (по `DISCONNECT`), мёртвого — дедлайн
  `RESERVE_TIMEOUT`; штатный уход клиента сервер отрабатывает CAS claim-а
  сессии (а не store — не отнимает слот у нового захвата); клиент на каждом
  пробуждении сверяет `generation` (чужая сессия — уйти, вернув `DATA`).
- Устаревший `S2C_CONNECT`: клиент, откатившийся по таймауту ровно в момент
  ответа сервера, оставлял сервер «подключённым» и взведённый `S2C_CONNECT`
  следующему клиенту. Рукопожатие фиксируется CAS-ами `client_state`
  (сервер `CLIENT_HELLO → SERVER_READY`, откат клиента `CLIENT_HELLO →
  IDLE`), клиент сбрасывает `S2C_CONNECT` до заявки; Drop клиента —
  CAS `SERVER_READY → IDLE` без записи `reserved[3]`.
- `AutoClient`/`DispatchClient`/`MultiClient`: `stop()`/Drop во время
  ожидания `S2C_CONNECT` ждали `connect_timeout` (2 с) / `channel_timeout`
  (10 с) / `slot_timeout` (≤ 8 с). Ожидание рукопожатия теперь прерывает
  `wake` worker-а (`SharedClient::connect_interruptible`, внутренний;
  `send` его не прерывает), заявка при этом отзывается.
- `MultiServer::stop()` сигналит `DISCONNECT` подключённым клиентам сразу
  (раньше — только когда отпустят последний `Arc` сервера).
- `DispatchServer::stop()` из колбэка обработчика (или потока лобби) —
  взаимная блокировка (поток лобби join-ил worker канала, ждущий в
  `stop()`). Теперь такой вызов распознаётся (метка потока) и остановка
  отложенная: без join, каналы закроет поток лобби после возврата колбэка;
  дождаться — повторным `stop()`/Drop из чужого потока.
- Multi: осиротевший слот (клиент ушёл штатно и снял claim раньше, чем
  worker разобрал его `DISCONNECT`) — кольцо дочитывается до колбэка.

#### Изменено
- `DispatchClient::server_pid()` — PID сервера из рукопожатия канала;
  сохраняется и после разрыва (раньше — только пока канал поднят): после
  отказа есть чей выход ждать.
- Комментарий `wait_any` (`win.rs`): предел — `MAXIMUM_WAIT_OBJECTS = 64`
  (worker Multi), а не «62».

#### Тесты
- `dispatch`: `refusal_in_on_client_connect_delivers_farewell_and_disconnect`
  (10 раундов: прощание, `on_disconnect`, `is_connected() == false`,
  `server_pid` после разрыва), `dedicated_channel_failure_ends_the_client`,
  `stop_from_handler_callback_does_not_deadlock`; прощальный тест — 30
  прогонов подряд под нагрузкой полного прогона, 30/30.
- `server::handshake_tests`: отозванная заявка не фиксируется и не
  сигналит `S2C_CONNECT`; устаревший `S2C_CONNECT` не подтверждает
  следующего клиента; клиент засчитывает рукопожатие ушедшего сервера и
  дочитывает прощание. `client::tests::withdraw_hello_loses_only_to_a_committed_handshake`.
- `multi`: `disconnect_client_keeps_claim_until_the_client_leaves` (20
  раундов гонки A/B), `claim_of_a_kicked_client_that_never_leaves_is_released_at_deadline`,
  `stop_disconnects_clients_while_the_server_arc_is_alive`,
  `drop_during_slot_handshake_is_prompt`; `auto::lossless_tests::stop_during_connect_is_prompt`
  (заявка отозвана, `send` рукопожатие не прерывает; лучшая из трёх
  попыток < 200 мс, каждая < 3 с при `connect_timeout` 30 с — единичный
  всплеск планировщика под нагрузкой полного прогона не валит тест).
- `auto::tests::drop_from_own_worker_callback_does_not_self_join_deadlock`
  ждёт фактического подключения вместо паузы 200 мс: `stop()` во время
  рукопожатия теперь отзывает заявку, и под нагрузкой тест падал.
- Прогон: 5 полных `cargo test --all-features` подряд — зелёные (116 unit
  + 1 ignored, 7 backpressure, 5 multi, 4 ordering, 13 peer_death, 3 stress).

### Не сделано / оставлено намеренно
- Переподключение `AutoClient` и повтор захвата `MultiClient` к
  отсутствующему серверу (или без свободного слота) остаются периодическими
  (`reconnect_delay` / `retry_delay`) — только вне подключения; событийная
  замена — `Beacon` (как в prof-shm), но старый сервер его не взводит, а
  без отката на таймер клиент 0.9 спал бы вечно. В Dispatch канал не
  переподключается.
- `LOBBY_RETRY_DELAY` (250 мс) — только после ошибки пересоздания лобби.
- Anonymous-сервер (без объектов ядра): `wait_for_client_noevent` и
  `wait_for_space` опрашивают раз в 1 мс — внутри вызова с таймаутом
  вызывающего, не в простое.

## Надёжная детекция смерти пира - 2026-09-24 (в рабочей копии, ветка `feat/try-write`, 0.8.0)

Продолжение запроса `prof-shm`: пир (viewer или продюсер), убитый или упавший,
раньше не сигналил ничего -- lossless-писатель мог ждать места вечно, а
`is_process_alive` по PID не отличала «процесс удалён» от «нет прав».

### Протокол (без изменения layout и `SHARED_VERSION`)
- `ControlBlock.reserved[2]` (`RESERVED_SERVER_PID_INDEX`) -- PID сервера, пишется в `SharedServer::start*`
- `ControlBlock.reserved[3]` (`RESERVED_CLIENT_PID_INDEX`) -- PID клиента, пишется ДО `CLIENT_HELLO` (Release через `client_state`); сервер забирает `swap(0)` в `complete_handshake`, откат handshake клиента обнуляет -- клиент старой версии не унаследует чужой PID
- Каждая сторона открывает по PID **удерживаемый handle процесса** пира (`win::ProcessWatch`, `SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION`) и закрывает его при отключении/Drop (RAII). Процесс, уже мёртвый на момент handshake, не наблюдается (устаревший PID)
- Пир 0.7.0 / раннего 0.8.0 без PID: наблюдения нет, всё как раньше (тесты совместимости моделируют обе стороны)

### Добавлено
- `DisconnectReason { Graceful, PeerDied, Local, Error }` (`#[non_exhaustive]`), `ShmError::PeerDied`
- `AutoHandler::on_disconnect_reason`, `DispatchHandler::on_client_disconnect_reason`, `DispatchClientHandler::on_disconnect_reason` -- методы по умолчанию, вызывают старые `on_disconnect`/`on_client_disconnect`; библиотека зовёт только их
- `SharedServer`/`SharedClient::{peer_pid, is_peer_alive}`; `AutoServer`/`AutoClient::{peer_pid, is_peer_alive}`; `DispatchServer::is_client_alive`; `DispatchClient::{is_peer_alive, server_pid, disconnect_reason}`
- Handle процесса пира -- в наборе ожидания auto-worker-а (4-й, после DISCONNECT/DATA/SPACE: штатный DISCONNECT и последние данные имеют приоритет); перед `PeerDied` кольцо дочитывается
- `poll_client`/`poll_server`/`wait_for_space` возвращают `Err(PeerDied)` сразу (handle процесса в том же `NtWaitForMultipleObjects`); пока в кольце есть данные мёртвого пира, `poll_*` отвечает `Ok(true)`

### Изменено
- `win::is_process_alive` переписана поверх `ProcessWatch` (семантика прежняя, консервативная); Multi-сервер сперва проверяет удерживаемый handle слота на каждой итерации и только для клиентов без PID откатывается на троттлинг-проверку по PID
- `SharedClient::mark_disconnected` больше не `const fn` (дропает handle пира)
- `ProcessWatch` помечен `Send + Sync` (как `EventHandle`), чтобы `SharedServer`/`SharedClient` не потеряли прежний `Sync`

### Замеры
`tests/peer_death.rs`, `TerminateProcess` дочернего процесса-пира, 6 прогонов: от убийства до колбэка/ошибки **0,9–4,3 мс** во всех режимах (Shared `wait_for_space`/`poll_server`, Auto обе стороны, Dispatch обе стороны); граница теста -- 500 мс.

### Проверки
fmt, clippy `-D warnings`, `cargo doc -D warnings` -- чисто; `cargo test -- --test-threads=1`: 73 unit + 6 backpressure + 11 peer_death + 5 multi + 4 ordering + 3 stress -- зелёные. Тест `wait_for_space_is_woken_by_reader_event` сделан устойчивым к нагрузке (момент пробуждения фиксируется внутри писателя, лучший из трёх замеров).

## Запись без перезаписи: `try_send` / `free_space` / `wait_for_space` - 2026-09-24 (в рабочей копии, ветка `feat/try-write`)

Запрос транспорта профайлера (`prof-shm`, SPEC §7 и решение №2 §13): протокол
без потерь «по построению» с backpressure; окно подтверждений остаётся только
страховкой.

### Добавлено
- **`RingBuffer::try_write_message`** -- запись, которая никогда не трогает непрочитанные данные: либо кадр целиком ложится в свободное место, либо `Err(QueueFull)` и кольцо не меняется (`overwritten` всегда 0, `drop_count` не растёт). Проверка консервативна: ложных успехов нет
- **`FreeSpace { bytes, messages }`** (+ `fits(len)`, `max_payload()`, `ZERO`) -- свободное место кольца; из потока писателя это нижняя граница (`fits(n)` ⇒ следующий `try_send*` длины `n` пройдёт)
- **Single-client**: `SharedServer::{try_send_to_client, free_space, wait_for_space}`, `SharedClient::{try_send_to_server, free_space, wait_for_space}`
- **Auto**: `AutoServer::{try_send, free_space}`, `AutoClient::{try_send, free_space}` -- lossless-сообщения пишутся в кольцо только через `try_write`, из очереди не вытесняются, `Err(QueueFull)` при `max_send_queue` непереданных; `free_space()` -- консервативная оценка (снимок кольца минус принятое, но не записанное)
- **Dispatch**: `DispatchClient::{try_send, free_space}`, `DispatchServer::{try_send_to, free_space(client_id)}`
- **Multi**: `MultiServer::{try_send_to, free_space(client_id)}` (у `MultiClient` -- пока нет, см. `Docs/NOTES.md`)
- **Пробуждение «место появилось»**: заявка писателя в `RingHeader::space_waiter` (размер ждущего кадра); читатель после `receive_*` снимает её CAS-ом, только когда места реально хватает, и сигналит существующее событие `SPACE`. Протокол Dekker на двух `fence(SeqCst)` -- потерянного пробуждения нет; ожидание нарезано срезами по 50 мс (страховка для старых пиров и anonymous-режима)
- Реэкспорт лимитов кольца: `RING_CAPACITY`, `MAX_MESSAGES`, `MAX_MESSAGE_SIZE`, `MIN_MESSAGE_SIZE`, `MESSAGE_HEADER_SIZE` (раньше README их перечислял, но импортировать было нельзя)

### Wire-формат
- `RingHeader.reserved[0]` стал `space_waiter: AtomicU32`; размер (64 Б), выравнивание и смещения остальных полей не изменились (тест `ring_header_offsets_are_stable`). Поле всегда было нулём и никем не читалось, поэтому **`SHARED_VERSION` не менялся**: пиры 0.7.0 совместимы в обе стороны. Со старым читателем писатель просыпается по прежнему сигналу «кольцо опустело» или по 50-мс срезу

### Изменено
- `write_message` и `try_write_message` делят одну проверку длины (`frame_len`) и одну публикацию кадра (`commit`); проверка «кадр больше кольца» заменена const-assert-ом (`4 + 65535 <= 2 МиБ`)
- `receive_from_client`/`receive_from_server` дополнительно выполняют `fence(SeqCst)` + чтение заявки: +~5 нс на сообщение (замер ниже)
- Auto: переполнение очереди обычным `send` вытесняет самое старое **обычное** сообщение; если очередь целиком из lossless -- выбрасывается само новое сообщение `send` (оба случая -- `on_overflow`)

### Исправлено
- Auto: сообщение `send` недопустимой длины возвращалось в голову очереди и **навсегда блокировало канал**; теперь оно выбрасывается с `on_error`

### Проверки
`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `RUSTDOCFLAGS=-D warnings cargo doc --no-deps` -- чисто; `cargo test -- --test-threads=1` -- 67 unit (+1 ignored бенчмарк) + 6 backpressure + 5 multi + 4 ordering + 3 stress, все зелёные. Новые тесты: модельный рандомизированный тест кадрирования (60 000 шагов, ≥10 оборотов позиций), писатель быстрее читателя (потоки и **два процесса**), пробуждение `wait_for_space` событием (~0,2 мс), политика очереди auto, lossless через `DispatchClient`/`DispatchServer`. Ring-тесты проходят под **miri** (кроме seqlock-теста overwrite-пути, помечен `ignore` под miri).

Замер (`cargo test --release --lib -- --ignored --nocapture bench_`, запись+чтение в одном потоке): 16 Б -- `write` 27,9 нс / `try_write` 28,6 нс; 256 Б -- 41,9 / 41,2 нс; 4 КиБ -- 157 / 163 нс; `try_write` на полном кольце (`QueueFull`) -- 3 нс; проверка заявки на стороне читателя -- +4…6 нс.

## Тулчейн и зависимости - 2026-08-22 (в рабочей копии)

### Изменено
- MSRV поднят **1.97 -> 1.98**: `rust-version = "1.98"` в `Cargo.toml`, синхронизировано в `README*.md`, `CLAUDE.md`, `Docs/CURRENT_STATE.md`
- Зависимости обновлены до последних совместимых: `thiserror` 2.0.18 -> 2.0.20, транзитивные `proc-macro2` 1.0.106 -> 1.0.107, `quote` 1.0.46 -> 1.0.47, `syn` 2.0.118 -> 3.0.3 (мажор, тянется через `thiserror-impl`)

### Обоснование по фичам Rust 1.98
Провёл ревизию стабилизаций 1.98 на применимость к крейту -- правок кода не потребовалось:
- Новые атомик-хелперы (`Atomic::from_mut`/`from_mut_slice`/`get_mut_slice`) конвертируют `&mut T` в `&Atomic<T>`; наша модель -- raw-указатели в shared-маппинг (`SharedView`/`RingBuffer`), а не `&mut`, поэтому неприменимы
- Новый FFI-линт `c_void_returns` (warn-by-default): чист -- все NT-функции возвращают `NTSTATUS`/`i32`, ни одна не возвращает `c_void`
- Линты `invalid/suspicious_runtime_symbol_definitions`: неактуальны (крейт не определяет runtime-символы)
- Algebraic floats, `NonZero::from_str_radix`, `format_into`/`NumBuffer`, `str::substr_range`/`strip_circumfix` -- нет соответствующих сценариев (нет float-математики, парсинг имён -- холодный путь)

### Проверки
`cargo build`/`--release` (LTO, panic=abort) чисто, `cargo clippy --all-targets` без предупреждений, все тесты зелёные (5 multi + 4 ordering + 3 stress + unit)

## [0.7.0] - 2026-07-28 (в рабочей копии)

### Удалено (BREAKING -- полностью убран C/C++ API)
- `src/ffi.rs`, `src/multi/ffi.rs`, `src/dispatch/ffi.rs` -- весь слой `extern "C"` (~2200 строк)
- `cbindgen.toml`, `include/xshm.h`, `include/xshm_server.h`, `include/xshm_client.h` -- генерация и ручные C-заголовки
- `build_all.bat` и каталог `lib/` -- сборка `.lib`/`.a` для MSVC x86/x64 и MinGW
- crate-type `staticlib` (остался только `rlib`) и build-зависимость `cbindgen`

### Изменено
- `build.rs` сведён к одной строке: `cargo:rustc-link-lib=ntdll`
- Doc-комментарии про `user_data`/C-вызывающий код переформулированы в терминах Rust-handler'ов
- Документация (`README*.md`, `Docs/API.md`, `Docs/CURRENT_STATE.md`, `Docs/PROJECT_MAP.md`, `Docs/PROJECT_OVERVIEW.md`, `Docs/CONVENTIONS.md`, `Docs/modules/`) очищена от C API

### Не затронуто
- Формат shared memory, magic/version, handshake, claim- и lobby-протоколы -- wire-совместимость с 0.6.0 сохранена
- `EventHandles` (raw `isize` для kernel-драйвера) и anonymous sections
- Rust API всех режимов: Single-client, Auto, Multi-client, Dispatch

### Обоснование
Нативные потребители переходят на отдельный синхронный проект на C23; xshm остаётся чистым Rust-крейтом. См. `Docs/DECISIONS.md` #21.

### Исправлено (аудит 2026-07-28)
- **`naming.rs`**: явный namespace в имени канала (`Global\`, `Local\`, готовый NT-путь) больше не оборачивается в `Local\` -- межсессионный IPC через `Global\` был неработоспособен (`DECISIONS.md` #22)
- **`win.rs`**: `Mapping::open` проверяет, что отображённый view не меньше `shared_mapping_size()` -- чужая секция меньшего размера уводила доступ к кольцам за пределы отображения
- **`dispatch/mod.rs`**: `DispatchClient` больше не переподключается вечно к снесённому каналу (это противоречило его же документации), `is_connected()` честно становится `false`, `on_connect` вызывается по факту подключения канала, а не из `connect()`
- **`multi/mod.rs`**: `MultiClient::is_connected()` сбрасывается при любом выходе worker-а (раньше -- только по disconnect-событию, поэтому после `stop()` оставался `true`)
- **`auto/mod.rs`**: переполнение send-очереди сообщается через `AutoHandler::on_overflow` (раньше сообщения терялись молча)
- **`auto/`, `multi/`**: убраны 4 `expect()`-паники в worker-потоках -- вместо паники ошибка уходит в `on_error`
- **`dispatch/protocol.rs`**: усечение длинных имён по границе UTF-8, а не по байту

### Удалено (чистка)
- Мёртвый код: `RingBuffer::{capacity,reset,drop_count}`, `NtResetEvent`, `nt_success`, `is_timeout`, `NOTIFICATION_EVENT`, `WAIT_ALL`, blanket `#![allow(dead_code)]` в `ntapi/`
- Неиспользуемые поля `_name` в `SharedServer`/`SharedClient`/`Mapping`/`EventHandle` (лишняя аллокация на каждый объект)
- Тавтологичный тест `test_architecture_supported` (архитектура проверяется `compile_error!` в `win.rs`)
- MinGW-таргеты из `.cargo/config.toml`

### Изменено (качество)
- `Mutex` убран из send-очереди auto-режима: очередь видна только своему worker-потоку
- Дублирующийся хвост handshake вынесен в `SharedServer::complete_handshake`, откат клиента -- в `client::rollback_handshake`
- Имена потоков во всех режимах -- только в debug-сборке (в release не светят ни библиотеку, ни имя канала)

### Модернизация под современный Rust (edition 2024)
- Крейт переведён на **edition 2024**, `rust-version = "1.97"`; из кода это потребовало ровно одной правки -- `unsafe extern "system"` для импортов ntdll
- Политика линтов вынесена в `[lints.rust]`/`[lints.clippy]` манифеста; `undocumented_unsafe_blocks = "deny"` -- правило «каждый unsafe с `// SAFETY:`» теперь проверяет компилятор (нашлось и закрыто 48 мест без обоснования)
- `SharedView` отдаёт безопасные `header_a()`/`header_b()`/`headers()`: unsafe сконцентрирован в одном типе, из `server.rs`/`client.rs`/`multi/` ушли 10+ unsafe-блоков
- Strict provenance: `handle.addr()` вместо `as isize`, `ptr::without_provenance_mut(pid)` вместо `pid as usize as HANDLE`
- `ring.rs::read_message`: запись в `spare_capacity_mut()` + `MaybeUninit`, `set_len` -- после (раньше `set_len` шёл до записи, что создавало `&mut [u8]` на неинициализированную память)
- if-let цепочки, `let...else`, `.cast()`, `size_of` из прелюдии; `#[expect(..., reason)]` вместо `#[allow]` (сразу выявил устаревшее подавление в `ntapi/funcs.rs`)
- `Debug` на 16 публичных типах и `#[must_use]` на 12 геттерах (Rust API Guidelines)

### Проверено
`cargo build`/`--release` чисто, `cargo clippy --all-targets` под строгой политикой линтов -- ноль предупреждений, `cargo fmt --check` чисто, `cargo test -- --test-threads=1` -- **64/64 зелёные** (10 новых регрессионных тестов, 1 тавтологичный удалён).

## [0.6.0] - 2026-07-10 (в рабочей копии)

### Изменено (BREAKING -- меняется layout публичных `repr(C)` структур)
- `shm_endpoint_config_t`: удалено мёртвое поле `buffer_bytes` (никогда не читалось)
- `shm_auto_options_t.wait_timeout_ms` -> `poll_timeout_ms` (унификация с `shm_multi_options_t`/`shm_dispatch_options_t`, которые уже использовали `poll_timeout_ms`); `AutoOptions.wait_timeout` -> `poll_timeout` в Rust API
- `shm_multi_client_options_t`: удалено мёртвое поле `lobby_timeout_ms` (не используется после перехода на claim-протокол в 0.4.0; имя коллидировало с живым `lobby_timeout_ms` в `dispatch/`, где оно означает совсем другое); `MultiClientOptions.lobby_timeout` удалено в Rust API
- `DispatchServer::client_channel()` -> `channel_name()` (унификация с `MultiServer::channel_name()`)

### Добавлено
- `shm_multi_client_options_t.max_send_queue` / `MultiClientOptions.max_send_queue` (по умолчанию 256) -- раньше внутренняя send-очередь `MultiClient` не имела предела и могла расти неограниченно при зависшем/медленном пире
- `shm_multi_client_callbacks_t.on_overflow` / `MultiClientHandler::on_overflow` (default no-op, не breaking для существующих Rust-реализаций) -- репортит переполнение send-очереди, симметрично `AutoHandler::on_overflow`

### Изменено (BREAKING -- меняются видимые имена NT-объектов)
- `naming.rs`: убран автопрефикс `XSHM_`/`XSHM_SEG_` из имён Section/Event -- `mapping_name(base)` теперь `Local\{base}` (было `Local\XSHM_SEG_{base}`), `event_name(base, ...)` теперь `Local\{base}_...` (было `Local\XSHM_{base}_...`). Пространство имён `Local\` полностью контролирует вызывающая сторона. См. `Docs/DECISIONS.md` #20

## [0.5.0] - 2026-07-10 (в рабочей копии)

### Добавлено
- Регрессионные тесты для всех находок аудита ниже (multi/, dispatch/, auto/, ffi.rs, win.rs, ntapi/helpers.rs)

### Исправлено
- **multi/**: orphan-слот с мёртвым владельцем не обнаруживался -- добавлена периодическая liveness-проверка процесса-владельца по PID (`is_process_alive`, `win.rs`)
- **multi/**: `stop()` не джойнил worker-поток синхронно -- возможен UAF при немедленном освобождении ресурсов вызывающим
- **multi/**: unconditional store при освобождении протухшего claim мог затереть легитимный новый claim -- заменён на CAS-guarded `handle_orphaned_slot_disconnect`
- **multi/**: клиентский `slot_timeout` мог превышать серверный `RESERVE_TIMEOUT` -- добавлен `clamp_slot_timeout` с защитным запасом
- **multi/**: `next_claim_token()` использовал предсказуемую схему `pid<<8^n` -- заменён на `RandomState` (OS-энтропия)
- **dispatch/**: self-join deadlock при синхронном отключении клиента изнутри его же callback'а (`AutoProxyHandler::on_disconnect`)
- **dispatch/**: `stop()` не джойнил worker-поток и pending-подключения -- потенциальный UAF через FFI
- **dispatch/**: `shm_dispatch_server_stop` вообще не вызывал `.stop()` перед освобождением -- перманентная утечка worker-потока
- **dispatch/**: обработка одного клиента в лобби блокировала регистрацию остальных до 30с -- финализация подключения вынесена в отдельный поток (`pending_connects`)
- **dispatch/**: `generate_channel_name()` использовал предсказуемую схему (время+счётчик) при заявленном в комментарии "cryptographic quality" -- заменён на `RandomState`
- **dispatch/ffi**: усечение C-строк через `.unwrap_or_default()` теряло данные при embedded NUL -- заменено на усечение по первому NUL
- **auto/**: тот же self-join deadlock, что и в dispatch/, независимо воспроизведён и исправлен в самом публичном `auto/mod.rs` (`join_unless_self`)
- **ffi.rs**: aliasing UB -- `shm_server_receive`/`shm_client_receive` брали `&mut ServerState`/`&mut ClientState`, конфликтуя с `&`-версией у send при конкурентном вызове с разных потоков на одном handle; заменено на `Mutex<RecvCache>`
- **ffi.rs**: потеря сообщения при слишком малом буфере вызывающего -- сообщение уже вычитывалось из ring buffer до проверки размера; теперь кэшируется до следующего вызова с достаточным буфером
- **tests/stress.rs**: assert не допускал легитимный forward-гэп при overwrite-on-full -- заменён на проверку монотонности

### Изменено
- `Docs/*.md` синхронизированы с фактическим состоянием кода (версия, dispatch/, claim-протокол вместо lobby, MAX_MESSAGES=500, InvalidConfig)

## [0.4.0] - 2026-06-05

### Добавлено
- `DispatchServer`/`DispatchClient` (`dispatch/`): единое лобби + динамический `AutoServer`-канал на клиента, бинарный протокол регистрации v2
- Редизайн multi-client: центральный lobby-сегмент убран, клиенты конкурентно захватывают слоты через CAS (`RESERVED_CLAIM_INDEX`)
- `ShmError::InvalidConfig` -- валидация конфигурации (например, `max_clients` вне диапазона)
- `NtOpenProcess` + `CLIENT_ID` (`ntapi/`) -- задел под liveness-проверку процессов

### Исправлено
- **H1**: torn-read в `read_message` при overwrite в byte-full режиме -- seqlock-copy с CAS-подтверждением `read_pos`
- **M1**: `Local\` резолвился в глобальный `\BaseNamedObjects` вместо session-scoped пространства имён -- честное различение `Local\`/`Global\` через `NtQueryInformationProcess`
- **M2**: race condition при одновременном подключении к lobby -- атомарная резервация слота + reclaim протухших + cap `max_clients<=31`
- **M3**: client-side retry `CLIENT_HELLO` в пределах `lobby_timeout`
- **L1**: `more_pending` дренирует остаток батча без блокировки

### Изменено
- `thiserror` 1.0 -> 2
- `build.rs` rerun на всех ffi-источниках; NT-внутренности исключены из публичного заголовка

## [0.3.0] - 2025

### Добавлено
- Multi-client режим: `MultiServer` и `MultiClient` с lobby + слоты
- Автоматическое назначение слотов через lobby handshake
- C FFI для multi-client: `shm_multi_server_*`, `shm_multi_client_*`
- `DEFAULT_MAX_CLIENTS = 20`
- Статическая CRT линковка для всех целевых платформ
- MIT лицензия

### Изменено
- Убрана зависимость от SSN, переход на прямую линковку ntdll.dll

## [0.2.0]

### Добавлено
- Auto-mode: `AutoServer`, `AutoClient` с background worker потоками
- Автоматический reconnect с настраиваемой задержкой
- Send-очередь с лимитом
- Статистика: sent/recv messages, overflows
- C FFI для auto-mode

## [0.1.0]

### Добавлено
- Начальная реализация: SharedServer, SharedClient
- Lock-free SPSC кольцевой буфер (2 МБ)
- NT API обертки (Handle, EventHandle, Mapping)
- Handshake протокол (CLIENT_HELLO → SERVER_READY)
- Anonymous секции для kernel driver
- Event handles API
- C FFI для single-client режима
- Интеграционные и стресс-тесты

---

## Инициализация документации

**2026-02-06** -- Создана структура Docs/ с полной документацией проекта на основе анализа кодовой базы.
