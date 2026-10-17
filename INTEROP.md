# Контракт совместимости xshm для нативных реализаций — версия 0.10.0

> **0.10.0 (16.10.2026): новый формат разделяемой памяти `0x00020000`** —
> размеры колец и лимиты сообщений по каждому направлению записаны в сегменте
> (`LayoutBlock`), кадр с `u32`-длиной и выравниванием 8, payload кадра
> непрерывен (PAD-кадр до границы кольца, `max_message_size ≤ capacity / 2 −
> 8`), `magic` публикуется последним, события канала открываются с проверкой
> типа.
> С пирами 0.9 несовместим (отказ по версии). Нормативно — §c; дельта для
> C/C++ — раздел [«Изменения 0.10»](#изменения-010-16102026) в конце.
> Остальные разделы (объекты ядра, рукопожатие, событийная модель, лобби,
> Multi) действуют как прежде.

Документ для авторов **независимой** реализации протокола xshm (синхронный
проект на C23/C++), совместимой с этим крейтом по объектам ядра и разделяемой
памяти. Он **нормативный**: слова «ОБЯЗАН», «ДОЛЖЕН», «ЖЕЛАТЕЛЬНО» означают
требование, рекомендацию и пожелание. Всё сверено с кодом ветки
`feat/layout-v2` (xshm 0.10.0, формат `0x00020000`); код указан именами
функций и типов (`SharedClient::connect_impl`, `src/client.rs`), без номеров
строк.

Если текст расходится с кодом — прав код, а расхождение — ошибка документа.

**Ревизия 2 (25.09.2026).** Повторный аудит изменил пир-видимые правила
рукопожатия (CAS-фиксация, смена `generation` как признак принятия, сброс
`S2C_CONNECT` до заявки) и владения claim-ом Multi. Дельта для C/C++ —
раздел [«Изменения после ревизии 2»](#изменения-после-ревизии-2-25092026)
в конце документа; основной текст ниже уже приведён к ней.

**Ревизия 3 (17.10.2026, Task 22).** Исключительная заявка single-client
канала: клиент захватывает `slot_claim` токеном (как клиент Multi),
`CLIENT_HELLO` ставится только CAS-ом из `IDLE`, принятие — по фиксации
своей заявки, уход — только при неизменном `generation` сессии с
освобождением claim последним; сервер сбрасывает `S2C_DISCONNECT` до
фиксации, снимает заявку умершего, потребляет самоописание клиента только
застав владельца в сессии, освобождает claim умершего в сессии сразу, а
протухший — по толчку проигравшего клиента (R15, R16, R18). Формат памяти
прежний (`0x00020000`). Дельта — п. 28 раздела «Изменения 0.10»; основной
текст ниже приведён к ревизии 3.

Связанные документы: [`CHANGELOG.md`](CHANGELOG.md) (что поменялось, для
Rust-потребителей), протокол профайлера поверх Dispatch —
`useful-lib/prof-shm/PROTOCOL.md`.

---

## a. Сводка: 0.8.x → 0.9.0

Главное: **формат разделяемой памяти, имена и типы объектов ядра каналов, протокол
регистрации в лобби не изменились** (`SHARED_MAGIC`, `SHARED_VERSION`,
`DISPATCH_VERSION` прежние). Изменилось *поведение во времени*: Rust-сторона
0.9 по умолчанию больше **не опрашивает** (раньше просыпалась каждые 50 мс
и этим «лечила» пропущенные сигналы пира) — теперь каждый пропущенный сигнал
пира означает ожидание без конца. Добавлен один новый вид именованного
объекта — маяк (`<имя>_beacon`).

Колонки: **Видимость** — видно ли изменение другой реализации: `wire` (байты в
памяти/сообщениях), `ядро` (объекты ядра), `тайминг` (когда и что
сигналится/ждётся), `внутр.` (только внутри Rust-процесса). **C/C++** — нужно ли
повторять. **Старые пиры** — что будет при смешении 0.8 ↔ 0.9.

| # | Изменение (код) | Видимость | C/C++ | Совместимость со старыми пирами |
|---|---|---|---|---|
| 1 | `poll_timeout: Duration` (50 мс) → `Option<Duration>`, по умолчанию `None`: worker-ы Auto/Dispatch, поток лобби, чтение регистрации, ожидание подключения к каналу ждут **только события** (`AutoOptions::poll_timeout` в `src/auto/mod.rs`, `DispatchOptions`/`DispatchClientOptions` в `src/dispatch/mod.rs`) | тайминг | Требования к пиру — **ОБЯЗАТЕЛЬНО** (§e.4). Самой ждать без таймаута — ЖЕЛАТЕЛЬНО | Rust 0.8 сигналит всё, что нужно — ок в обе стороны. Нативный/старый пир, который не сигналит `DATA`/`SPACE`/`CONNECT_REQ`/`DISCONNECT` по правилам §e.4, **повиснет** против Rust 0.9 (раньше — работал с задержкой ≤ 50 мс) |
| 2 | Безымянное событие `wake` (автосброс) в наборе ожидания worker-а Auto; `send`/`try_send`/`stop`/`Drop` его взводят (`enqueue`, `Endpoint::stop`, `Drop for Endpoint` в `src/auto/mod.rs`) | внутр. | Не нужно (синхронной реализации нечего будить); при фоновом потоке — ЖЕЛАТЕЛЬНО | Нет влияния |
| 3 | На `SPACE` worker сначала дописывает очередь в кольцо, потом зовёт `on_space_available` (`Worker::on_space` в `src/auto/mod.rs`) | внутр. | ЖЕЛАТЕЛЬНО, если у вас есть очередь и колбэк места | Нет |
| 4 | Штатный `DISCONNECT`: **сначала дочитать кольцо**, потом `on_disconnect_reason(Graceful)` (`Worker::drain_remaining` перед `Worker::notify_disconnect` в `server_worker`/`client_worker`, `src/auto/mod.rs`) | тайминг (семантика приёма) | **ОБЯЗАТЕЛЬНО** для приёмника (иначе теряется прощальное сообщение, см. prof-shm `BYE`) | Rust 0.8 как приёмник теряет последние сообщения, если `DATA` и `DISCONNECT` взведены одновременно (NT отдаёт меньший индекс = `DISCONNECT`) |
| 5 | При остановке worker дописывает в кольцо принятое до `stop()`, сколько влезет, и только потом закрывает канал (хвост `server_worker`/`client_worker` — `Worker::flush`, `src/auto/mod.rs`) | тайминг | **ОБЯЗАТЕЛЬНО**: писать все сообщения ДО сигнала `DISCONNECT` | Старый отправитель 0.8 мог не дописать очередь |
| 6 | `DispatchServer::start` создаёт лобби **синхронно**: занятое имя → ошибка `start` (`DispatchServer::start`) | ядро (лобби существует сразу после `start`) | **ОБЯЗАТЕЛЬНО** для сервера лобби: создание без `OBJ_OPENIF`, коллизия имени — ошибка | 0.8-сервер на занятом имени возвращал `Ok` и тихо повторял раз в 50 мс |
| 7 | `on_client_connect` вызывается в worker-е канала при handshake канала, **строго до** первого `on_message` этого клиента (`AutoProxyHandler::on_connect`, `src/dispatch/mod.rs`) | внутр. (серверное поведение) | **ОБЯЗАТЕЛЬНО**, если вы реализуете Dispatch-сервер (протоколы поверх, например prof-shm, на это опираются) | Пир этого не видит |
| 8 | Сообщения/ошибки/события места клиента пересылаются обработчику только между `on_client_connect` и `on_client_disconnect_reason` (`AutoProxyHandler::is_active` и колбэки `AutoProxyHandler`, `src/dispatch/mod.rs`) | внутр. | ЖЕЛАТЕЛЬНО | Нет |
| 9 | Безымянное событие остановки сервера Dispatch (ручной сброс); `stop()` возвращается сразу (`DispatchCore::stop`, `src/dispatch/mod.rs`) | внутр. | ЖЕЛАТЕЛЬНО | Нет |
| 10 | Остановка: каналы останавливаются и джойнятся вне лога карты клиентов; `on_client_disconnect_reason(Local)` — после join (`DispatchCore::shutdown`, `src/dispatch/mod.rs`) | внутр. | ЖЕЛАТЕЛЬНО | Нет |
| 11 | `lobby_register` ждёт ответ на `S2C_DATA` без нарезки по 50 мс; чтение регистрации сервером — на `C2S_DATA` без нарезки (`lobby_register`, `DispatchCore::handle_lobby_client`, `src/dispatch/mod.rs`) | тайминг | **ОБЯЗАТЕЛЬНО**: пишущая сторона лобби сигналит `DATA` (§d, §e.4) | Пир, писавший в лобби без сигнала `DATA`, раньше обслуживался опросом; теперь — `Timeout` через `lobby_timeout`/`response_timeout` |
| 12 | Новый объект: маяк `Beacon` — именованное **NotificationEvent** `<имя>_beacon`, `OBJ_OPENIF`, NULL DACL (`Beacon::open`, `EventHandle::open_or_create_notification`) | ядро | **ОБЯЗАТЕЛЬНО**, если участвуете в обнаружении сервера (prof-shm) | 0.8 маяка не знает: клиент 0.9, спящий на маяке, **никогда** не проснётся от сервера 0.8 (тот не взводит маяк) |
| 13 | `Beacon::unnamed()` — безымянное NotificationEvent (`Beacon::unnamed`, `EventHandle::create_unnamed`) | внутр. | ЖЕЛАТЕЛЬНО (для собственных сигналов остановки — вместо предсказуемых имён) | Нет |
| 14 | `ProcessExit` + `xshm::wait_any(&[&dyn Waitable])` — handle процесса в общем ожидании (`ProcessExit`, `wait_any` в `src/beacon.rs`) | внутр. | ЖЕЛАТЕЛЬНО | Нет |
| 15 | `DispatchClient::lobby_exists(name)` = `NtOpenSection` + проверка размера (`DispatchClient::lobby_exists`) | ядро (только чтение) | ЖЕЛАТЕЛЬНО (нужно для алгоритма маяка, §b.3) | Нет |
| 16 | `DispatchHandler::on_space_available(client_id)`, `DispatchClientHandler::on_space_available()` | внутр. | ЖЕЛАТЕЛЬНО | Нет |
| 17 | Multi-client переведён на события (§i): `MultiOptions`/`MultiClientOptions::poll_timeout: Option<Duration>` = `None`, новое `MultiClientOptions::retry_delay` (250 мс, только вне подключения); worker-ы сервера по 21 слоту; смерть клиента — по handle процесса; протухший захват — по дедлайну | тайминг | Требования к пиру-клиенту Multi — **ОБЯЗАТЕЛЬНО** (§i.4, R13) | Клиент 0.8 без PID в handshake наблюдается по PID владельца claim-а (`slot_owner_pid`); без обоих — только на пробуждениях (§i.5) |
| 18 | `SharedServer/SharedClient::wait_for_space`: одно ожидание `[SPACE, DISCONNECT, процесс пира]` на весь таймаут вместо срезов по 50 мс; штатное отключение пира → `Err(NotConnected)` (§i.6) | тайминг | ЖЕЛАТЕЛЬНО (для своей реализации ожидания места) | Читатель без заявки (≤ 0.7) будит только опустошив кольцо (R2) — задержка, не зависание |
| 19 | `complete_handshake` сбрасывает `S2C_DISCONNECT` (`NtResetEvent`) **до** `S2C_CONNECT` (ревизия 3 — до фиксации, S4, п. 28 «Изменений 0.10») | ядро (состояние события) | **ОБЯЗАТЕЛЬНО** для сервера (иначе устаревший сигнал «отключает» нового клиента) | Старый сервер не сбрасывает — риск §h.3 остаётся с ним |
| 20 | `mark_disconnected`: `client_state` сбрасывается CAS `SERVER_READY → IDLE`, а не store (не затирает `CLIENT_HELLO` следующего клиента) | wire (порядок записи) | **ОБЯЗАТЕЛЬНО** для сервера лобби (иначе ломается сериализация R12) | Нет |
| 21 | Мьютекс лобби `<лобби>_lock` (Mutant, `OBJ_OPENIF`): клиент Dispatch держит его всё рукопожатие с лобби (§d.2) | ядро (новый объект) | **ОБЯЗАТЕЛЬНО** для клиента лобби (R12) | Клиент 0.8/0.9-ранний мьютекс не берёт — с ним гонка §h.2 остаётся |
| 22 | Имя канала Dispatch наследует namespace лобби: `Global\<hex>` / `Local\<hex>` / `\Dir\<hex>` / `<hex>` | wire (содержимое `channel_name`) | **ОБЯЗАТЕЛЬНО** для Dispatch-сервера; клиент открывает имя из ответа как есть (так и было) | Клиент любой версии открывает имя как есть — совместимо |
| 23 | `Beacon::open` проверяет тип существующего события (`NtQueryEvent`): не NotificationEvent → ошибка | ядро (только чтение) | ЖЕЛАТЕЛЬНО | Нет |
| 24 (рев. 2) | Рукопожатие фиксируется CAS `client_state` с обеих сторон; клиент засчитывает его по смене `generation`, сбрасывает `S2C_CONNECT` до заявки; Drop клиента — CAS `SERVER_READY → IDLE` (R15, R16) | wire (порядок записи), ядро (состояние события) | **ОБЯЗАТЕЛЬНО** | Старый клиент проверяет `server_state` после `S2C_CONNECT` — сервер, отключивший его сразу, снова даёт ему `HandshakeFailed`; старый сервер пишет `client_state` store-ом — гонка отката остаётся с ним |
| 25 (рев. 2) | Multi: `disconnect_client` не освобождает claim; claim снимает клиент по `DISCONNECT`, мёртвого — дедлайн; штатный уход — CAS claim-а сессии; `stop()` сигналит `DISCONNECT` сразу (R17) | wire (кто пишет `slot_claim`), тайминг | **ОБЯЗАТЕЛЬНО** для клиента и сервера Multi | Старый сервер освобождает claim сам — гонка двух клиентов на слоте остаётся с ним (новый клиент её замечает по `generation`) |

Таблица — история перехода 0.8 → 0.9: в 0.9 раскладка, константы формата,
события и байты сообщений лобби не менялись. В 0.10 раскладка и формат
переписаны (`src/layout.rs`, `src/constants.rs`, `src/ring.rs`, проверка
типа событий в `src/events.rs`) — действующие правила в §c и разделе
«Изменения 0.10», а не в этой таблице.

---

## b. Объекты ядра

### b.1 Имена

Базовое имя `B` (строка вызывающего) превращается в имена так
(`ObjectPath::scoped`, `has_explicit_namespace`, `scoped_name` в `src/naming.rs`; `to_nt_path` в `src/ntapi/helpers.rs`):

- «явный namespace» — `B` начинается с `Global\`, `Local\` или `\`;
- секция: явный namespace → `B`, иначе `Local\B`;
- событие канала: явный namespace → `B_<DIR>_<SUFFIX>`, иначе `Local\B_<DIR>_<SUFFIX>`;
- маяк (0.9): явный namespace → `B_beacon`, иначе `Local\B_beacon` (`beacon_name`); состояние маяка (0.10) — то же имя + `_state` (`beacon_state_name`).

Перевод в NT-путь (`to_nt_path`), который реально уходит в `UNICODE_STRING`:

| Префикс | NT-путь |
|---|---|
| `Global\X` | `\BaseNamedObjects\X` |
| `Local\X` или без префикса | сессия `S` = `NtQueryInformationProcess(ProcessSessionInformation)`; `S == 0` → `\BaseNamedObjects\X`, иначе `\Sessions\S\BaseNamedObjects\X` (0.10: ошибка запроса → отказ создания/открытия объекта, `WindowsError`; до 0.10 — как `S = 0`, т.е. молча в глобальный namespace) |
| `\...` | как есть |

`UNICODE_STRING`: `Length` = байты без терминатора, `MaximumLength` = `Length + 2`,
UTF-16. `OBJECT_ATTRIBUTES.Attributes` всегда содержит `OBJ_CASE_INSENSITIVE`
(`0x40`), `RootDirectory = NULL` (0.10: в частном пространстве имён —
handle пространства и относительное имя, §b.4).

### b.2 Полный перечень объектов

`P` = префикс событий (`Local\B_` или `B_`). **Дескриптор безопасности задаёт
создатель** объекта (0.10, `ObjectSecurity`; до 0.10 — всегда NULL DACL):
`OBJECT_ATTRIBUTES.SecurityDescriptor` при `NtCreate*` — дескриптор политики
создателя; открывающая сторона передаёт `SecurityDescriptor = NULL` (**без
изменений**) и получает доступ по DACL объекта, отказ — `STATUS_ACCESS_DENIED
(0xC0000022)` (в Rust — `ShmError::AccessDenied`). Политики Rust:

- `CurrentUser` (**по умолчанию**) — абсолютный дескриптор
  (`RtlCreateSecurityDescriptor` + `RtlSetDaclSecurityDescriptor`), DACL
  ревизии 2 (`RtlCreateAcl`) из двух `ACCESS_ALLOWED_ACE` с маской
  `GENERIC_ALL (0x10000000)`, без флагов наследования: SID пользователя токена
  процесса (`NtOpenProcessToken(TOKEN_QUERY)` +
  `NtQueryInformationToken(TokenUser)`), затем LocalSystem `S-1-5-18`
  (процесс от SYSTEM — одна запись). Владелец, группа, SACL — по умолчанию
  токена;
- `CurrentUserAllowLowIntegrity` — то же + SACL ревизии 2 с одной
  `SYSTEM_MANDATORY_LABEL_ACE` (тип `0x11`, флаги 0) для `S-1-16-4096` (Low)
  с политикой `SYSTEM_MANDATORY_LABEL_NO_WRITE_UP (0x1)` — SDDL
  `S:(ML;;NW;;;LW)`;
- `Everyone` — NULL DACL (`RtlSetDaclSecurityDescriptor(DaclPresent=TRUE,
  Dacl=NULL)`), как до 0.10;
- `Descriptor` — self-relative дескриптор вызывающего (проверен
  `RtlValidRelativeSecurityDescriptor`).

Секция и все события одного канала (и всех слотов Multi, лобби Dispatch вместе с
`B_lock` и каналами клиентов) создаются с **одной** политикой. Нативный пир,
создающий объекты, выбирает дескриптор сам; чтобы с ним работал Rust 0.10 с
политикой по умолчанию от другого пользователя, его DACL обязан выдавать этому
пользователю `SECTION_ALL_ACCESS`/`EVENT_ALL_ACCESS`/`MUTANT_ALL_ACCESS`.

| Объект | Имя | Тип | Нач. состояние | Создаёт (флаги, доступ) | Открывает | Кто сигналит | Кто ждёт |
|---|---|---|---|---|---|---|---|
| Секция канала | `Local\B` / `B` | Section, `SEC_COMMIT`, `PAGE_READWRITE`, размер `LayoutBlock.segment_size` (0.10; по умолчанию `4 194 560`) | нули | сервер: `NtCreateSection(SECTION_ALL_ACCESS=0x000F001F)`, **без** `OBJ_OPENIF` — существующее имя = ошибка (`STATUS_OBJECT_NAME_COLLISION`) (`Mapping::create`) | клиент: `NtOpenSection(SECTION_ALL_ACCESS)` + проверка типа `NtQuerySection` = `SEC_COMMIT` без `SEC_RESERVE`/`SEC_IMAGE`/`SEC_FILE`/`SEC_NOCACHE`/`SEC_WRITECOMBINE` (0.10, §c.5 п. 1) + отображение целиком + проверки §c.5 (`magic`/`version`/`LayoutBlock` против размера view; `SharedView::attach`) | — | — |
| `P S2C_DATA` | | SynchronizationEvent (автосброс, `1`) | FALSE | сервер: `NtCreateEvent(EVENT_ALL_ACCESS=0x001F0003)`, без `OBJ_OPENIF` (`EventHandle::create`) | клиент: `NtOpenEvent(EVENT_ALL_ACCESS)` + проверка типа `NtQueryEvent` = SynchronizationEvent (0.10, `EventHandle::open_auto_reset`) | писатель-сервер | читатель-клиент |
| `P C2S_DATA` | | SynchronizationEvent | FALSE | сервер | клиент | писатель-клиент | читатель-сервер |
| `P S2C_SPACE` | | SynchronizationEvent | FALSE | сервер | клиент | **читатель-клиент** | писатель-сервер |
| `P C2S_SPACE` | | SynchronizationEvent | FALSE | сервер | клиент | **читатель-сервер** | писатель-клиент |
| `P S2C_CONNECT` (connect_ack) | | SynchronizationEvent | FALSE | сервер | клиент | сервер (после handshake, только при удачном CAS фиксации — R16; ревизия 3: `S2C_DISCONNECT` сброшен **до** фиксации, S4); клиент делает `NtResetEvent` до своей заявки (R15) | клиент |
| `P C2S_CONNECT_REQ` | | SynchronizationEvent | FALSE | сервер | клиент | клиент (после `CLIENT_HELLO`); ревизия 3 — и клиент, проигравший захват claim-а, если `server_state != SERVER_READY` («толчок», R18, §i.3) | сервер |
| `P S2C_DISCONNECT` | | SynchronizationEvent | FALSE | сервер | клиент | **обе** стороны при штатном закрытии (одно событие на оба направления, несмотря на `S2C` в имени) | обе стороны |
| **Маяк (0.9)** | `Local\B_beacon` / `B_beacon` | **NotificationEvent** (ручной сброс, `0`) | FALSE у нового; у существующего не трогается | **любая** сторона: `NtCreateEvent(EVENT_ALL_ACCESS, OBJ_CASE_INSENSITIVE \| OBJ_OPENIF=0x80, NotificationEvent, InitialState=FALSE)`; успех = `STATUS_SUCCESS` или `STATUS_OBJECT_NAME_EXISTS` (`0x40000000`) (`EventHandle::open_or_create_notification`) | тем же вызовом | сервер: `NtSetEvent` (raise, 0.10 — после публикации в `_beacon_state`) / `NtResetEvent` (lower); клиент — только `clear_if_stale` (§b.3, 0.10) | клиенты |
| **Состояние маяка (0.10)** | `Local\B_beacon_state` / `B_beacon_state` | Section, `SEC_COMMIT`, `PAGE_READWRITE`, 4096 байт | нули | **любая** сторона (вместе с маяком): сначала `NtOpenSection(SECTION_ALL_ACCESS)` существующей (открытие не требует `SeCreateGlobalPrivilege`); только при `STATUS_OBJECT_NAME_NOT_FOUND` — `NtCreateSection(SECTION_ALL_ACCESS, OBJ_CASE_INSENSITIVE \| OBJ_OPENIF, MaximumSize=4096, SEC_COMMIT)`, дескриптор — политика создателя маяка (та же, что у `_beacon`; 0.10, до 0.10 — NULL DACL); успех = `STATUS_SUCCESS` или `STATUS_OBJECT_NAME_EXISTS`; у существующей — проверка `NtQuerySection` = `SEC_COMMIT` без `SEC_RESERVE`/`SEC_IMAGE`/`SEC_FILE`/`SEC_NOCACHE`/`SEC_WRITECOMBINE` (§c.5 п. 1) и view ≥ 4096, иначе ошибка (`Mapping::open`/`Mapping::open_or_create`); затем CAS `magic: 0 → 0x4E434258`. Отказ `STATUS_PRIVILEGE_NOT_HELD (0xC0000061)` или `STATUS_ACCESS_DENIED (0xC0000022)` (и только они; второй — в том числе DACL чужой секции не включает этого пользователя) — маяк **без состояния** (§b.3); любой другой отказ (другой тип объекта, `SEC_RESERVE`, `SEC_NOCACHE`, `SEC_WRITECOMBINE`, мал) — ошибка `Beacon::open` | тем же вызовом | — (пишет сервер в `raise`, раскладка §b.3) | клиенты (`clear_if_stale`) |
| **Мьютекс лобби (0.9)** | `Local\B_lock` / `B_lock` (только Dispatch) | **Mutant** | не захвачен | 0.10: **сервер** в `start` (после секции лобби, держит до остановки, не захватывает; дескриптор — политика лобби) и **клиент** лобби: `NtCreateMutant(MUTANT_ALL_ACCESS=0x001F0001, OBJ_CASE_INSENSITIVE \| OBJ_OPENIF, InitialOwner=FALSE)`, дескриптор — политика создателя (до 0.10 — NULL DACL, создавал только клиент); успех = `STATUS_SUCCESS` или `STATUS_OBJECT_NAME_EXISTS`; имя занято объектом другого типа → `STATUS_OBJECT_TYPE_MISMATCH` = ошибка регистрации (`src/win.rs`, `NamedMutex`) | тем же вызовом | — (захват `NtWaitForSingleObject`: `STATUS_SUCCESS` или `STATUS_ABANDONED_WAIT_0 = 0x80` — оба «захвачен»; освобождение `NtReleaseMutant` **тем же потоком**) | клиенты лобби друг друга; сервер мьютекс только держит открытым (0.10) |
| Безымянное событие (0.9) | нет (`ObjectAttributes = NULL`) | Notification (`Beacon::unnamed`, стоп Dispatch-сервера, «зарегистрирован») или Synchronization (`wake` Auto) | FALSE | свой процесс (`EventHandle::create_unnamed`) | не открывается по имени | свой процесс | свой процесс |
| Handle процесса пира (0.8+, не объект с именем) | — | Process | сигнален после выхода | — | `NtOpenProcess(PROCESS_QUERY_LIMITED_INFORMATION \| SYNCHRONIZE = 0x1000 \| 0x00100000, ObjectName=NULL, ClientId={pid,0})` (`ProcessWatch::open`, `ProcessWatch::open_peer`) | ядро (при завершении процесса) | сторона, открывшая handle |

Константы: `src/ntapi/types.rs`, суффиксы — `src/constants.rs`
(`DATA`, `SPACE`, `CONNECT`, `CONNECT_REQ`, `DISCONNECT`), направления —
`S2C`/`C2S` (`Direction::as_str`, `src/naming.rs`).

Порядок создания объектов сервером: секция → `ControlBlock` (PID в
`server_pid`), `LayoutBlock`, заголовки колец, `magic` последним (0.10, §c.5) → 7 событий в порядке `S2C_DATA`,
`S2C_SPACE`, `C2S_DATA`, `C2S_SPACE`, `S2C_CONNECT`, `C2S_CONNECT_REQ`,
`S2C_DISCONNECT` (`SharedEvents::create`, `src/events.rs`). Любая ошибка
создания события (в т.ч. коллизия имени) — ошибка `start`.

Ожидание: `NtWaitForMultipleObjects(n, handles, WaitAny=1, Alertable=FALSE,
timeout)`; `timeout = NULL` — без конца; иначе относительное значение в
100-нс единицах со знаком минус, ненулевая длительность короче 100 нс
округляется до `-1` (`wait_any` в `src/win.rs`, `duration_to_nt_timeout` в `src/ntapi/helpers.rs`). Результат
`STATUS_TIMEOUT (0x102)` проверяется **до** диапазона индексов. При
одновременном срабатывании NT возвращает **наименьший** индекс — на этом
построены приоритеты наборов ожидания (§e).

### b.3 Маяк (`Beacon`, новое в 0.9)

- **Открытие** (`Beacon::open`): и сервер, и клиенты
  вызывают один и тот же create-or-open (см. таблицу). Порядок запуска любой;
  событие живёт, пока открыт хоть один handle. **Первый создатель определяет
  тип события**: `OBJ_OPENIF` открывает существующее событие любого типа.
  Нативная реализация ОБЯЗАНА создавать маяк как **NotificationEvent**:
  автосбросное событие под тем же именем сломает всех (взвод разбудит одного
  ждущего; `is_raised` через ожидание с нулевым таймаутом начнёт *сбрасывать*
  маяк). **0.9:** получив `STATUS_OBJECT_NAME_EXISTS`, Rust проверяет тип
  `NtQueryEvent(h, EventBasicInformation=0, &EVENT_BASIC_INFORMATION{EventType
  u32, EventState i32}, 8, &ret)` и при `EventType != 0` (не Notification)
  закрывает handle и возвращает ошибку `STATUS_OBJECT_TYPE_MISMATCH
  (0xC0000024)`; маяк с ограничивающим DACL отсекается раньше —
  `EVENT_ALL_ACCESS` не выдаётся (`STATUS_ACCESS_DENIED`). Нативной
  реализации ЖЕЛАТЕЛЬНО делать ту же проверку.
- **`raise`** = `NtSetEvent`; **`lower`** = `NtResetEvent` (импорт добавлен в
  0.9, `src/ntapi/funcs.rs`); **`wait(t)`** = `NtWaitForSingleObject`;
  **`wait_any`** = `NtWaitForMultipleObjects(WaitAny)`, при нескольких
  взведённых — наименьший индекс; **`is_raised`** = ожидание с таймаутом 0
  (для NotificationEvent не сбрасывает) (`Beacon::{raise, lower, wait,
  wait_any, is_raised}`). 0.10: `raise`/`lower` ещё и пишут состояние маяка
  (ниже).
- **Нормативный порядок сервера**: `raise` строго **после** того, как лобби
  (секция + события) создано; `lower` **до** остановки/закрытия лобби. Тогда
  «маяк взведён, лобби нет» бывает только после падения сервера.
- **Состояние маяка (0.10, `<B>_beacon_state`)** — секция 4096 байт (таблица
  §b.2), поля `u32` LE, только атомарный доступ:

  | Смещение | Поле | Пишет | Смысл |
  |---|---|---|---|
  | 0 | `magic` = `0x4E434258` (`'XBCN'`) | первый открывший: CAS `0 → magic` | иное ненулевое значение — порча: состоянию не доверяют, клиент маяк **не** сбрасывает |
  | 4 | `generation` | сервер: `+1` в каждом `raise` (SeqCst/locked) | точка линеаризации взвода |
  | 8 | `server_pid` | сервер, до `generation` | PID взводившего; `0` — не публиковалось |
  | 12 | `server_start_lo` | сервер, до `server_pid` | младшие 32 бита `CreateTime` процесса сервера (как `server_start_*` §c.1) |
  | 16 | `server_start_hi` | сервер, до `server_pid` | старшие 32 бита; `0:0` — время неизвестно, сверки нет |

  Остальные байты зарезервированы (нули).
- **`raise` (сервер, 0.10)**: при верном `magic` — записать
  `server_start_lo`, `server_start_hi`, затем `server_pid` (release), затем
  `generation += 1` (атомарный инкремент с полным барьером), и только потом
  `NtSetEvent`. При неверном `magic` публикация пропускается, `NtSetEvent`
  выполняется. **Клиент `raise`/`lower` именованного маяка НЕ вызывает**
  (`raise` клиента записал бы его PID как сервера).
- **`lower` (сервер, 0.10)**: при верном `magic` — записать `server_pid = 0`,
  затем `server_start_lo = server_start_hi = 0` (release), затем
  `NtResetEvent`. Иначе клиент, вернувший маяк по сменившемуся поколению уже
  после этого `lower` (снимок — при мёртвом прежнем сервере, затем `raise` и
  `lower` нового), оставил бы маяк взведённым без лобби, а все следующие
  клиенты видели бы «сервер жив» до выхода его процесса и крутились бы в
  опросе. С `server_pid = 0` следующий `clear_if_stale` идёт путём «сервера
  нет» и сбрасывает маяк (поколение не менялось).
- **`clear_if_stale` (клиент, 0.10)** — вместо прежнего `lower` клиента:
  1. `magic` неверный (или маяк безымянный) → `false`, маяк не трогать;
  2. прочитать `g = generation` (первым), затем `server_pid`,
     `server_start_lo/hi`;
  3. `server_pid != 0` и процесс сервера **не подтверждённо мёртв** → `false`,
     маяк не трогать. «Подтверждённо мёртв» (`win::is_process_gone`):
     `NtOpenProcess` вернул `STATUS_INVALID_CID (0xC000000B)` (любой другой
     отказ — двусмысленность, «жив»), либо процесс сигнален, либо
     время сервера ненулевое и `CreateTime` открытого процесса другое (PID
     переиспользован). Нет прав / время не узнать — «жив»;
  4. `NtResetEvent(маяк)`, полный барьер, повторно прочитать `generation`:
     `!= g` → `NtSetEvent(маяк)` (новый сервер взвёл маяк между шагами 2 и 4);
  5. вернуть `true`.

  Корректность: новый сервер увеличивает `generation` **до** `NtSetEvent`,
  клиент перечитывает его **после** `NtResetEvent`, — либо клиент видит
  новое поколение и возвращает маяк сам, либо `NtSetEvent` сервера идёт после
  сброса. `server_pid = 0` (никто не публиковал: секция создана заново после
  ухода всех держателей, сервер штатно ушёл через `lower`, или пир её не
  пишет) — шаг 3 пропускается: сброс с перепроверкой поколения.

  Мусор в личности сервера при верном `magic` (PID несуществующего процесса,
  чужое время создания) допускает сброс — не новая возможность: любой процесс
  с доступом к маяку (NULL DACL до 0.10, тот же пользователь с 0.10) и так
  может сбросить его напрямую. Неверный `magic` — маяк не
  трогается.
- **Гонка остаётся (явно)** в двух случаях — там клиент защищён только
  перепроверкой лобби, как до 0.10, и при ≥ 2 клиентах один может погасить
  маяк, только что взведённый новым сервером (находка 11):
  1. **нативный сервер по старому контракту** не пишет `<B>_beacon_state`
     (`server_pid = 0`, `generation` в `raise` не растёт) — перепроверка
     поколения его взвод не замечает;
  2. **маяк без состояния** (`Beacon::open` при `STATUS_PRIVILEGE_NOT_HELD`/
     `STATUS_ACCESS_DENIED` на секции состояния — например, `Global\` из
     сессии != 0 без `SeCreateGlobalPrivilege`, пока сервер-служба секцию не
     создал): `raise` только взводит событие, `clear_if_stale` = `NtResetEvent`
     без проверок, `Ok(true)`. Открытие маяка при этом не отказывает — как в
     0.9.
- **Нормативный алгоритм клиента** (`src/beacon.rs`, пример — README
  «Server discovery», prof-shm `run`):
  1. спать в `wait_any([свой_стоп, маяк], None)`;
  2. маяк взведён → попытаться подключиться;
  3. не удалось и **лобби нет** (`NtOpenSection` не прошёл) →
     `clear_if_stale`: `true` → перепроверить лобби (есть — сразу к шагу 2,
     без ожидания маяка; нет — к шагу 1); `false` (сервер жив) — пауза,
     прерываемая своим стопом, затем к шагу 1.
     Потерянного пробуждения нет даже при нескольких клиентах (находка 11):
     до 0.10 клиент делал `lower` + перепроверку лобби + `raise`, и клиент A
     мог погасить маяк, только что взведённый новым сервером, — клиент B
     засыпал навсегда.
- **`generation` в `<B>_beacon_state` (0.10) — не «маяк поколения» prof-shm.**
  Здесь это счётчик взводов одного маяка `<B>_beacon` (для `clear_if_stale`).
  «Маяк поколения» prof-shm — отдельное событие с именем
  `<лобби>.gen.<016x>` по поколению вьюера, понятие его протокола (см. его
  `PROTOCOL.md`); xshm такого объекта не создаёт.
- **Ожидание выхода процесса** (`ProcessExit::{open, has_exited}`, `xshm::wait_any`):
  `NtOpenProcess` как в таблице; `open(0)` → нет объекта; handle удерживается
  (переиспользование PID не путает); `has_exited` = `NtWaitForSingleObject` с
  таймаутом 0 == `STATUS_WAIT_0`; ждётся в том же
  `NtWaitForMultipleObjects`, что и маяки. В отличие от `ProcessWatch::open_peer`
  (детекция смерти канала, 0.8), `ProcessExit::open` **не** отбрасывает уже
  завершившийся процесс — такой handle сразу сигнален.

### b.4 Приватное пространство имён (`PrivateNamespace`, новое в 0.10)

Необязательно: по умолчанию объекты живут в `\BaseNamedObjects` (§b.1), как
раньше. Пространство включает создающая сторона (`ChannelConfig::namespace`,
`AutoOptions`/`MultiOptions`/`DispatchOptions::namespace`, `Beacon::open_in`)
и открывающая (`SharedClient::connect_in`,
`AutoOptions`/`MultiClientOptions`/`DispatchClientOptions::namespace`,
`DispatchClient::lobby_exists_in`). Обе стороны ОБЯЗАНЫ работать в одном
пространстве: объект в пространстве снаружи не виден
(`STATUS_OBJECT_NAME_NOT_FOUND (0xC0000034)`), и наоборот (`src/namespace.rs`).

- **Дескриптор границы** (идентичность пространства):
  `RtlCreateBoundaryDescriptor(Name = <boundary>, Flags = 0)` (имя — строка
  вызывающего, непустая, без `\`), затем
  `RtlAddSIDToBoundaryDescriptor(SID пользователя токена процесса,
  NtQueryInformationToken(TokenUser))`, затем — только для политики
  `CurrentUserAllowLowIntegrity` —
  `RtlAddIntegrityLabelToBoundaryDescriptor(S-1-16-4096)` (Low). Ядро ищет
  пространство по границе целиком (имя + SID + метка): граница с меткой Low —
  **другое** пространство, поэтому обе стороны передают одну политику.
  Win32-эквивалент: `CreateBoundaryDescriptorW(boundary, 0)` +
  `AddSIDToBoundaryDescriptor` [+ `AddIntegrityLabelToBoundaryDescriptor`].
- **Создание или открытие** — любая сторона, порядок запуска любой:
  `NtCreatePrivateNamespace(DIRECTORY_ALL_ACCESS = 0x000F000F,
  OBJECT_ATTRIBUTES{ObjectName = NULL, Attributes = 0, SecurityDescriptor =
  политика создателя §b.2}, boundary)`; `STATUS_OBJECT_NAME_COLLISION
  (0xC0000035)` — `NtOpenPrivateNamespace(DIRECTORY_ALL_ACCESS,
  OBJECT_ATTRIBUTES{ObjectName = NULL, SecurityDescriptor = NULL},
  boundary)`; если открытие вернуло `STATUS_OBJECT_PATH_NOT_FOUND
  (0xC000003A)`/`STATUS_OBJECT_NAME_NOT_FOUND` (последний держатель закрыл
  пространство между вызовами) — снова создание (Rust: до 8 попыток).
  `ObjectName` ОБЯЗАН быть NULL: имя ядро отвергает
  (`STATUS_OBJECT_NAME_INVALID`).
- **Псевдоним (`alias`)** — как `lpAliasPrefix` у Win32
  `CreatePrivateNamespaceW`: ядро его не использует, на идентичность он не
  влияет, Rust его в ядро не передаёт (только диагностика). Пиры с одной
  границей и разными псевдонимами — в **одном** пространстве.
- **Имена объектов — относительные**: `OBJECT_ATTRIBUTES{RootDirectory =
  handle пространства, ObjectName = имя}`; имя — непустое, без
  `Local\`/`Global\` и без `\` (Rust: `InvalidConfig`). Это имена §b.1 без
  префикса namespace: секция `B`, события `B_<DIR>_<SUFFIX>`, маяк
  `B_beacon` и `B_beacon_state`, мьютекс лобби `B_lock`, слоты Multi `B_N`
  с их событиями. Канал Dispatch — 16 hex **без префикса** в пространстве
  лобби (§d.2 шаг 6: префикс namespace лобби пуст); клиент открывает имя из
  ответа лобби относительно своего пространства (имя с `\` — отказ). Типы,
  флаги, права доступа и дескрипторы объектов — как в §b.2.
- **Кто держит**: каждая сторона открывает пространство сама по той же
  границе. Создатель объектов держит handle пространства, пока живы его
  объекты (сервер канала, слоты Multi, лобби и каналы Dispatch, маяк);
  клиенты Auto/Multi/Dispatch — всё время жизни (переподключение ищет
  объекты там же); `SharedClient::connect_in` — на время подключения.
- **Удаление**: Rust `NtDeletePrivateNamespace` **не вызывает** — пространство
  исчезает, когда закрыт последний handle в любом процессе. Удалённое
  пространство остаётся у пиров, держащих handle, «висящим» каталогом, а
  новое `NtCreatePrivateNamespace` с той же границей создаёт **другое**
  пространство: перезапущенный сервер и переподключающийся клиент
  разошлись бы. Нативной реализации — тоже не удалять.
- **Безопасность**: создать или открыть пространство может только токен, в
  котором есть все SID границы (тот же пользователь; Low-процесс — только с
  меткой Low в границе). Процесс другого пользователя не видит объекты и не
  может занять их имена заранее (§h п. 8). Сценарий служба SYSTEM ↔
  пользователь с пространством не работает (SID границ разные — разные
  пространства): для него — `Global\` + политика `ObjectSecurity` (§b.2).

---

## c. Раскладка разделяемой памяти — формат `0x0002_0000` (xshm 0.10)

**Разрыв совместимости с 0.9.** Размер колец, лимит сообщений и максимальный
размер сообщения задаются **по каждому направлению** и записаны в самом
сегменте; кадр — `u32`-длина. Пир 0.9 (`SHARED_VERSION = 0x00010000`) и пир
0.10 друг к другу **не подключаются**: каждая сторона сверяет `version` и
отвергает чужой формат (Rust 0.10 — `ShmError::VersionMismatch`, Rust 0.9 —
`HandshakeFailed`). Молча разойтись в смещениях стороны не могут.

`SHARED_MAGIC = 0x5853484D` (байты в памяти: `4D 48 53 58`),
`SHARED_VERSION = 0x00020000` (`src/constants.rs`). Все поля — `u32`
little-endian, доступ к ним — атомарный (выровненные 4-байтовые слова).

```text
0      ControlBlock   (64)   magic, version, generation, состояния, claim, PID
64     LayoutBlock    (64)   размер сегмента + дескрипторы обоих направлений
A      RingHeader S2C (64)   ┐ A = LayoutBlock.s2c.offset   (канонически 128)
A+64   кольцо S2C           ┘ ёмкость s2c.capacity
B      RingHeader C2S (64)   ┐ B = LayoutBlock.c2s.offset   (канонически A+64+cap_s2c)
B+64   кольцо C2S           ┘ ёмкость c2s.capacity
```

**Проверка тестами.** Каждое число этого раздела закреплено unit-тестом
модуля `src/layout.rs` (`cargo test --lib layout::tests`); размеры и
смещения wire-типов вдобавок проверяются `const`-утверждениями при каждой
сборке (в том числе у потребителя):

| Таблица / значение | Тест |
|---|---|
| размеры блоков (64/64/64/16), `SEGMENT_HEADER_SIZE = 128`, `segment_size` по умолчанию | `layout::tests::layout_sizes_are_exact` |
| §c.1 смещения `ControlBlock` | `layout::tests::control_block_offsets_are_stable` |
| §c.2 смещения `LayoutBlock` и дескриптора кольца | `layout::tests::layout_block_offsets_are_stable` |
| §c.3 смещения `RingHeader` | `layout::tests::ring_header_offsets_are_stable` |
| §c.1–§c.3 байты сегмента по смещениям таблиц (LE `u32`, канонические дескрипторы, `connection_gen = 1`) | `layout::tests::initialized_segment_bytes_match_interop` |
| `SHARED_MAGIC`/`SHARED_VERSION`, константы кадра §c.4 (`8`, `FRAME_PAD = 1`, `frame_size`), пределы ёмкости, состояния рукопожатия, `CLAIM_FREE`, `RECOUNT_MAX = 64` (§c.5) | `layout::tests::wire_constants_are_stable` |
| §c.2 ограничения ёмкости и `max_message_size ≤ capacity / 2 − 8` | `layout::tests::ring_config_validation` |
| §c.2 раскладка по умолчанию, лобби по умолчанию, каноническая раскладка (`128`, `A + 64 + cap_s2c`, `256 + cap_s2c + cap_c2s`) | `layout::tests::default_and_canonical_layouts_match_interop` |

Отказы клиента по §c.2/§c.5 (п. 2–4) проверяют `shared::tests::attach_*`
(в том числе фаззер `attach_fuzz_never_panics`).

### c.1 `ControlBlock` (смещение 0, 64 байта)

| Смещение | Поле | Смысл |
|---|---|---|
| 0 | `magic` | `SHARED_MAGIC`; пишется **последним** (release), когда весь сегмент инициализирован. `0` — сервер ещё инициализирует |
| 4 | `version` | `0x00020000` |
| 8 | `generation` | как в 0.9 (R15/R16) |
| 12 | `server_state` | `IDLE=0`, `CLIENT_HELLO=1`, `SERVER_READY=2` |
| 16 | `client_state` | то же |
| 20 | `slot_claim` | Multi и single-client (ревизия 3, R18): токен владельца заявки/сессии, `0` — свободно. Захват — `CAS 0 → T`, освобождение — `CAS T → 0` (бывш. `reserved[0]`; до ревизии 3 — только Multi) |
| 24 | `slot_owner_pid` | Multi и single-client (рев. 3): PID процесса-владельца claim-а; пишет захвативший сразу после CAS (Release) (бывш. `reserved[1]`) |
| 28 | `server_pid` | PID сервера (бывш. `reserved[2]`) |
| 32 | `client_pid` | PID клиента: пишется до `CLIENT_HELLO`, одноразовый — сервер потребляет его после фиксации, только застав владельца заявки в сессии (ревизия 3, S6: CAS `pid → 0`; до ревизии 3 — `swap(0)`) (бывш. `reserved[3]`) |
| 36 | `server_start_lo` | Время создания процесса-сервера (`KERNEL_USER_TIMES::CreateTime`, 100-нс FILETIME), младшие 32 бита. Пишет сервер при инициализации **до** `magic` вместе с `server_pid` |
| 40 | `server_start_hi` | старшие 32 бита `server_start` |
| 44 | `client_start_lo` | Время создания процесса-клиента, младшие 32 бита. Пишет клиент **до** `client_pid` (т.е. до `CLIENT_HELLO`); сервер читает парой с `client_pid` и обнуляет вместе с его потреблением (ревизия 3, S6); откат handshake клиентом обнуляет вместе с `client_pid` |
| 48 | `client_start_hi` | старшие 32 бита `client_start` |
| 52 | `owner_start_lo` | Multi и single-client (рев. 3): время создания процесса-владельца claim-а (**настоящее**, не заявляемое), младшие 32 бита. Пишет клиент сразу после CAS `slot_claim`, **до** `slot_owner_pid` (Release). По нему сервер подтверждает смерть держателя (S2) |
| 56 | `owner_start_hi` | старшие 32 бита `owner_start` |
| 60 | `slot_count` | Multi: число слотов сервера (`max_clients`, 1..=256), одинаковое во всех слотах; пишет сервер при инициализации **до** `magic`. Подсказка: клиенты по нему перебор НЕ ограничивают (§i.1). Не Multi — `0` |

Смещения полей 0.9 не сдвинулись. Нормативный текст называет поля по
именам; запись `reserved[N]` осталась только в исторических таблицах
(колонки «было», «бывш.») и означает поле с тем же смещением:
`reserved[0]` = `slot_claim`, `reserved[1]` = `slot_owner_pid`,
`reserved[2]` = `server_pid`, `reserved[3]` = `client_pid`.

**Время создания процесса** (0.10, находка 23 аудита). 64-битное значение
хранится двумя `u32` (атомарной 64-битной записи на x86 нет): пишущий
записывает `lo`, затем `hi`, затем публикующее поле с release (`magic`,
`client_pid`, `slot_owner_pid`); читающий читает публикующее поле с acquire,
затем `lo | hi << 32`. `0` — «время не сообщено» (пир старой версии или
запрос не удался): сверки нет, поведение как в 0.9. Иначе наблюдатель,
открыв процесс по PID, сверяет его `CreateTime` (`NtQueryInformationProcess(
ProcessTimes)`, право `PROCESS_QUERY_LIMITED_INFORMATION`): процесс с другим
временем — **не пир** (PID переиспользован после смерти пира или подделан) —
handle процесса не удерживается, разовая проверка Multi считает владельца
слота умершим. Время, которое узнать не удалось, — handle тоже не
удерживается (личность не подтверждена), а разовая проверка консервативно
считает процесс живым.

### c.2 `LayoutBlock` (смещение 64, 64 байта) — новое

| Смещение | Поле | Смысл |
|---|---|---|
| 64 | `segment_size` | полный размер сегмента в байтах |
| 68 | `flags` | `0`; иное значение — сегмент отвергается (`Corrupted`) |
| 72 | `s2c` | дескриптор кольца сервер → клиент (16 байт, ниже) |
| 88 | `c2s` | дескриптор кольца клиент → сервер |
| 104 | `reserved[6]` | нули |

Дескриптор кольца (16 байт): `offset` (+0, смещение `RingHeader` от начала
сегмента; данные — сразу за ним), `capacity` (+4), `max_messages` (+8),
`max_message_size` (+12).

Ограничения (нарушение любого — сегмент отвергается, `Corrupted`):

- `capacity` — степень двойки, `4 096 ..= 1 073 741 824` (позиции — `u32` с
  переполнением по модулю 2³²; ёмкость обязана делить 2³²);
- `max_messages ≥ 1`;
- `1 ≤ max_message_size ≤ capacity / 2 − 8` (0.10, п. 26 «Изменений 0.10»;
  до PAD-кадров было `capacity − 8`): кадр максимальной длины **вместе с
  наихудшим PAD-кадром** (§c.4) помещается в пустое кольцо при любой позиции
  записи. Вывод: кадр `F = frame_size(len)` кратен 8, остаток кольца от
  позиции записи до конца `t` кратен 8 и `t ≥ 8`. При `F ≤ t` кадр
  занимает `F`, иначе PAD на весь остаток + кадр с начала кольца, `t + F`.
  Наихудший остаток, при котором PAD ещё нужен, — `t = F − 8`, запись
  занимает `2F − 8`. Пустое кольцо принимает её при любой позиции, только
  если `2F − 8 ≤ capacity` ⇔ `F ≤ capacity / 2 + 4` ⇔ (кратность 8,
  `capacity / 2` кратна 8) `F ≤ capacity / 2` ⇔ `len ≤ capacity / 2 − 8`.
  Граница точная: при `F > capacity / 2` позиция с остатком `F − 8`
  достижима, и в ней сообщение не вставляется никогда (даже в пустое
  кольцо). Rust-клиент отвергает сегмент с большим `max_message_size`
  (`Corrupted`), Rust-сервер такой не создаёт (`InvalidConfig`);
- `offset` кратен 64, `offset ≥ 128`, `offset + 64 + capacity ≤ segment_size`;
  кольца не пересекаются;
- `segment_size ≤` фактического размера отображения секции.

Смещения записаны явно: клиент проверяет только границы, поэтому сервер
вправе разложить кольца иначе (например, выровнять данные на страницу), не
меняя версию. Rust-сервер пишет каноническую раскладку
(`segment_size = 256 + cap_s2c + cap_c2s`).

Раскладка по умолчанию (`ChannelLayout::DEFAULT`, она же раскладка 0.9 по
объёму): оба кольца — `2 097 152` байт, `max_messages = 500`,
`max_message_size = 65 535`; `segment_size = 4 194 560`. Лобби Dispatch по
умолчанию — `16 384` байт на кольцо, 16 сообщений, сообщение до 1 024 байт
(`DEFAULT_LOBBY_LAYOUT`; раскладка самоописываемая, клиенту лобби её знать не
нужно).

### c.3 `RingHeader` (64 байта) — без изменений

`write_pos@0`, `read_pos@4`, `message_count@8`, `drop_count@12`,
`sequence@16`, `connection_gen@20`, `handshake_state@24`, `space_waiter@28`,
`reserved[8]@32`. `space_waiter` — сколько **свободных байт** нужно ждущему
писателю (0.10): `need(W, frame_size(len))` в его текущей позиции записи `W`,
то есть кадр с выравниванием и, если кадр не помещается до конца кольца, PAD
перед ним (§c.4). Читатель сравнивает заявку со всеми свободными байтами
(`capacity − (write_pos − read_pos) ≥ space_waiter`).

### c.4 Кадр — `u32` длина, `u32` флаги, выравнивание 8, непрерывный payload

```text
+0  u32 len    сообщение: длина payload (1 ..= max_message_size кольца);
               PAD: остаток кольца до границы минус 8 (может быть 0)
+4  u32 flags  бит 0 = FRAME_PAD (0x1); остальные биты зарезервированы, = 0
+8  payload    len байт
    дополнение до кратного 8 (содержимое не определено)
```

`frame_size(len) = (8 + len + 7) & !7`. Позиции кадров (`write_pos`,
`read_pos`) **всегда кратны 8**, ёмкость кратна 8 — заголовок кадра никогда не
разрывается границей кольца. `MIN_MESSAGE_SIZE = 1` (пустые сообщения не
передаются; в 0.9 было 2).

**Непрерывные кадры (0.10).** Кадр (заголовок + payload) **никогда не
разрывается границей кольца**: payload лежит одним куском, его можно читать и
писать на месте (zero-copy). Индексы: `index(pos) = pos & (capacity − 1)`,
остаток до конца `tail(pos) = capacity − index(pos)` (кратен 8, `8 ..=
capacity`).

- **Писатель**, кладущий кадр `F = frame_size(len)` с позиции `W`:
  - `F ≤ tail(W)` — кадр в `[W, W + F)`, сдвиг `write_pos` на `F`;
  - `F > tail(W)` — сначала **PAD-кадр** на весь остаток: заголовок в
    `index(W)`: `len = tail(W) − 8`, `flags = FRAME_PAD`; затем кадр с
    индекса `0` (позиция `W + tail(W)`); сдвиг `write_pos` на
    `tail(W) + F`. Место под запись — `need(W, F) = F` или `tail(W) + F`;
    его же писатель сравнивает со свободным местом (`capacity −
    (write_pos − read_pos) ≥ need`) и пишет в `space_waiter`.
  - PAD и кадр публикуются **одной** записью `write_pos` (release): читатель
    никогда не видит PAD без следующего за ним кадра. Решение о сигнале
    `DATA` (§e.2 п. 1) принимается один раз, сравнением `read_pos` с `W` —
    позицией **до** PAD.
  - PAD — не сообщение: `message_count` растёт на 1 только за кадр
    сообщения; PAD не входит в лимит `max_messages` и не считается в
    `drop_count`.
- **Читатель** в позиции `R` (`write_pos − R > 0`) читает `len` и `flags`
  заголовка в `index(R)`:
  - `flags = 0` — сообщение: `len ∈ 1 ..= max_message_size`,
    `frame_size(len) ≤ write_pos − R` и `frame_size(len) ≤ tail(R)` (кадр не
    пересекает конец кольца), иначе `Corrupted`;
  - `flags = FRAME_PAD` — PAD: ОБЯЗАН кончаться ровно на границе кольца
    (`len = tail(R) − 8`), стоять **не на индексе 0** (`index(R) ≠ 0`, то
    есть PAD никогда не занимает всё кольцо) и лежать в опубликованных данных
    (`tail(R) ≤ write_pos − R`), иначе `Corrupted`. Честный писатель PAD на
    индексе 0 не пишет никогда (кадр `≤ capacity / 2` с индекса 0 помещается
    сам), и за PAD в той же публикации всегда стоит сообщение с индекса 0 —
    два PAD подряд невозможны, пропуск PAD у читателя ограничен. Читатель сдвигает `read_pos` за PAD
    (`R + tail(R)`, тем же CAS, что и при чтении) **без доставки** и без
    изменения `message_count`, затем читает следующий кадр с индекса `0`;
  - **любые другие биты `flags`** (в том числе `FRAME_PAD | x`) —
    `Corrupted`. Новые биты будут вводиться только со сменой
    `SHARED_VERSION`, поэтому «неизвестный бит» — всегда порча, а не новая
    функция.
- **Перезапись (overwrite-oldest)**: вытесняя старейший кадр, писатель
  вытесняет и PAD-кадры (CAS `read_pos` за PAD), но в `drop_count` и в число
  вытесненных сообщений их не считает и `message_count` не уменьшает.
- Граница `max_message_size ≤ capacity / 2 − 8` (§c.2) гарантирует, что
  PAD + кадр всегда помещаются в пустое кольцо.

Нативный пир ОБЯЗАН реализовать PAD **на обеих сторонах**: его читатель
получает PAD от Rust-писателя (Rust 0.10 payload через границу не пишет), а
кадр, payload которого пересекает конец кольца, Rust-читатель 0.10 отвергает
как `Corrupted`.

### c.5 Обязанности сторон

**Сервер (создание):**

1. `NtCreateSection(size = segment_size)` без `OBJ_OPENIF`.
2. Все поля `ControlBlock` кроме `magic`, весь `LayoutBlock`, оба
   `RingHeader` (`connection_gen = generation = 1`).
3. `version`, затем **`magic` последним** с release-семантикой (на x86
   достаточно обычной записи после остальных; компилятору — барьер).
4. События канала (§b.2). Клиент, открывший секцию раньше событий, получает
   ошибку открытия событий и повторяет попытку.

**Клиент (открытие):**

1. `NtOpenSection` + `NtQuerySection(SectionBasicInformation)`: клиент
   ОБЯЗАН отвергать секции не `SEC_COMMIT` — отказ, если в
   `AllocationAttributes` есть `SEC_RESERVE` (`0x04000000`), `SEC_IMAGE`
   (`0x01000000`) или `SEC_FILE` (`0x00800000`), либо нет `SEC_COMMIT`
   (`0x08000000`) (0.10, `Mapping::open_view`). Также ОБЯЗАН отвергать
   секции с атрибутом кэширования: `SEC_NOCACHE` (`0x10000000`) и
   `SEC_WRITECOMBINE` (`0x40000000`) (0.10, «Изменения 0.10» п. 31) —
   на WC-памяти записи не упорядочены (TSO нарушен, публикации
   Release/Acquire кольца теряют смысл), на UC-памяти каждая атомарная
   операция идёт мимо кэша. Проверка — запрет-список: прочие биты
   `AllocationAttributes` (`SEC_LARGE_PAGES` `0x80000000`, служебные биты
   ядра) допускаются. Отказ — `ShmError::Corrupted`, без повтора. Правило
   действует на всех путях открытия чужой секции: секция канала (в т.ч.
   слоты Multi при переборе — отображение только заголовка) и состояние
   маяка, найденное существующим (§b.2). Затем отображение секции
   целиком (`ViewSize = 0`).
2. Отображение меньше 128 байт → отказ.
3. `magic` (acquire): `0` → «не готово» (повторить позже), иное ≠
   `SHARED_MAGIC` → отказ; `version ≠ 0x00020000` → отказ.
4. `LayoutBlock`: проверки §c.2 против **фактического** размера отображения.
   Раскладку скопировать к себе один раз: дальнейшие изменения пиром ни на
   что не влияют.
5. События канала открываются с проверкой типа: каждое ОБЯЗАНО быть
   SynchronizationEvent (`NtQueryEvent`), иначе отказ — событие с ручным
   сбросом под чужим именем держало бы ожидание взведённым.

**Писатель кольца** соблюдает лимиты своего направления (`max_messages`,
`max_message_size`) и пишет кадр так: при `frame_size(len) > tail(W)` —
заголовок PAD (§c.4); заголовок кадра (`len`, `flags = 0`) и payload одним
куском → `message_count += 1` → `write_pos += need(W, frame_size(len))`
(release, одна запись на PAD + кадр) → полный барьер → чтение `read_pos`
(решение о сигнале `DATA` по `W`, §e.2 п. 1). Место проверяется под
`need(W, F)`, а не под `F`.
Перед записью писатель ОБЯЗАН проверить обе позиции из заголовка (их может
испортить читатель-пир): кратность 8 и `write_pos − read_pos ≤ capacity`.
Иначе — `Corrupted`, ни одного байта в кольцо: невыровненная позиция у конца
кольца увела бы заголовок кадра за его пределы. Лимит `max_messages`
применяется только к непустому по позициям кольцу: пустое
(`write_pos == read_pos`) принимает кадр, помещающийся по байтам, при любом
`message_count` (§e.2 п. 1) — по одному счётчику ни `Corrupted`, ни «полно».
**Правило лимита в непустом кольце** (0.10, п. 27 «Изменений 0.10»;
нормативно — нативный писатель ОБЯЗАН применять его с тем же порогом
`RECOUNT_MAX = 64`). Счётчик законно отстаёт от `read_pos` на 1 (читатель
уменьшает его после CAS), поэтому «полно» по одному `message_count ≥
max_messages` вытесняло бы сообщение на кадр раньше. Писатель берёт ОДИН
снимок `W = write_pos`, `R = read_pos`, `C = message_count` и по нему же
решает и место по байтам, и лимит (`read_pos` по ходу не перечитывается):

- `C < max_messages` — лимит не мешает;
- иначе при `max_messages ≤ RECOUNT_MAX` — **пересчёт по кадрам** окна
  `[R, W)`: обход заголовков от `R` (PAD не считаются, счёт до
  `max_messages`, не больше `2 · RECOUNT_MAX + 1` заголовков); «полно» —
  если пересчитано `max_messages`; кадр, не прошедший проверки читателя
  (§c.4), — «полно». Лимит точный;
- иначе (`max_messages > RECOUNT_MAX`) — **по счётчику с запасом на
  отставание**: «полно» только при `C > max_messages` (то есть `C ≥
  max_messages + 1`). Отставание честного читателя на 1 раннего вытеснения
  не даёт; цена — в кольце бывает `max_messages + 1` сообщение. O(1).

«Полно» — запись с перезаписью вытесняет старейший кадр и повторяет
решение со свежим снимком, без перезаписи — отказ. Гарантия «сообщений в
кольце не больше `max_messages` (`max_messages + 1` выше порога)» действует
только при **честном читателе**: испорченный пиром счётчик её снимает
(§h п. 9), байтовая граница — нет.

**Читатель кольца** ОБЯЗАН не доверять данным пира (граница доверия — NULL
DACL): при `write_pos − read_pos` больше ёмкости, `read_pos` не кратном 8,
`len` вне `1 ..= max_message_size`, `frame_size(len) > write_pos − read_pos`,
кадре сообщения, пересекающем конец кольца, PAD-кадре не до границы кольца
или за `write_pos`, неизвестных битах `flags` (§c.4) — ошибка `Corrupted` и
разрыв соединения. Разрыв — **уход своей стороны с сигналом** (§e.2 п. 5):
клиент — полный уход L (`CAS client_state: SERVER_READY → IDLE`, заголовки
`IDLE`, `S2C_DISCONNECT`, claim последним), сервер — состояния `IDLE`, затем
`S2C_DISCONNECT`. Пир в сессии ждёт без таймаута: молчаливый разрыв оставил
бы его «подключённым» навсегда, а при исключительной заявке (R18) канал
стал бы недоступен и для следующего подключения. Пустота кольца определяется по
`write_pos == read_pos`, а не по `message_count`; `message_count` уменьшается
без ухода ниже нуля и может кратковременно отличаться от числа кадров (в том
числе превышать `max_messages`: счётчик отстаёт от `read_pos`) — это НЕ
порча, читатель не проверяет его на соответствие лимиту. Прежде чем признать кольцо пустым и уснуть на `DATA`,
читатель ОБЯЗАН выполнить полный барьер и перечитать `write_pos` (§e.2 п. 1).

---

## d. Протокол лобби (Dispatch)

Лобби — обычный канал xshm с именем `B` (секция + 7 событий), который сервер
держит всегда; каждый клиент получает **собственный** канал.

### d.1 Сообщения регистрации (байты без изменений; 0.10 — строгий разбор, `src/dispatch/protocol.rs`)

Все целые — little-endian. `DISPATCH_MAGIC = 0x44495350` (байты `50 53 49 44`),
`DISPATCH_VERSION = 2`.

Запрос (клиент → сервер, кольцо B), 13 + `name_len` байт:
`[0..4] magic` · `[4] version=2` · `[5] msg_type=1` · `[6..10] pid u32` ·
`[10..12] revision u16` · `[12] name_len u8 (≤ 64)` · `[13..] name UTF-8`
(усечение по границе символа).

Ответ (сервер → клиент, кольцо A), 12 + `len` байт:
`[0..4] magic` · `[4] version=2` · `[5] msg_type=2` · `[6] status` (`0` — OK,
`1` — REJECTED) · `[7..11] client_id u32` · `[11] channel_name_len u8 (≤ 64)`
· `[12..] channel_name UTF-8`.

**Строгость разбора (0.10).** Сообщение — **ровно** заголовок + имя: длина
сообщения в кадре ОБЯЗАНА быть `13 + name_len` (запрос) / `12 + len` (ответ).
Отвергаются (`Corrupted`): `name_len`/`channel_name_len` > 64, байты после
имени, имя не в валидном UTF-8 (до 0.10 лишние байты молча отбрасывались, а
битый UTF-8 заменялся на U+FFFD). Меньше байтов, чем `name_len`, —
`MessageTooSmall`; чужой `magic` — `Corrupted`; чужие `version`/`msg_type` —
`HandshakeFailed`. **Сервер на любую ошибку разбора запроса отвечает
`STATUS_REJECTED`** (+ `S2C_DATA`, шаг 6 §d.2) — до 0.10 он молчал, и клиент
ждал весь `response_timeout`. Нативный пир ОБЯЗАН кодировать так же
(усечение имени по границе символа, без хвоста) — иначе Rust 0.10 ему
откажет.

### d.2 Последовательность подключения (0.9)

1. **Сервер `start`** (синхронно, `DispatchServer::start`): `SharedServer::start(B)` —
   секция лобби и 7 событий; ошибка (имя занято) → `start` возвращает ошибку,
   поток не создаётся. Затем безымянное событие остановки, поток лобби.
   Только после `Ok` сервер вправе взвести маяк (§b.3).
2. **Поток лобби** ждёт `{C2S_CONNECT_REQ, стоп}` без таймаута
   (`DispatchCore::worker_loop`, `SharedServer::wait_for_client_or`); `CONNECT_REQ`
   первым индексом. `CONNECT_REQ` без `CLIENT_HELLO` (клиент откатил заявку)
   — 0.9: просто игнорируется, лобби **не** пересоздаётся; 0.10: сигнал
   поглощается внутри ожидания, оно продолжается (§«Изменения 0.10» п. 15).
3. **0.9 — сериализация (R12).** До шага 3 клиент открывает мьютекс
   `<B>_lock` (§b.2) и захватывает его, ожидая не дольше `lobby_timeout`
   (истёк → `Timeout`, лобби не трогается). `STATUS_ABANDONED_WAIT_0` —
   прежний клиент умер с мьютексом, захват состоялся. Мьютекс держится
   **до отключения от лобби** (шаг 7: Drop клиента — CAS состояния в `IDLE` + `S2C_DISCONNECT`)
   и освобождается `NtReleaseMutant` тем же потоком. Так следующий клиент
   пишет `CLIENT_HELLO` строго после отката предыдущего. Сервер со своей
   стороны сбрасывает `client_state` лобби только CAS `SERVER_READY → IDLE`
   (п. 20 §a), поэтому `HELLO` следующего клиента, записанный раньше, чем
   сервер закончил свой `mark_disconnected`, не затирается.
   **Ревизия 3 (R18):** claim лобби (`slot_claim`) клиент берёт строго
   после захвата мьютекса (шаг 3a, C1) и освобождает последним шагом ухода
   (шаг 7), до `NtReleaseMutant`, — при соблюдении R12 claim к захвату
   мьютекса свободен. Мьютекс достался брошенным (`STATUS_ABANDONED_WAIT_0`)
   — прежний держатель умер, возможно, не сняв claim: ЖЕЛАТЕЛЬНО перехватить
   его — при проигранном `CAS 0 → T` с наблюдённым `c ≠ 0` выполнить `CAS c
   → T` (Rust: `ClaimMode::TakeOverAbandoned`), **но только при
   `client_state == IDLE`** (Acquire). Стоящие `CLIENT_HELLO`/`SERVER_READY`
   умершего не перехватывать: обычный отказ `AlreadyConnected` и толчок
   (C1) — заявку умершего снимет сервер (S2), сессию — наблюдение процесса.
   Перехват при стоящем `HELLO` затёр бы в C2/C5 самоописание и
   `slot_owner_pid` умершего, после чего S2 его заявку уже не снял бы, а
   сервер зафиксировал бы ненаблюдаемую сессию мёртвого клиента до
   `lobby_timeout`. `IDLE` при ещё стоящем claim умершего бывает и в окне S2
   (сервер снял заявку, claim ещё не снят): перехват там допустим, потому что
   сервер ОБЯЗАН обнулить самоописание умершего **до** CAS `CLIENT_HELLO →
   IDLE` (шаг 4, S2) — после CAS он полей не трогает, и записанное
   перехватившим в C2/C5 не затирается. Без перехвата лобби отказывает до дедлайна протухшего
   захвата (`RESERVE_TIMEOUT` = 10 с, длиннее `lobby_timeout`).
3a. **Клиент** (`SharedClient::connect_impl` → `Hello::post`,
   `src/client.rs`; ревизия 3 — R15, R18; `T` — свой токен, §i.1) строго по
   порядку:
   0. (C0) `NtOpenSection` лобби/канала, проверки §c.5 (размер, `magic`,
      `version`, `LayoutBlock`), открыть 7 событий. Ошибки — без побочных
      эффектов в памяти;
   1. (C1) `CAS slot_claim: 0 → T` (AcqRel/Acquire). Не удался — канал занят:
      если `server_state != SERVER_READY` (Acquire) —
      `NtSetEvent(C2S_CONNECT_REQ)` («толчок», §i.3), и `AlreadyConnected`.
      **Больше ничего не писать.** Клиент Multi (claim слота уже захвачен
      перебором §i.1) пропускает шаги 1–2;
   2. (C2) `owner_start_lo`/`hi` = **настоящее** время создания своего
      процесса, затем `slot_owner_pid = PID` (Release);
   3. (C3) `g0 = generation` (Acquire) — **до** заявки;
   4. (C4) `NtResetEvent(S2C_CONNECT)` — сигнал чужого/брошенного
      рукопожатия не должен подтвердить нашу заявку;
   5. (C5) время создания своего процесса в `client_start_lo`/`hi`, затем
      свой PID в `client_pid` (Release) (§c.1);
   6. (C6) **`CAS client_state: IDLE → CLIENT_HELLO`** (AcqRel/Acquire;
      ревизия 2 — store). Не удался (чужая заявка или сессия клиента без
      claim, мусор) — `client_pid = 0`, `client_start = 0`, `CAS slot_claim:
      T → 0`, `AlreadyConnected` (Multi — `HandshakeFailed`: снять claim и
      повторить захват);
   7. (C7) `handshake_state = CLIENT_HELLO` в обоих заголовках (Release);
   8. (C8) `slot_claim == T` (Acquire)? Нет — claim отняли по дедлайну
      (держатель завис между C1 и C6 дольше `RESERVE_TIMEOUT` — нарушение
      допущения A1, §h п. 10): отзыв (п. 11); если отзыв покажет «принят» —
      немедленный уход (шаг 7); итог `AlreadyConnected`;
   9. (C9) `NtSetEvent(C2S_CONNECT_REQ)`;
   10. (C10) ждать `S2C_CONNECT` до `lobby_timeout` (по умолчанию 5 с;
       prof-shm — 200 мс). На сигнале `s = client_state` (Acquire):
       **принят**, если `s == SERVER_READY` (сервер зафиксировал нашу
       заявку; другой заявки нет — её ставит только держатель claim-а) или
       `s == IDLE` и `generation != g0` (зафиксировал и уже отключил:
       `S2C_DISCONNECT` взведён, прощальные данные в кольце — их надо
       дочитать, а не откатываться). `s == CLIENT_HELLO` — сигнал
       **устаревший** при любом `generation` (ревизия 3: это `S2C_CONNECT`
       прошлой сессии, а сервер мог уже опубликовать `generation` новой, ещё
       не зафиксировав её) — поглотить и ждать дальше. Rust-worker-ы ждут
       здесь ещё и свой `wake` (остановка прерывает ожидание, это
       внутреннее);
   11. (W) таймаут/остановка/ошибка → отзыв `CAS client_state: CLIENT_HELLO
       → IDLE` (AcqRel/Acquire). Удался → откат: `client_pid = 0`,
       `client_start = 0`, `handshake_state = IDLE` в заголовках, затем `CAS
       slot_claim: T → 0` (Multi — claim снимает перебор после рукопожатия),
       ошибка. Не удался: наблюдён `SERVER_READY` → **подключён**; наблюдён
       `IDLE` при `generation != g0` → **подключён** (как в п. 10); иначе
       (`IDLE` при `generation == g0`: сервер ушёл без рукопожатия или снял
       заявку умершего, шаг 4 S2; мусор) → откат и освобождение, как выше,
       ошибка;
   12. (A) подключён: `G = generation` (Acquire), PID сервера из
       `server_pid`, `connection_gen = G` и `handshake_state = SERVER_READY`
       в заголовках.
4. **Сервер** видит `CONNECT_REQ`, проверяет `client_state == CLIENT_HELLO`
   (Acquire; иначе — толчок или откатившийся клиент: сигнал поглощается,
   single-client сервер проверяет протухший захват, §i.3; лобби не
   пересоздаётся, см. шаг 2) и выполняет `complete_handshake`
   (`src/server.rs`; ревизия 3 — R16, R18):
   - **S1:** `T = slot_claim`, `pid = client_pid`, `start = client_start`
     (Acquire, **без** потребления); handle процесса `pid` с проверкой
     времени (`open_peer`, §c.1);
   - **S2 (заявка умершего):** handle не открылся, `pid ≠ 0`, `pid ==
     slot_owner_pid` (тот же процесс заявил и захватил: защита от ABA, когда
     `pid` прочитан у прежнего клиента) и процесс подтверждённо ушёл
     (`STATUS_INVALID_CID`, завершён или другое `CreateTime`) **и по
     `client_start`, и по `owner_start`** → **непосредственно перед CAS
     перепроверить `slot_claim == T`** (при `T ≠ 0`; Acquire): чтения S1
     могли устареть — прежний клиент отозвал заявку, освободил claim и умер,
     а живой следующий уже поставил свою. Claim сменился — S2 пропустить,
     рукопожатие идёт дальше (S3–S7, S6 наблюдает процесс нового клиента).
     Claim прежний (пока он стоит, новой заявки нет) → `client_pid = 0`,
     `client_start = 0`, заголовки `IDLE` — **до** CAS (fix round 2: после
     CAS, пока claim умершего ещё стоит, его вправе перехватить клиент лобби
     с брошенным мьютексом, шаг 3, и обнуление после CAS затёрло бы его
     самоописание) → `CAS client_state: CLIENT_HELLO → IDLE` → удался — при
     `T ≠ 0` `CAS slot_claim: T → 0`. Итог — `HandshakeFailed` **без** смены
     `generation` и без `S2C_CONNECT`;
   - **S3:** сброс обоих заголовков колец с `new = generation + 1` →
     `generation = new` (Release) → `handshake_state = SERVER_READY` в
     заголовки → `server_state = SERVER_READY` (Release);
   - **S4:** **`NtResetEvent(S2C_DISCONNECT)` — до фиксации** (ревизия 3;
     раньше — после, п. 19 §a): сигнал прошлой сессии сброшен, а прощальный
     сигнал клиента, засчитавшего подключение по проигранному отзыву и сразу
     ушедшего (он взводится уже после фиксации), не теряется;
   - **S5:** **фиксация** `CAS client_state: CLIENT_HELLO → SERVER_READY`. Не
     удался (клиент отозвал заявку) → `server_state` и заголовки в `IDLE`,
     `S2C_CONNECT` **не** сигналится, `HandshakeFailed` (поток лобби его
     игнорирует); опубликованный `generation` остаётся (кольца пусты, вреда
     нет: клиент засчитывает его только при проигранном отзыве);
   - **S6:** `T2 = slot_claim`, `pid2 = client_pid`, `start2 = client_start`
     (Acquire), затем `client_state == SERVER_READY` (Acquire)?
     - **да** — владелец зафиксированной заявки ещё в сессии (свой claim он
       снимает только после `CAS SERVER_READY → IDLE`), прочитанное — его.
       Потребить: `CAS client_pid: pid2 → 0`; удался и `slot_claim` всё ещё
       `T2` — обнулить `client_start`, иначе вернуть `CAS client_pid: 0 →
       pid2` (claim сменился — потреблённое могло оказаться значением
       следующего клиента того же процесса). Claim сессии = `T2`. Если
       `(pid2, start2) ≠ (pid, start)` (между S1 и S5 прежний клиент отозвал
       заявку, а зафиксирована заявка следующего) — открыть handle процесса
       `pid2`; не открылся и держатель подтверждённо умер (как в S2) →
       закрыть сессию (`server_state = IDLE`, `CAS client_state:
       SERVER_READY → IDLE`, заголовки `IDLE`, `CAS slot_claim: T2 → 0`,
       `NtSetEvent(S2C_DISCONNECT)`), `HandshakeFailed`;
     - **нет** — владелец уже ушёл (принят по проигранному отзыву), сессию
       закончит его `DISCONNECT`: поля **не трогать** (они могут быть уже
       следующего клиента), наблюдение и claim сессии — из S1;
   - **S7:** `NtSetEvent(S2C_CONNECT)`, подключён. Сигнал не удался (0.10) —
     сессия уже зафиксирована S5, клиент мог засчитать её по проигранному
     отзыву: закрыть её, как уход сервера по ошибке (п. 29: `server_state =
     IDLE`, `CAS client_state: SERVER_READY → IDLE`, заголовки `IDLE`, затем
     `NtSetEvent(S2C_DISCONNECT)`), и вернуть ошибку.
5. **Клиент** записывает `RegistrationRequest` в кольцо B. Rust сигналит
   `C2S_DATA` дважды: из `send_to_server` (если кольцо было пусто) и явно
   (`lobby_register`). **Нативный клиент ОБЯЗАН сигналить
   `C2S_DATA` после записи запроса**: сервер 0.9 ждёт `{C2S_DATA, стоп}` с
   дедлайном `lobby_timeout` и **не перечитывает** кольцо по таймеру
   (`DispatchCore::handle_lobby_client`); без сигнала — `Timeout` и отказ через 5 с.
   0.9: в том же наборе — handle процесса клиента (если PID получен): клиент,
   умерший до запроса, освобождает лобби сразу, а не через `lobby_timeout`.
6. **Сервер** декодирует запрос, выдаёт `client_id` (счётчик с 1) и имя канала
   — **16 hex-символов из ОС-энтропии** (`format!("{prefix}{:016x}")`,
   `DispatchServer::generate_channel_name`; **без** префикса `B_`, но (0.9) **в
   namespace лобби**: `prefix` = `Global\` / `Local\` / каталог NT-пути лобби
   до последнего `\` включительно / пусто для имени без префикса; сервер с
   каталогом длиннее 48 байт не стартует — имя канала не влезло бы в 64 байта
   ответа; 0.10: лобби в частном пространстве имён — префикс пуст, канал
   создаётся в том же пространстве, §b.4), создаёт безымянное событие «зарегистрирован» и
   `AutoServer(канал)`; пишет ответ `STATUS_OK` в кольцо A и явно сигналит
   `S2C_DATA` (`DispatchCore::handle_lobby_client`). Ошибка на этом шаге → ответ
   `STATUS_REJECTED` + `S2C_DATA` (`DispatchCore::reject`);
   0.10 — так же на ошибку разбора запроса (§d.1). 0.10: PID рукопожатия
   лобби (`client_pid`, самоописание пира; сверенный — только при ненулевом `client_start_*`, совпавшем с процессом, §c.1) сервер
   запоминает как `ClientRegistration::handshake_pid` (шаг 9).
   Затем лобби **сразу** помечается отключённым (`mark_disconnected`:
   состояния `IDLE`, кольца не трогаются), **0.10 — `NtSetEvent(S2C_DISCONNECT)`**
   (сессия лобби закрыта, R14: сначала `IDLE`, потом `DISCONNECT`; и после
   ответа, и когда ответа не будет — таймаут чтения, ошибка, остановка
   сервера) и ждёт следующего клиента.
7. **Клиент** ждёт ответ до `response_timeout` (5 с; prof-shm —
   1 с) **без нарезки** (0.9). 0.10 — набор `[S2C_DATA, S2C_DISCONNECT,
   процесс сервера (если PID получен)]`: на каждом пробуждении сначала
   читается кольцо (ответ, записанный до ухода сервера, не теряется); кольцо
   пусто после `DISCONNECT` при `server_state != SERVER_READY` или сменившемся
   `generation` — `NotConnected`, после смерти процесса сервера — `PeerDied`;
   `DISCONNECT` при живой сессии — устаревший, поглощается. Читает, закрывает лобби —
   **уход L** (Drop клиента, ревизия 3, R15): если `generation == G`
   (Acquire) — `CAS client_state: SERVER_READY → IDLE` (не store, не
   затирает `CLIENT_HELLO` следующего), `handshake_state = IDLE` в
   заголовках, затем `NtSetEvent(S2C_DISCONNECT)`; `generation != G` —
   сессия уже не наша (сервер отключил клиента, claim отнят, канал у
   следующего), ничего из этого; `client_pid` не трогается — PID уже
   забран сервером; затем **всегда последним** `CAS slot_claim: T → 0`, и
   только потом отпускает мьютекс. Клиент, уже узнавший об уходе сервера
   (`DISCONNECT`, смерть процесса сервера) или о смене сессии (`generation
   != G`), — только освобождение claim, без ответного `DISCONNECT`.
   Разрыв по собственной ошибке клиента (`Corrupted` при чтении или записи,
   отказ ожидания) — НЕ «узнал об уходе»: сервер ещё в сессии, клиент
   ОБЯЗАН выполнить полный уход L (так и в канале Auto/Dispatch, и в слоте
   Multi).
   **Нативный сервер лобби ОБЯЗАН сигналить `S2C_DATA` после записи
   ответа** (и `STATUS_OK`, и `REJECTED`).
8. **Клиент** подключается к каналу `channel_name` обычным handshake
   (шаг 3a, `channel_timeout` 10 с; prof-shm — 2 с). Ревизия 2: канал
   **не переподключается** — ни после разрыва, ни при неудаче ПЕРВОГО
   подключения (раньше Rust-клиент повторял его раз в 250 мс вечно, если
   сервер уже снёс канал): неудача → `on_error` + `on_disconnect_reason(Error)`,
   клиент завершён. Сервер, отключивший клиента сразу после рукопожатия
   (например, отказ из `on_client_connect`), теперь гарантированно
   доставляет ему записанное до `DISCONNECT` (шаг 3a п. 6–7).
9. **Сервер**: worker канала (`AutoServer`) принимает handshake; в **его же
   потоке** до чтения первого сообщения выполняется регистрация клиента в
   карте и `on_client_connect` (`AutoProxyHandler::on_connect`), затем
   взводится «зарегистрирован».
   **0.10 — сверка согласованности PID (не аутентификация, §h п. 6)**.
   PID рукопожатия лобби считается *сверенным* (`handshake_pid`, шаг 6),
   только если пир сообщил PID **и ненулевое** `client_start_*`, и они
   совпали с живым процессом; иначе (без PID, время `0`, нет прав, время не
   совпало) — не сверен. Сверен — пир канала ОБЯЗАН сообщить тот же PID **и
   ненулевое** время создания, совпадающее с процессом (закрыто по
   умолчанию); иначе (другой PID, канал без PID или с временем `0`) канал
   закрывается **до** `on_client_connect` (`DISCONNECT` клиенту,
   `on_error(Some(id), HandshakeFailed)` обработчику). Не сверен — сверки
   нет, как до 0.10. Нативный клиент ОБЯЗАН проходить лобби и канал одним
   процессом и сообщать PID и время создания в обоих рукопожатиях или ни в
   одном. Поток ожидания (`handle_lobby_client`) ждёт `{зарегистрирован, стоп}`
   до `channel_connect_timeout` (30 с); клиент не пришёл или сервер
   остановлен → канал останавливается.

### d.3 Гарантии Dispatch-сервера 0.9 и их природа

| Гарантия | Как обеспечена | Видно ли пиру |
|---|---|---|
| Лобби существует к возврату `start`; занятое имя — ошибка | `SharedServer::start` в `start`, а не в фоне | Да, косвенно: второй сервер падает, маяк первого не трогается |
| `on_client_connect` строго до первого `on_message` клиента | Регистрация — в `on_connect` worker-а канала; тот же поток затем читает кольцо (`process_receive_queue` идёт после `on_connect`, `server_worker` в `src/auto/mod.rs`) | **Нет** — чисто серверное поведение. Нативный Dispatch-сервер ОБЯЗАН дать ту же гарантию своим обработчикам |
| После отключения клиента его сообщения обработчику не доставляются | `disconnected` общий у прокси и карты; `is_active()` | Нет |
| Последние сообщения пира (например, `BYE`) читаются **до** колбэка разрыва | `drain_remaining` перед `on_disconnect_reason` и при `DISCONNECT`, и при смерти процесса (`src/auto/mod.rs`, `server_worker`/`client_worker`) | Да, семантически: отправитель может рассчитывать, что «последнее слово» дойдёт, если оно записано **до** `DISCONNECT`. Ревизия 2: в том числе когда сервер отключает клиента **сразу после рукопожатия** (из `on_client_connect`) — клиент засчитывает рукопожатие по `generation` (R15) и дочитывает кольцо |
| `stop()` из колбэка обработчика не блокируется (ревизия 2) | Метка потока сервера (thread-local): вызов из потока лобби/колбэка канала — отложенная остановка без join | Нет |
| Очередь, принятая до `stop()`/`disconnect_client`, дописывается в кольцо до закрытия канала (сколько влезет, без ожидания места) | Хвост `server_worker`/`client_worker` (`Worker::flush`, `src/auto/mod.rs`), потом Drop → `NtSetEvent(S2C_DISCONNECT)` | Да: порядок «данные → `DISCONNECT`» в памяти |
| `stop()` не ждёт `channel_connect_timeout` зависших клиентов | Событие остановки в наборах лобби/ожидания | Нет |

---

## e. Событийная модель (вместо `poll_timeout` 50 мс)

### e.1 Наборы ожидания Rust 0.9 (`poll_timeout = None` по умолчанию)

| Поток | Набор `NtWaitForMultipleObjects` (индекс: объект) | Таймаут |
|---|---|---|
| Auto-сервер, **подключён** (`server_worker` → `Worker::handle_wait`) | 0 `S2C_DISCONNECT` · 1 `C2S_DATA` · 2 `S2C_SPACE` · 3 процесс клиента (если PID получен и процесс открыт) · последний — `wake` (безымянный) | `poll_timeout` (`None`) |
| Auto-клиент, подключён (`client_worker` → `Worker::handle_wait`) | 0 `S2C_DISCONNECT` · 1 `S2C_DATA` · 2 `C2S_SPACE` · 3 процесс сервера (если есть) · последний — `wake` | `None` |
| Auto-сервер, **не подключён** (`server_worker`) | 0 `C2S_CONNECT_REQ` · 1 `wake` | `None` |
| Auto-клиент, между попытками (`client_worker`) | `wake` | `reconnect_delay` (250 мс) — **намеренно периодически** |
| Лобби Dispatch (`DispatchCore::worker_loop`) | 0 `C2S_CONNECT_REQ` · 1 стоп сервера | `None` |
| Чтение регистрации (`DispatchCore::handle_lobby_client`) | 0 `C2S_DATA` лобби · 1 стоп | остаток `lobby_timeout` (5 с) |
| Ожидание подключения к каналу (`DispatchCore::handle_lobby_client`) | 0 «зарегистрирован» · 1 стоп | `channel_connect_timeout` (30 с) |
| Клиент лобби, ответ (`lobby_register`, `src/dispatch/mod.rs`) | 0 `S2C_DATA` лобби · 1 `S2C_DISCONNECT` лобби (0.10) · 2 процесс сервера (0.10, если есть) | остаток `response_timeout` (5 с) |
| Worker канала Dispatch | как Auto (сервер/клиент) | `None` |
| Чтение регистрации (0.9 дополнение) | + 2 процесс клиента лобби (если PID получен) | остаток `lobby_timeout` |
| Клиент лобби, мьютекс (0.9) | `<B>_lock` (Mutant) | `lobby_timeout` |
| Multi-сервер (0.9; 0.10 — один поток-диспетчер на порту завершения, §i.2) | по слотам: свободный — `C2S_CONNECT_REQ`; подключённый — `S2C_DISCONNECT`, `C2S_DATA`, процесс клиента; пробуждение — пост в порт (0.9 — `wake` группы) | ближайший дедлайн (§i.3) или `None` |
| Multi-клиент, подключён (0.9, §i.4) | 0 `S2C_DISCONNECT` · 1 `S2C_DATA` · 2 процесс сервера (если есть) · последний — `wake` | `None` |
| Multi-клиент, между попытками | `wake` | `retry_delay` (250 мс) — только вне подключения |
| `wait_for_space` (0.9, §i.6) | 0 `SPACE` · 1 `S2C_DISCONNECT` · 2 процесс пира (если есть) | остаток таймаута вызывающего, одним ожиданием |
| `poll_client`/`poll_server` (синхронный читатель; 0.10 — с `DISCONNECT`, §i.6) | 0 `DATA` направления · 1 `S2C_DISCONNECT` · 2 процесс пира (если есть) | остаток таймаута вызывающего, одним ожиданием (устаревший `DISCONNECT` — ещё одно с остатком) |
| Клиент Auto/Dispatch/Multi, ожидание `S2C_CONNECT` (ревизия 2) | 0 `S2C_CONNECT` · 1 `wake` (своё; остановка прерывает, `send` — нет) | `connect_timeout` / `channel_timeout` / `slot_timeout`; `SharedClient::connect` — только `S2C_CONNECT` |

Приоритеты: при одновременном сигнале NT отдаёт меньший индекс —
`DISCONNECT` важнее данных, данные важнее смерти процесса, смерть важнее
`wake`. Поэтому на `DISCONNECT` и на смерть процесса worker **дочитывает**
кольцо до колбэка (иначе данные, взведённые вместе с `DISCONNECT`, терялись
бы).

### e.2 Кто сигналит и когда (правила, обязательные для любой стороны)

Все правила ниже существовали и в 0.8; в 0.9 Rust **перестал подстраховывать**
их опросом.

1. **`DATA` (направление записи)** — протокол «публикация → перепроверка»
   (Dekker; 0.10, `RingBuffer::publish`/`confirm_empty` в `src/ring.rs`;
   сигнал — `SharedServer::send_to_client`/`try_send_to_client`,
   `SharedClient::send_to_server`/`try_send_to_server`). Обе половины
   ОБЯЗАТЕЛЬНЫ:
   - **Писатель**, записав кадр в позицию `W` (прежний `write_pos`; с PAD —
     позиция PAD, §c.4): данные кадра → `message_count += 1` →
     `store(write_pos, W + need, release)` (`need` — кадр или PAD + кадр,
     одна запись) → **полный барьер** (`atomic_thread_fence(memory_order_seq_cst)`;
     на x86 — `mfence` или любая `lock`-инструкция; одного `release` мало:
     TSO разрешает чтению обогнать предшествующую запись) → `R =
     load(read_pos, acquire)` → если `R == W` — `NtSetEvent(DATA)`.
     `R == W` значит «читатель выбрал всё, что было до этого кадра, и мог
     уснуть». Решение принимается **только** так: `message_count` (его
     прежнее значение) на сигнал не влияет — читатель уменьшает счётчик
     после сдвига `read_pos`, и правило 0.9 «`prev message_count == 0`»
     теряло пробуждение (п. 24 «Изменений 0.10»).
     `message_count` — подсказка, а не истина: его пишет недоверенный пир, и
     он законно отстаёт на 1 (читатель уменьшает его после CAS `read_pos`).
     Поэтому писатель ОБЯЗАН применять лимит `max_messages` (вытеснение или
     отказ «полно») только при `write_pos − read_pos > 0`: пустое по
     позициям кольцо принимает любой кадр, помещающийся по байтам, при любом
     значении счётчика — ни «порчи», ни «полно» из-за счётчика (иначе при
     `max_messages = 1` законная гонка даёт разрыв). Счётчик писатель не
     исправляет.
   - **Читатель**, увидев пустоту (`read_pos == write_pos == W'` по
     снимку), **до** сна на `DATA`: **полный барьер** → `load(write_pos,
     acquire)`; значение ≠ `W'` — в кольце новый кадр, читать снова, а не
     спать. Сдвиг `read_pos` читателем (его CAS) предшествует этому барьеру
     в порядке программы. На x86 требованию удовлетворяет и `lock cmpxchg`
     самого CAS `read_pos`, выполненный **до** загрузки снимка `write_pos`
     (`lock`-инструкция — полный барьер, снимок тогда уже «после
     барьера»), — но только если компилятор не переставит загрузку выше
     CAS; переносимо (модель C11) — явный SeqCst-фенс и перечитывание, как
     в Rust.
   - Два полных барьера дают: хотя бы одна сторона видит запись другой —
     либо читатель при перепроверке видит новый `write_pos`, либо писатель
     видит `read_pos == W` и сигналит. Сигналить чаще — можно (лишний
     `DATA` безвреден: читатель получит `QueueEmpty` и снова уснёт), реже —
     нельзя.
2. **`SPACE` (направление, из которого читают)** — читатель после **каждого**
   успешного чтения, если выполнено **любое** из двух
   (`SharedServer::receive_from_client`, `SharedClient::receive_from_server`):
   - (а) кольцо опустело: `write_pos == read_pos` после чтения (0.10 —
     по позициям, не по `message_count`; Rust проверяет через `is_empty` с
     той же перепроверкой после барьера, что и в п. 1; лишний `SPACE`
     безвреден);
   - (б) `take_space_waiter()` вернул `true` (`RingBuffer::take_space_waiter`):
     `fence(SeqCst)` → `load(space_waiter)`; если ≠ 0 и свободно
     `messages > 0` и `bytes ≥ space_waiter` → `CAS(space_waiter: frame → 0)`;
     сигнал только при успешном CAS.

   Писатель без потерь ставит заявку `space_waiter = need(W,
   frame_size(len))` (0.10: кадр — заголовок `u32 len` + `u32 flags` и
   payload, дополненные до кратного 8, — плюс PAD до конца кольца, если кадр
   там не помещается, §c.4; `W` — его позиция записи, пока он ждёт, она не
   меняется), делает `fence(SeqCst)` и
   **перепроверяет** место до сна (Dekker-пара с (б),
   `RingBuffer::arm_space_waiter`/`wait_for_space`); снимает заявку
   (`store 0`) на первой успешной записи.
3. **Событие «место освободилось» для колбэков** (`on_space_available`) — это
   тот же ядерный `SPACE`. Worker Auto на `SPACE` **сначала** дописывает свою
   очередь в освободившееся место (`process_send_queue`), и **только потом**
   зовёт `on_space_available` (`Worker::on_space` в `src/auto/mod.rs`). В
   Dispatch это пробрасывается как `DispatchClientHandler::on_space_available()`
   / `DispatchHandler::on_space_available(client_id)`.
   Важно: `try_send` Auto отвечает `QueueFull` по **очереди** worker-а
   (`max_send_queue`), не по кольцу. Если очередь переполнилась, пока worker
   ещё не записал её в кольцо, заявки `space_waiter` может не быть — тогда
   разбудит **только** правило (а) «кольцо опустело». Поэтому (а) ОБЯЗАТЕЛЬНО,
   даже если (б) реализовано.
4. **`CONNECT_REQ`** — клиент после `CLIENT_HELLO`; ревизия 3 — и клиент,
   проигравший захват claim-а (C1), если `server_state != SERVER_READY`
   («толчок», §i.3). **`S2C_CONNECT`** — сервер после `complete_handshake`,
   **только** если его CAS `client_state: CLIENT_HELLO → SERVER_READY` удался
   (R16). `NtResetEvent(S2C_DISCONNECT)` сервер делает **до** фиксации
   (ревизия 3, S4; 0.9 и ревизия 2 — после, непосредственно перед
   `S2C_CONNECT`): сигнал прошлой сессии иначе мгновенно «отключил» бы
   нового клиента, а сброс после фиксации съедал прощальный сигнал клиента,
   уже засчитавшего подключение по проигранному отзыву и ушедшего. Клиент
   делает `NtResetEvent(S2C_CONNECT)` **до** своего `CLIENT_HELLO` и
   засчитывает сигнал, только наблюдая фиксацию своей заявки (R15, §d.2
   шаг 3a п. 10).
5. **`S2C_DISCONNECT`** — штатное закрытие любой стороны **после** записи
   последних сообщений: клиент (Drop, `src/client.rs`, ревизия 3) — только
   пока `generation == G` своей сессии: `CAS client_state: SERVER_READY →
   IDLE`, `handshake_state = IDLE` в заголовках, затем сигнал (`client_pid`
   не трогается); затем всегда `CAS slot_claim: T → 0` последним (R18); сервер (Drop,
   `src/server.rs`) — состояния `IDLE`, сигнал только если был
   подключён. **Локальный разрыв по ошибке** (0.10, final review I1/I2) —
   тоже уход с сигналом, а не молчаливая отметка «отключён»: клиент,
   разорвавший сессию из-за `Corrupted` или отказа ожидания, выполняет
   полный уход L (как выше); сервер — состояния `IDLE`, затем
   `S2C_DISCONNECT` (`SharedServer::disconnect_local`; так же Multi
   `force_disconnect` и конец сессии лобби, §d.2 шаг 6). Без сигнала только
   уход стороны, уже узнавшей об уходе пира (его `DISCONNECT`, смерть его
   процесса) или о смене сессии (`generation != G`). Кольца при закрытии **не** сбрасываются (их сбрасывает только
   следующий `complete_handshake`) — приёмник успевает дочитать. Порядок
   «состояние `IDLE` → потом `DISCONNECT`» ОБЯЗАТЕЛЕН (0.9): по нему
   `wait_for_space` отличает настоящее отключение от устаревшего сигнала
   (§i.6), Multi-сервер в `disconnect_client` делает так же. Сервер,
   сбрасывая состояние сессии, пишет `client_state` только CAS
   `SERVER_READY → IDLE` (не затирает `CLIENT_HELLO` следующего клиента).
6. **Процесс** — PID в `server_pid` (сервер, при создании секции) и
   `client_pid` (клиент, **до** `CLIENT_HELLO`); без этого смерть пира не
   видна **никогда** (см. e.4, R6).

### e.3 Новые события отправки (send wake) — внутренние

`wake` Auto (`Endpoint::spawn` создаёт, `enqueue`/`Endpoint::stop` взводят; `src/auto/mod.rs`) — безымянное SynchronizationEvent,
взводится после постановки команды в канал (`enqueue`), в `stop` и `Drop`.
Стоп Dispatch-сервера и «зарегистрирован» — безымянные NotificationEvent.
Другому процессу они не видны и по имени не открываются. Нативной синхронной
реализации аналог не нужен; при собственном фоновом потоке ЖЕЛАТЕЛЬНО то же
решение (безымянное событие, не таймер).

### e.4 Требования к пиру, без которых Rust 0.9 ждёт вечно

Rust 0.9 с `poll_timeout = None` не имеет страховочного таймаута в
worker-ах, лобби и отправителе prof-shm. Нативный пир **ОБЯЗАН**:

| # | Требование | Что зависнет без него (Rust 0.9) | 0.8 |
|---|---|---|---|
| R1 | Писатель: после публикации `write_pos` — полный барьер, чтение `read_pos`, `NtSetEvent(DATA)` при `read_pos ==` прежнему `write_pos`; читатель перед сном на `DATA` — полный барьер и перепроверка `write_pos` (0.10, §e.2 п. 1; правило 0.9 «`prev message_count == 0`» теряет пробуждение) | Worker-приёмник спит, пока не придёт другое событие; данные «висят» в кольце | Забирались по тику 50 мс |
| R2 | Читатель: `NtSetEvent(SPACE)` после чтения, опустошившего кольцо | Отправитель без потерь (Auto `try_send`, Dispatch `try_send`/`try_send_to`, prof-shm `send`) спит без конца после `QueueFull`; `on_space_available` не приходит | Worker перепробовал запись по тику; prof-shm продюсер (фаза 4) спал по 1 мс |
| R3 | Читатель: снятие заявки `space_waiter` (CAS) + `NtSetEvent(SPACE)`, когда места хватает | Не зависание, а задержка: писатель проснётся только когда кольцо опустеет полностью (R2) | То же + тик |
| R4 | Клиент: `NtSetEvent(C2S_CONNECT_REQ)` после `CLIENT_HELLO` | Handshake не начнётся (так было и в 0.8: сервер и тогда смотрел только событие) | — |
| R5 | Сервер: `NtSetEvent(S2C_CONNECT)` после handshake | Клиент получит `Timeout` по `connect_timeout` (ограничено) | — |
| R6 | Сервер пишет PID в `server_pid`; клиент — в `client_pid` до `CLIENT_HELLO` | Смерть пира не будет замечена **никогда** (и в 0.8 тоже, но 0.8 хотя бы просыпался). Для prof-shm (ревизия 2): продюсер после отказа ждёт маяк поколения и `ProcessExit` вьюера **без таймаута**, только если `ProcessExit` открылся; без PID (или нет прав, другая сессия) — ждёт маяк поколения не дольше `BACKOFF_MAX` = 30 с, затем снова к маяку лобби (лобби нет — алгоритм «упавшего вьюера», есть — новая попытка регистрации). Упавший без PID вьюер стоит продюсеру до 30 с, а не вечного сна | — |
| R7 | Штатное закрытие: `NtSetEvent(S2C_DISCONNECT)` **после** записи последних данных; разрыв по своей ошибке (`Corrupted`, отказ ожидания) — тоже закрытие с сигналом (0.10, §e.2 п. 5) | Rust-сторона не узнает о закрытии (без R6 — вообще никогда) | Так же не узнавала |
| R8 | Клиент лобби: `NtSetEvent(C2S_DATA)` после записи запроса | Сервер 0.9 не прочитает запрос → `Timeout` через `lobby_timeout` (5 с), клиенту — `Timeout` ответа | Читалось по тику 50 мс |
| R9 | Сервер лобби: `NtSetEvent(S2C_DATA)` после записи ответа (`OK` и `REJECTED`) | Клиент 0.9 ждёт до `response_timeout` и получает `Timeout` (5 с; prof-shm — 1 с) | Читалось по тику 50 мс |
| R10 | Маяк — NotificationEvent; сервер `raise` после создания лобби, `lower` до закрытия | Клиенты на маяке спят вечно (нет `raise`) или просыпаются по одному (автосброс) | Маяка не было |
| R11 | Нативный Dispatch-клиент подключается к выданному каналу в пределах `channel_connect_timeout` (30 с) | Канал будет снесён, подключение — `Timeout` | То же |
| R12 (0.9) | Клиент лобби: захватить мьютекс `<B>_lock` до `CLIENT_HELLO`, держать до отключения от лобби, освободить тем же потоком (§d.2 п. 3) | Не зависание: два одновременных клиента снова могут пройти рукопожатие оба, писать в одно кольцо и прочитать чужой ответ (§h.2) — `Timeout`/битый ответ у проигравшего | Мьютекса не было |
| R13 (0.9) | Клиент Multi: `slot_owner_pid` = свой PID сразу после CAS захвата, `client_pid` = PID до `CLIENT_HELLO`, сигнал `C2S_CONNECT_REQ` сразу после `HELLO`; при уходе — `S2C_DISCONNECT` **до** снятия claim (§i.4) | Без PID смерть клиента после рукопожатия замечается только при пробуждении сервера по другой причине (§i.5); без `CONNECT_REQ` захват освободится лишь по дедлайну 10 с | Опрос 50 мс и проверка PID раз в 3 с маскировали |
| R14 (0.9) | Уходя штатно, сначала перевести своё состояние handshake в `IDLE`, потом `NtSetEvent(S2C_DISCONNECT)` | `wait_for_space` пира примет сигнал за устаревший и продолжит ждать места (до своего таймаута / смерти процесса) | `wait_for_space` просыпался по срезу 50 мс, но о разрыве не узнавал |
| R15 (рев. 3) | Клиент (любой именованный канал, §d.2 шаг 3a): `CAS slot_claim: 0 → T` до всего остального (Multi — уже захвачен), проигрыш — толчок и `AlreadyConnected` без записей; `g0 = generation` до заявки; `NtResetEvent(S2C_CONNECT)` до заявки; заявка — `CAS client_state: IDLE → CLIENT_HELLO`, после неё сверка `slot_claim == T`; принятым считать `S2C_CONNECT` при `client_state == SERVER_READY` или `IDLE` и `generation != g0` (при `CLIENT_HELLO` — устаревший); отзыв — только `CAS CLIENT_HELLO → IDLE` с разбором наблюдённого (`SERVER_READY` или `IDLE` при `generation != g0` — подключён); Drop — записи состояния только при `generation == G`: `CAS SERVER_READY → IDLE`, затем `DISCONNECT`; освобождение claim — последним (§d.2 шаг 3a, шаг 7) | Два ждущих клиента засчитывают одно рукопожатие — два писателя в одном SPSC-кольце; отзыв снимает чужую заявку — клиент «подключён к никому»; уход «призрака» рвёт следующую сессию; принятие по устаревшему `S2C_CONNECT` до фиксации; сервер, отключивший клиента сразу после рукопожатия, для него «не отвечал» (откат, прощальные данные теряются) | Гонка существовала (Concern 1 ревью Task 18) |
| R16 (рев. 3) | Сервер (§d.2 шаг 4): S1 — claim и самоописание без потребления; S2 — заявку подтверждённо умершего держателя claim-а снять CAS-ом до фиксации, без `generation` и `S2C_CONNECT`, перепроверив перед CAS `slot_claim == T` (сменился — S2 пропустить); S3 — кольца, `generation`, `server_state`; S4 — `NtResetEvent(S2C_DISCONNECT)` **до** фиксации; S5 — фиксация `CAS client_state: CLIENT_HELLO → SERVER_READY`, не удался — `IDLE`, без `S2C_CONNECT`; S6 — потребить самоописание и взять claim сессии, только застав владельца в сессии (`client_state == SERVER_READY`), при иной зафиксированной заявке — наблюдать её процесс; S7 — `S2C_CONNECT` (не удался — закрыть сессию: `IDLE`, затем `S2C_DISCONNECT`, п. 29) | Клиент, отозвавший заявку в момент ответа, оставляет сервер подключённым к никому; сброс `DISCONNECT` после фиксации съедает прощальный сигнал быстро ушедшего клиента (сервер ждёт вечно); потребление без сверки забирает самоописание следующего клиента (его сессия без наблюдения, claim сессии — чужой) | Так же |
| R17 (рев. 2) | Multi: сервер в `disconnect_client`/`stop` claim **не** освобождает (только `IDLE` → `DISCONNECT`); клиент, получив `DISCONNECT` от сервера, **ОБЯЗАН** снять claim сам (`CAS token → 0`); при штатном уходе клиента сервер снимает claim только `CAS <claim сессии> → 0`; клиент на каждом пробуждении сверяет `generation` (§i.2, §i.4) | Без снятия клиентом слот занят до дедлайна `RESERVE_TIMEOUT` (10 с). Сервер, освобождающий claim сам, открывает гонку: новый клиент захватывает слот, его рукопожатие сбрасывает `DISCONNECT` прежнего — два клиента на одном SPSC-кольце | Гонка существовала |
| R18 (рев. 3) | Single-client канал (не Multi-слот, не anonymous): исключительная заявка — клиент `CAS slot_claim 0 → T` (токен уникален и внутри процесса, §i.1), `CLIENT_HELLO` — только CAS из `IDLE`, освобождение claim — последним шагом отзыва и ухода (лобби — до `NtReleaseMutant`). Сервер: claim **не** освобождает при штатном уходе клиента и при своём отключении клиента (снимает клиент; «не заметившего» — дедлайн); освобождает claim умершего в сессии сразу (`mark_disconnected`: handle процесса клиента с подтверждённым временем сигнален → `CAS <claim сессии> → 0` после сброса состояния) и снимает заявку умершего до фиксации (S2); протухший захват без заявки — **только по толчку** проигравшего клиента: на каждом `CONNECT_REQ` без `HELLO` у неподключённого сервера — `observe_idle_claim` (§i.3), без таймера | Два клиента на одном SPSC-кольце, сессия-призрак, разрыв чужой сессии уходящим «призраком»; без освобождения claim умершего — канал отдаёт `AlreadyConnected` до дедлайна (10 с с первого толчка); без толчка и дедлайна — до перезапуска сервера | claim в single-client не использовался (внешняя сериализация — только мьютекс лобби R12) |

Обратное направление (что нативной стороне ждать от Rust 0.9): Rust
выполняет R1–R18 всегда (R3 — с 0.8, R6 — с 0.8, R10 — с 0.9 при
использовании `Beacon`, R12–R14 — с 0.9, R15–R17 — с ревизии 2, R15/R16 в
редакции ревизии 3 и R18 — с ревизии 3).

### e.5 Оставшиеся таймауты и таймеры в 0.9

- **Multi-client** (0.9, §i): опроса нет. Таймауты — только дедлайны
  (протухший захват: `RESERVE_TIMEOUT` = 10 с от первого наблюдения; разовая
  проверка брошенного рукопожатия через 2 с после handshake). Клиент вне
  подключения повторяет захват раз в `retry_delay` = 250 мс (сервера нет /
  нет свободного слота) — как `AutoClient::reconnect_delay`, в простое
  подключённого клиента не участвует.
- **`SharedServer/SharedClient::wait_for_space`** (0.9, §i.6): одно ожидание
  на весь таймаут вызывающего, без срезов. Без событий вовсе (anonymous-сервер:
  объектов ядра нет) — сон `SPACE_POLL_INTERVAL = 1 мс`, только пока
  вызывающий сам блокирован в `wait_for_space`.
- **`wait_for_client_noevent`** (anonymous-сервер): опрос `client_state`
  раз в 1 мс — единственный способ без объектов ядра, только внутри вызова
  с таймаутом вызывающего.
- **`AutoClient`**: переподключение раз в `reconnect_delay` = 250 мс, каждая
  попытка ждёт `S2C_CONNECT` до `connect_timeout` = 2 с (намеренно
  периодически; `DispatchClient` канал не переподключает — ревизия 2: и
  после неудачного первого подключения). Ревизия 2: это ожидание (как и
  `channel_timeout` Dispatch и `slot_timeout` Multi) прерывается `wake`
  остановки — `stop`/Drop не ждут таймаута рукопожатия.
- **`DispatchServer`**: `LOBBY_RETRY_DELAY = 250 мс` — только после ошибки
  пересоздания лобби (`DispatchCore::worker_loop`); дедлайны `lobby_timeout`
  (5 с), `channel_connect_timeout` (30 с); клиент — `lobby_timeout` (5 с),
  `response_timeout` (5 с), `channel_timeout` (10 с).
- **Single-client канал (ревизия 3, R18)**: таймера нет — протухший
  захват (`RESERVE_TIMEOUT` = 10 с от первого наблюдения) проверяется только
  на толчке проигравшего клиента (`CONNECT_REQ` без `HELLO`); ожидание
  сервера остаётся `[CONNECT_REQ, wake]` без таймаута.
- `poll_timeout = Some(t)` в Auto/Dispatch — необязательная страховка
  (просыпаться не реже `t`), по умолчанию выключена.

---

## f. Изменения Rust API (кратко)

| API | Суть | Аналог в C/C++ |
|---|---|---|
| `AutoOptions/DispatchOptions/DispatchClientOptions::poll_timeout: Option<Duration>` (было `Duration`) | `None` — только события | Не нужен (синхронная реализация); если есть фоновые потоки — ждать без таймаута, но выполнять e.2 |
| `Beacon::{open, unnamed, raise, lower, clear_if_stale, wait, wait_any, is_raised}` | Маяк сервера (`clear_if_stale` — 0.10, клиент вместо `lower`) | **Желателен** `open/raise/lower/clear_if_stale/wait/wait_any/is_raised`, если участвуете в обнаружении (обязателен для prof-shm); `unnamed` — желателен |
| `ProcessExit::{open, pid, has_exited}`, трейт `Waitable`, `xshm::wait_any` | Ждать маяки и выход процессов вместе | Желателен |
| `DispatchClient::lobby_exists(name)` | Лобби есть? (`NtOpenSection` + размер) | Желателен (нужен для алгоритма маяка) |
| `DispatchClientHandler::on_space_available()`, `DispatchHandler::on_space_available(id)` | Проброс `SPACE` | Желателен, если у вас есть колбэки |
| `AutoHandler::on_space_available` | Документирован порядок «сначала дописать очередь» | Желателен |
| `DispatchServer::start` — ошибка на занятом имени | | **Обязателен** (поведение сервера) |
| `DispatchServer::disconnect_client` — дописывает принятое до вызова | | **Обязателен** порядок «данные → DISCONNECT» |
| `MultiOptions/MultiClientOptions::poll_timeout: Option<Duration>` (было `Duration` 50 мс), `MultiClientOptions::retry_delay` (250 мс) | `None` — только события и дедлайны (§i) | Не нужен; требования к пиру — R13/R14 |
| `MultiHandler::on_client_disconnect_reason`, `MultiClientHandler::on_disconnect_reason` (по умолчанию зовут старые методы), `MultiServer::is_client_alive` | Причина разрыва (`Graceful`/`PeerDied`/`Local`/`Error`) | Желателен |
| `SharedServer/SharedClient::wait_for_space` → `Err(NotConnected)` при штатном уходе пира | §i.6 | Желателен |
| `SharedServer::poll_client*`/`SharedClient::poll_server*` (0.10) — `&mut self`; ждут и `DISCONNECT`: после дочитывания — `Err(NotConnected)`, сторона помечается отключённой (`PeerDied` — тоже) | §i.6, §e.1 | Желателен (синхронный читатель видит штатный уход, а не только таймаут); пиру — только R14 |
| `DispatchClient::server_pid()` (ревизия 2) | PID сервера из рукопожатия канала, сохраняется и после разрыва | Желателен (нужен prof-shm-продюсеру после отказа) |
| `DispatchServer::stop()` из колбэка (ревизия 2) | Отложенная остановка без join (иначе взаимная блокировка); полная — повторным `stop()`/Drop из чужого потока | Желателен, если у вас есть колбэки |
| `MultiServer::stop()` (ревизия 2) | `DISCONNECT` подключённым клиентам — сразу, а не при Drop последнего `Arc` | **Обязателен** (клиент иначе не узнает об остановке, R7) |
| `set_thread_start_hook(Option<fn()>)`, `thread_start_hook()` (26.09.2026) | Хук старта рабочих потоков, созданных текущим потоком (наследуется рабочими): служебная разметка потоков хостом (prof-shm — «тихие» потоки учёта памяти) | **Не нужен** — Rust-only, не пир-видимо: ни объектов ядра, ни раскладки, ни сигналов не меняет |

---

## g. Чек-лист для C/C++-реализации (порядок внедрения)

1. **Ревизия сигналов (e.2/e.4).** Писатель: `DATA` по протоколу
   «публикация → полный барьер → `read_pos == прежний write_pos`», читатель —
   полный барьер и перепроверка `write_pos` перед сном (R1, 0.10; тесты
   `ring::try_write_tests::{data_signal_survives_reader_draining_concurrently,
   reader_recheck_sees_frame_published_after_empty_snapshot}`,
   `tests/wakeup.rs`). Читатель: `SPACE` при опустошении (R2) и по заявке
   `space_waiter` с CAS и `SeqCst`-фенсом (R3). Тесты-аналоги:
   `tests/backpressure.rs::{cross_process_lossless_backpressure,
   wait_for_space_is_woken_by_reader_event, not_connected_has_no_space}`,
   `ring::tests::{space_waiter_is_taken_only_when_enough_space,
   wait_for_space_polls_without_event, lossless_producer_faster_than_consumer}`,
   `tests/ordering.rs::test_message_count_before_write_pos`.
1a. **PAD-кадры (§c.4, 0.10) — на обеих сторонах.** Писатель: PAD на остаток
   кольца, если кадр не помещается до конца; место и `space_waiter` — под
   PAD + кадр; одна публикация `write_pos`. Читатель: пропуск PAD без
   доставки, проверка «PAD ровно до границы, в опубликованных данных»,
   отказ на неизвестных битах `flags` и на кадре через границу. Перезапись
   вытесняет PAD без учёта. Тесты: `ring::tests::{payload_never_wraps,
   pad_frame_is_skipped_and_accounted, pad_at_u32_wrap_boundary,
   hostile_pad_frames_are_corrupted, read_with_reports_overrun}`,
   `ring::try_write_tests::{randomized_framing_matches_model,
   data_signal_on_eviction_by_bytes_that_empties_ring}`,
   `tests/zero_copy.rs`.
2. **PID в handshake (R6)**: `server_pid` сервером при создании секции,
   `client_pid` клиентом до `CLIENT_HELLO`, одноразовое потребление сервером
   (ревизия 3 — CAS в S6, только застав владельца в сессии),
   откат обнуляет. Тесты: `peer_tests::{old_client_interoperates_without_peer_watch,
   old_server_interoperates_without_peer_watch, stale_dead_pid_is_not_reported_as_peer_death,
   rollback_clears_announced_pid, disconnect_drops_peer_watch}`,
   `tests/peer_death.rs` (11 тестов).
3. **Закрытие**: сначала все данные, потом `S2C_DISCONNECT` (R7); приёмник
   на `DISCONNECT` и на смерть процесса **дочитывает кольцо** до колбэка.
   Тесты: `dispatch::tests::farewell_message_is_delivered_before_disconnect`,
   `tests/peer_death.rs::{auto_graceful_disconnect_reports_graceful,
   shared_graceful_disconnect_is_not_peer_death, dispatch_graceful_disconnect_reports_graceful,
   dispatch_local_disconnect_reports_local}`.
4. **Лобби**: клиент сигналит `C2S_DATA` после запроса (R8), сервер —
   `S2C_DATA` после ответа, включая `REJECTED` (R9). Сервер создаёт секцию
   лобби без `OBJ_OPENIF`, коллизия — ошибка старта. Тесты:
   `dispatch::tests::{second_server_on_same_lobby_fails_synchronously,
   dispatch_roundtrip, dispatch_multiple_clients,
   stalled_client_does_not_block_subsequent_registrations,
   stop_is_prompt_with_stalled_registration, channel_names_are_practically_unique}`.
5. **Dispatch-сервер (если есть)**: объявлять клиента (`on_client_connect`)
   в том же потоке, что читает его канал, **до** первого сообщения;
   не доставлять сообщения после отключения; при `disconnect_client`/`stop`
   дописать очередь в кольцо, потом закрыть. Тесты:
   `dispatch::tests::{messages_never_precede_client_connect,
   dispatch_disconnect_no_double_notify, stop_does_not_deadlock_after_client_disconnects,
   client_is_connected_false_after_server_disconnect, stop_synchronously_joins_worker}`.
6. **Маяк**: NotificationEvent `<B>_beacon` с `OBJ_OPENIF` (+ принять
   `STATUS_OBJECT_NAME_EXISTS`), дескриптор — своя политика (§b.2; до 0.10
   NULL DACL), `InitialState = FALSE`;
   `raise`/`lower`/`wait`/`wait_any`/`is_raised`; 0.10 — секция состояния
   `<B>_beacon_state`, публикация в `raise`, `clear_if_stale` клиента вместо
   `lower`; порядок сервера и алгоритм клиента §b.3; `lobby_exists`. Тесты:
   `beacon::tests::{waiters_wake_together_on_raise_and_lower_resets,
   wait_any_reports_which_beacon_fired, client_may_open_before_server,
   unnamed_beacons_are_private_and_waitable,
   stale_clear_racing_new_raise_keeps_beacon_raised,
   clear_if_stale_keeps_live_server, clear_if_stale_lowers_beacon_of_dead_server,
   corrupted_state_never_lowers_beacon}`; сценарий «маяк упавшего
   сервера» — `prof-shm/tests/live.rs::stale_beacon_is_lowered_and_the_producer_sleeps_until_a_viewer_appears`.
7. **Ожидание выхода процесса** (`ProcessExit`): удерживаемый handle,
   `SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION`, в том же
   `WaitForMultipleObjects`. Тесты: `beacon::tests::{process_exit_of_live_process_does_not_fire,
   process_exit_fires_when_process_ends}`.
8. **Отправитель без потерь поверх колбэка места**: после `QueueFull` спать
   до `SPACE`/колбэка, запомнив «номер события» **до** попытки (иначе
   потерянное пробуждение). Тест: `dispatch::tests::space_event_drives_lossless_sender`
   (очередь 8, 3000 × 30 КиБ), `tests/backpressure.rs::{dispatch_client_try_send_is_lossless,
   dispatch_server_try_send_to_is_lossless}`.
9. **Раскладка** — без изменений; сверить: `layout::tests::{layout_sizes_are_exact,
   ring_header_offsets_are_stable}`, `naming::tests::*`,
   `tests/ordering.rs::{test_invalid_magic_rejected, test_generation_on_reconnect}`.

10. **Multi-клиент (если есть)**: R13 и §i.4; тесты
    `multi::tests::{idle_server_and_client_do_not_wake_up,
    stale_claim_is_released_at_deadline_not_by_ticking,
    client_waiting_for_slot_gets_it_after_stale_claim_deadline, stop_is_prompt,
    disconnect_client_notifies_client_and_frees_slot,
    backlog_beyond_recv_batch_is_delivered_without_tick,
    clients_across_two_worker_groups}`,
    `tests/peer_death.rs::{multi_server_sees_client_death,
    multi_client_sees_server_death}`.
11. **Лобби под мьютексом (R12)**, канал в namespace лобби, проверка типа
    маяка: `dispatch::tests::{concurrent_registrations_are_serialized,
    channel_inherits_lobby_namespace_end_to_end,
    channel_namespace_prefix_follows_lobby}`,
    `win::tests::{named_mutex_serializes_and_survives_abandoned_owner,
    named_mutex_rejects_other_object_type,
    notification_open_rejects_synchronization_event}`,
    `beacon::tests::open_rejects_squatted_auto_reset_event`.
12. **Ревизия 2 (R15–R17)**: рукопожатие —
    `server::handshake_tests::{server_does_not_commit_a_withdrawn_hello,
    stale_connect_ack_does_not_confirm_the_next_client,
    client_sees_a_handshake_even_if_the_server_already_left}`,
    `client::tests::withdraw_hello_loses_only_to_a_committed_handshake`;
    отказ сразу после рукопожатия —
    `dispatch::tests::{refusal_in_on_client_connect_delivers_farewell_and_disconnect,
    farewell_message_is_delivered_before_disconnect,
    dedicated_channel_failure_ends_the_client}`; Multi —
    `multi::tests::{disconnect_client_keeps_claim_until_the_client_leaves,
    claim_of_a_kicked_client_that_never_leaves_is_released_at_deadline,
    stop_disconnects_clients_while_the_server_arc_is_alive,
    drop_during_slot_handshake_is_prompt}`; остановка во время
    рукопожатия — `auto::lossless_tests::stop_during_connect_is_prompt`; prof-shm —
    `prof-shm/tests/live.rs::revision_refusal_from_on_client_connect_is_a_rejection`.
    Ревизия 3 переписала R15/R16 — эталоны рукопожатия теперь п. 13.
13. **Ревизия 3 (R15/R16/R18, исключительная заявка)**: модель —
    `handshake_model::{handshake_model, handshake_model_finds_revision_two_defect,
    handshake_model_design_c10_accepts_before_commit,
    handshake_model_design_s6_loses_next_description}` (исчерпывающий
    перебор чередований двух клиентов и сервера, ≈1,9 млн состояний;
    варианты протокола ревизии 2 и дизайна без двух поправок обязаны
    нарушать инварианты); клиент —
    `client::tests::{loser_gets_already_connected_and_writes_nothing,
    same_process_competition_single_acceptance, withdraw_never_removes_foreign_hello,
    ghost_drop_does_not_tear_next_session, connect_async_loser_already_connected,
    connect_async_drop_releases_claim, connect_async_drop_after_commit_leaves,
    withdraw_hello_loses_only_to_a_committed_handshake, takeover_only_from_idle}`;
    сервер — `server::handshake_tests::{inflight_aba_accepts_by_ready,
    fast_leaver_disconnect_not_eaten, dead_hello_withdrawn_before_commit,
    dead_hello_withdrawal_rechecks_claim, dead_hello_withdrawal_does_not_clobber_takeover,
    dead_hello_of_other_pid_not_withdrawn,
    stale_claim_reclaimed_on_nudge_only}`;
    совместимость — `peer_tests::{legacy_client_without_claim_connects,
    client_with_forged_start_time_is_reported_dead}`; Auto/Dispatch/Multi —
    `auto::claim_tests::{auto_client_retries_after_already_connected,
    idle_server_with_foreign_claim_does_not_wake_up}`,
    `dispatch::tests::{lobby_takes_over_claim_of_abandoned_mutex,
    idle_lobby_with_foreign_claim_does_not_wake_up}`,
    `multi::tests::{multi_handshake_reuses_slot_token,
    idle_server_with_foreign_claim_does_not_wake_up}`; смерть —
    `tests/peer_death.rs::peer_death_releases_claim_immediately`; шторм —
    `tests/hostile.rs::{handshake_storm, handshake_storm_with_retry}` (принятий
    клиентами ровно столько, сколько сессий сервера; причины отключений —
    только `Graceful`; с повтором — 1600 полных рукопожатий).
14. **Атрибуты чужой секции (§c.5 п. 1, «Изменения 0.10» п. 31)**: на
    каждом открытии чужой секции (канал, заголовок слота Multi, существующее
    состояние маяка) — `NtQuerySection(SectionBasicInformation)` до
    отображения; отказ, если в `AllocationAttributes` есть хоть один из
    `SEC_RESERVE` (`0x04000000`), `SEC_IMAGE` (`0x01000000`), `SEC_FILE`
    (`0x00800000`), `SEC_NOCACHE` (`0x10000000`), `SEC_WRITECOMBINE`
    (`0x40000000`), либо нет `SEC_COMMIT` (`0x08000000`); прочие биты
    (`SEC_LARGE_PAGES` `0x80000000`, служебные) не проверять. Тесты-эталоны:
    `win::tests::{open_rejects_reserved_or_image_section,
    open_rejects_uncached_sections, own_section_attributes_pass_check,
    open_or_create_shares_section_and_rejects_reserved_squatter}`,
    `tests/hostile.rs::client_rejects_uncached_squatter` (клиент получает
    `Corrupted` сразу, без ожидания таймаута).

Прогон: `cargo test --all-features` (на 2026-09-25, рабочее дерево 0.9.0
после ревизии 2: 116 unit + 1 ignored, 7 backpressure, 5 multi, 4 ordering,
13 peer_death, 3 stress — зелёные, 5 полных прогонов подряд;
`farewell_message_is_delivered_before_disconnect` и
`refusal_in_on_client_connect_delivers_farewell_and_disconnect` — 30 из 30
под нагрузкой параллельного полного прогона). «Ноль пробуждений в простое»
доказывается счётчиками пробуждений worker-ов (только в `cfg(test)`):
`auto::tests::idle_worker_does_not_wake_up`,
`dispatch::tests::idle_dispatch_does_not_wake_up`,
`multi::tests::idle_server_and_client_do_not_wake_up` — за 1 с простоя
подключённого канала 0 пробуждений на каждой стороне; ожидание места —
`ring::try_write_tests::wait_for_space_wakes_on_{space,peer_death}_without_slicing`
(ровно одно ожидание в ядре).

**Межъязыковая проверка (0.10).** `examples/interop_peer.rs` — Rust-сторона
сценариев single-client канала: `interop_peer <сценарий> <имя>
[ключ=значение ...]`, в stdout только строки `READY`, `INFO <ключ>=<значение>`
(ошибки и причины — именами вариантов `ShmError`/`DisconnectReason`), `PASS`
или `FAIL <текст>`; коды выхода 0/1/2 (2 — ошибка аргументов), самотаймаут
60 с. Перечень сценариев, ключи, генератор кадров и правило эха — в шапке
файла. Нативная реализация переиспользует его как есть: `cargo build
--release --example interop_peer` (и `--target i686-pc-windows-msvc` для
32-битного пира), запуск из своих тестов, проверки — на своей стороне. Так
устроены оркестраторы xhook/xipc (`tests_static/test_ipc_interop_*.c`,
`scripts/run_interop.ps1` — все четыре пары разрядностей C ↔ Rust).

---

## h. Известные неоднозначности и риски

Пункты 1 и 2 **исправлены в 0.9** (п. 22 и 21 §a; пункт 2 для любого
single-client канала — ревизией 3, R18), пункт 3 — смягчён (п. 19),
пункт 4 — частично (п. 23: тип проверяется, но первый создатель по-прежнему
может занять имя). Текст ниже описывает поведение до исправления и остаётся
верным при смешении со старыми пирами.

1. **(Исправлено в 0.9, п. 22 §a; остаётся с сервером ≤ 0.8) Имя канала
   Dispatch не наследовало namespace лобби.** Сервер ≤ 0.8 называл канал 16
   hex-символами без префикса, то есть в `Local\` **сессии сервера**: лобби
   `Global\X` открывалось из другой сессии, а канал — нет, межсессионный
   Dispatch с таким сервером не работает. Сервер 0.9 выдаёт имя в namespace
   лобби (`Global\<hex>` и т.д., §d.2 шаг 6), и межсессионный Dispatch через
   `Global\` работает. Нативной реализации: клиент открывает канал **ровно
   по имени из ответа** (как Rust, любой версии) — тогда он совместим с
   серверами обеих версий; нативный сервер ОБЯЗАН выдавать имя в namespace
   лобби (п. 22).
2. **(До 0.9; со старыми клиентами — и сейчас) Нет взаимного исключения
   клиентов лобби.** `SharedClient::connect` пишет
   `CLIENT_HELLO` без проверки/CAS `client_state`; лобби — одноклиентский
   канал. Два клиента, пришедшие одновременно (например, продюсеры,
   разбуженные одним `raise` маяка), могут оба пройти handshake, стать двумя
   писателями одного SPSC-кольца, ждать одно автосбросное `S2C_DATA` и
   прочитать чужой ответ. Проигравший получает `Timeout`/битый запрос и
   повторяет (у prof-shm — с отступом). Поведение существовало и в 0.8, но
   маяк делает одновременный старт клиентов типичным. Нативная реализация
   должна вести себя так же (или лучше — но тогда это новый протокол).
   **Ревизия 3:** это и есть новый протокол — single-client канал (лобби,
   канал Dispatch, `SharedServer`/`AutoServer`) сериализует подключения сам
   исключительным claim-ом (R18), внешняя сериализация не требуется;
   проигравший получает `AlreadyConnected` сразу. Со старыми клиентами без
   claim — п. 10.
3. **`S2C_DISCONNECT` — одно автосбросное событие на оба направления.**
   Лишний/запоздавший сигнал остаётся взведённым до следующего ожидания
   любой стороной; с 0.9 его сбрасывает `complete_handshake` перед
   `S2C_CONNECT` (п. 19), с ревизии 2 его больше не может сбросить
   рукопожатие *чужого* клиента Multi (claim держит отключённый клиент,
   R17). Аналогично `S2C_CONNECT` (ревизия 2): клиент сбрасывает его до
   заявки (R15).
4. **Первый создатель маяка определяет его тип** (§b.3): любой процесс может
   заранее создать `<B>_beacon` как SynchronizationEvent или с жёстким DACL
   (тогда `EVENT_ALL_ACCESS` у остальных даст отказ). Модель безопасности
   до 0.10 — NULL DACL; с 0.10 DACL задаёт создатель (§b.2), по умолчанию —
   пользователь + SYSTEM: занять маяк заранее может только процесс того же
   пользователя (или с политикой `Everyone` — любой).
5. **Лобби «существует»**, пока на секцию есть хоть один handle — в том числе
   у клиента, застрявшего в регистрации: `lobby_exists` может вернуть `true`
   сразу после падения сервера, а новый сервер — не стартовать на этом имени.
6. **(0.10) Сверка PID канала Dispatch — не аутентификация.** PID и время
   создания процесса (`client_pid`, `client_start_*`) пир пишет в
   разделяемую память **сам**; сервер лишь сверяет их с существующим
   процессом с этим PID. Локальный процесс может заявить чужие PID и время
   (время чужого процесса читается с `PROCESS_QUERY_LIMITED_INFORMATION`), а
   секция лобби читаема любым процессом, которому её DACL даёт доступ
   (0.10 по умолчанию — тем же пользователем; `Everyone` — любым), — имя
   канала из ответа **не секрет**. Поэтому сверка (§d.2 шаг 9) ловит только несогласованное
   самоописание (другой PID, канал без PID или без времени), но не
   подмену личности. Аутентификацию дают политика безопасности объектов
   (ObjectSecurity) и приватное пространство имён. **Внутри одной учётной
   записи границы нет**: процесс того же пользователя может открыть,
   прочитать и подменить любые объекты xshm этого пользователя.
7. **(0.10) Сервер Dispatch принимает существующий `B_lock`.** `start`
   открывает мьютекс с `OBJ_OPENIF`: если `B_lock` уже есть (его создал
   клиент до старта сервера или заранее занял чужой процесс), дескриптор
   остаётся **тем, что задал создатель**, а не политика лобби. Политика
   лобби тут ни при чём: в общем `\BaseNamedObjects` (`Local\` своей сессии —
   процессу другого пользователя в той же сессии, `Global\` — любому, у кого
   есть право создавать там объекты) захватчик может заранее создать
   `B_lock` с запрещающим DACL **при любой политике**, в том числе
   `CurrentUser` по умолчанию, — клиенты получат `AccessDenied` на
   регистрации (отказ в обслуживании, не подмена данных). Закрыто для
   приватного пространства имён (§b.4): другой пользователь не может создать
   объект внутри него.
8. **(0.10) Перехват и захват имени канала (находка 13).** В общем
   `\BaseNamedObjects` имя канала, лобби, слота или маяка может заранее
   занять любой процесс с правом создавать там объекты (до 0.10 — NULL DACL у
   всех объектов, объекты сервера открывал и подменял кто угодно). Закрыто:
   (а) политикой `ObjectSecurity` (§b.2) — объекты, созданные сервером,
   открывает только его пользователь и SYSTEM; (б) приватным пространством
   имён (§b.4) — процесс другого пользователя не видит объекты и не может
   занять имена заранее (без пространства захват имени до старта сервера
   даёт отказ в обслуживании: `start` получает `STATUS_OBJECT_NAME_COLLISION`
   или клиенты — `AccessDenied`). **Остаточный риск — тот же пользователь**:
   его процесс проходит границу пространства и DACL `CurrentUser`, значит
   может открыть пространство, занять имя первым или подменить объекты (п. 6).
9. **(0.10) Испорченный `message_count`.** Счётчик пишет и читатель-пир:
   враждебный (например, `u32::MAX`, который инкремент писателя
   заворачивает в `0`) может заставить писателя превысить лимит
   `max_messages` (§c.5) — но только в канале самого вандала; байтовая
   граница (`write_pos − read_pos ≤ capacity`) и проверки кадров соблюдаются
   всегда, читатель счётчик на соответствие лимиту не проверяет.
10. **(Ревизия 3) Исключительная заявка: допущения и остаточные риски.**
    Single-client канал сериализует подключения сам (R18): внешняя
    сериализация не требуется (мьютекс лобби R12 остаётся — очередь клиентов
    лобби и перехват claim-а умершего). Остаётся:
    - **Допущение A1** (как у Multi, `clamp_slot_timeout`): живой держатель
      проходит C1→C6 (прямолинейный участок без ожиданий, микросекунды)
      быстрее `RESERVE_TIMEOUT` (10 с). Нарушение (заморозка ≥ 10 с ровно
      там при толчке другого клиента): claim отнимают; C6 или C8 это
      ловят, но окна в наносекунды остаются — тот же класс, что у Multi,
      принято как остаточный риск:
      - откат `client_pid` держателя (C8 → W) после чужого C5 обнуляет
        самоописание следующего держателя — его сессия без наблюдения;
      - **поздний уход «забытого» клиента**: клиент, чей claim отнят
        отъёмом (не заметил отключения сервером), уходя, сверяет
        `generation == G` и затем пишет `CAS SERVER_READY → IDLE` и
        `DISCONNECT`; если между сверкой и записью целиком прошло
        рукопожатие следующего клиента (S3–S5), записи снимут чужую сессию.
        Требует одновременно ≥ 10 с «забытого» клиента и попадания его
        ухода (наносекунды) на чужую фиксацию;
      - возврат самоописания в S6 (`CAS 0 → pid2`, обнуление
        `client_start` при неизменном claim) — между двумя соседними
        записями сервера значение следующего клиента может быть искажено:
        его сессия без проверки времени или без наблюдения.
    - **Смешанный режим** с черновыми клиентами ревизии 2 (store `HELLO`,
      без claim): сервер принимает такую заявку «наследным» путём (claim
      сессии `0`, как anonymous/драйвер), но store чернового клиента может
      перетереть `HELLO` нового — дефект ревизии 2 сохраняется **только** в
      смешанном режиме; сервер подделку отличить не может, исправить можно
      только версией формата (если черновой пир уже развёрнут — поднять
      `SHARED_VERSION` до `0x0002_0001`, тогда несовместимость явная).
    - **Занятый канал**: клиент, не заметивший отключения сервером и не
      уничтоженный (`SharedClient` пользователя, `mem::forget`), держит
      claim до отъёма — ≥ 10 с с первого толчка. Его поздний уход ничего не
      пишет (`generation != G`); продолжающий писать в кольцо после отъёма —
      тот же риск, что у Multi. **Упрочнение вне ревизии 3** (отслеживается
      отдельно): `ensure_connected` клиента сверяет `generation == G` (одна
      Acquire-загрузка) — отправка и приём «забытого» клиента в чужую сессию
      тогда отказывают `NotConnected`.
    - **Ненаблюдаемый мёртвый клиент** (R6: процесс не открылся — другая
      сессия без прав): S2 не срабатывает (`is_process_gone`
      консервативен), claim умершего в сессии не освобождается сразу —
      только дедлайном по толчку после отключения.

---

## i. Multi-client и wait_for_space: событийная модель (0.9)

До 0.9 Multi-сервер и Multi-клиент просыпались каждые 50 мс (`poll_timeout`),
и этот тик обслуживал всё сразу: чтение данных, освобождение брошенных слотов,
проверку живости клиентов по PID (раз в 3 с), приём команд клиента;
`wait_for_space` нарезал ожидание срезами по 50 мс. В 0.9 тиков нет:
каждое пробуждение — событие ядра или заранее вычисленный дедлайн.
Раскладка памяти, имена и типы объектов слотов **не менялись**; нового
объекта ядра Multi не добавляет (сигнал «толчка» идёт через существующий
`C2S_CONNECT_REQ`).

### i.1 Объекты и поля слота (0.10: предел 256, `slot_count`)

Слот `k` — обычный канал `B_k` (секция + 7 событий §b.2), создаётся
сервером при старте для `k = 0..max_clients-1` (`max_clients ≤ 256`,
0.10; в 0.9 — `≤ 31`).
`ControlBlock.slot_claim` — claim (`0` = свободен, иначе token клиента),
`slot_owner_pid` — PID владельца claim-а, `client_pid` — PID клиента для
handshake (0.8+), `server_pid` — PID сервера. Захват: `CAS slot_claim:
0 → token` (AcqRel), сразу за ним (0.10) `owner_start_lo`/`hi` и `store
slot_owner_pid = PID` (Release, §c.1);
освобождение клиентом — `CAS token → 0`.

**Токен (ревизия 3, общий с single-client каналом).** `T ≠ 0`, уникален
внутри процесса (два клиента одного процесса неразличимы по PID и времени
создания — владение различают только токены) и практически уникален между
процессами. Rust (`shared::claim_token`): `seed` — один раз на процесс
(ОС-энтропия `RandomState` ⊕ PID ⊕ время создания), `T = fmix32(seed + n)`
(финализатор murmur3 — биекция `u32`), `n` — процесс-локальный счётчик
(атомарный `fetch_add`), значение `0` пропускается. Нативный пир вправе
использовать любой генератор с теми же гарантиями; `pid << k | counter` — нет
(32 бит не хватает: PID до 2²⁶+, счётчик быстро заворачивается).

**Перебор слотов и число слотов (0.10).** Клиент перебирает `B_0, B_1, …`
по порядку и останавливается на **первом не открывшемся** слоте, но не
дальше `B_255` (предел 256); захват — как выше. **Исключение (0.10):** отказ
`STATUS_ACCESS_DENIED` (DACL слота не включает этого пользователя, §b.2) —
слот существует, перебор продолжается со следующего. Итог без захвата, по
приоритету: был хоть один отказ в доступе — `AccessDenied`; слоты есть, но все
заняты — `NoFreeSlot`; `B_0` не открылся по другой причине — сервера нет
(`NotConnected`). Толчок §i.3 перебирает до первого не открывшегося слота (в
том числе отказавшего в доступе — best-effort). Предел 256 — цена перебора, а не лимит
`MAXIMUM_WAIT_OBJECTS` (0.10: сервер ждёт все слоты на порту завершения, §i.2).

Сервер ОБЯЗАН записать `slot_count = max_clients` (§c.1, смещение 60) в
**каждый** слот до публикации `magic`. Это **подсказка** (диагностика):
клиенты по ней перебор **НЕ** ограничивают и ОБЯЗАНЫ не ограничивать —
поле доступно на запись любому клиенту, которому DACL слота даёт доступ, и `slot_count = 1` в
`B_0` иначе закрыл бы остальные слоты для всех (отказ в обслуживании).
Любое значение поля, включая мусор, на перебор не влияет.

### i.2 Сервер: кто что ждёт

**0.10 (внутреннее, пиру не видно):** все слоты обслуживает один
поток-диспетчер на собственном порту завершения (IOCP): каждый объект из
таблицы ниже привязан к порту пакетом ожидания (`NtAssociateWaitCompletionPacket`,
ключ — слот и вид, метка — поколение привязки), пробуждение
(`disconnect_client`, `stop`) — пост `NtSetIoCompletion`. Привязка — такое же
ожидание, как `NtWaitForMultipleObjects`: сигнал автосбросного события она
потребляет ровно так же, поэтому объекты, их типы, кто и когда их сигналит, и
правила R1–R17 **не меняются**. Колбэки всех слотов выполняются на этом
потоке по очереди, данные слотов читаются по кругу (до `recv_batch`
сообщений слота за круг). До 0.10: слоты делились на группы по 21
(`SLOTS_PER_WORKER`); на группу — worker-поток и безымянное событие `wake`
(`3 × 21 + 1 = 64 = MAXIMUM_WAIT_OBJECTS`, 256 слотов — 13 потоков), порядок
набора задавал приоритет NT.

| Слот | Объекты | Кто сигналит |
|---|---|---|
| свободный | `C2S_CONNECT_REQ` | клиент после `CLIENT_HELLO` (R4); клиент, не нашедший свободного слота, — «толчок» (§i.3) |
| подключённый | `S2C_DISCONNECT`, `C2S_DATA`, handle процесса клиента | клиент при штатном уходе (R7, R14) / писатель-клиент, увидевший после публикации кадра и полного барьера `read_pos ==` прежнему `write_pos` (R1, §e.2 п. 1) / ядро при смерти процесса |
| — | пробуждение диспетчера (0.10 — пост в порт; 0.9 — событие `wake` группы, последним в наборе) | сервер сам: `disconnect_client` (набор меняется), `stop`/Drop |

Handle процесса клиента: из handshake (`client_pid`, клиент 0.8+,
`ProcessWatch::open_peer`); если PID не передан (клиент ≤ 0.7) — процесс
**владельца claim-а** из `slot_owner_pid` (его пишут клиенты 0.5+). Handshake
слота теперь общий с `SharedServer` (`accept_pending` → `complete_handshake`:
S1–S7 ревизии 3, §d.2 шаг 4: потребление `client_pid` в S6, сброс
`DISCONNECT` до фиксации, `S2C_CONNECT`).

Реакция (диспетчер; до 0.10 — `worker_loop`):
- `CONNECT_REQ` при `client_state == CLIENT_HELLO` — рукопожатие,
  `on_client_connect`, назначается разовый дедлайн проверки (§i.3).
  Без `HELLO` — **не** рукопожатие: claim не трогается (до 0.9 сервер
  безусловно сбрасывал его в `0`).
- `DATA` — слот встаёт в очередь чтения; круг — пачка до `recv_batch`
  сообщений с каждого слота очереди (честная ротация: болтливый слот не
  «затеняет» остальных); пачка выбрана целиком — слот остаётся в очереди,
  следующее ожидание порта нулевое (добор без сна). До 0.10 — пачка с
  каждого подключённого слота группы.
- `DISCONNECT` подключённого слота — дочитать кольцо (0.10: только кадры,
  опубликованные до снимка `write_pos` в момент обнаружения ухода, и не больше
  кадров, чем физически помещается в кольцо; дописанное после обнаружения
  (`DISCONNECT`, смерть, снятый claim) не доставляется; сдвиг `read_pos` пиром
  во время дочитывания — порча кольца, разрыв с `Error` — так же для смерти
  процесса и осиротевшего слота ниже; честный пир после `DISCONNECT` не пишет,
  R14), затем
  `on_client_disconnect_reason(Graceful)`; claim снимается **только** `CAS
  <claim сессии> → 0` (ревизия 2: claim сессии запоминается при
  рукопожатии; раньше — безусловный store `0`, который мог отнять слот у
  нового клиента, захватившего его после того, как ушедший снял свой
  claim). Если слот уже отключён локально (`disconnect_client`), а worker
  спал на старом наборе и поглотил сигнал, адресованный клиенту, — сигнал
  **возвращается** (`NtSetEvent`), клиент его получит.
- процесс клиента сигнален — дочитать кольцо, затем
  `on_client_disconnect_reason(PeerDied)`; claim снимается CAS, только если
  `slot_owner_pid` всё ещё равен PID умершего (новый захват не затирается).
  Handle уходит из набора в любом случае.
- `disconnect_client(id)` (ревизия 2, R17): `server_state = IDLE` и `CAS
  client_state: SERVER_READY → IDLE` (п. 20 §a) → `NtSetEvent(S2C_DISCONNECT)`
  → пробуждение диспетчера (0.9 — `wake` группы) → `on_client_disconnect_reason(Local)`. **Claim НЕ
  освобождается**: его снимает сам клиент, проснувшись по `DISCONNECT`
  (§i.4). До ревизии 2 порядок был «`IDLE` → claim `0` → `DISCONNECT`» —
  ошибочный: слот освобождался раньше, чем клиент узнавал об отключении,
  следующий клиент успевал его захватить, и его `complete_handshake`
  (`NtResetEvent(S2C_DISCONNECT)`) стирал сигнал прежнего — два клиента на
  одном SPSC-кольце. Клиент, так и не проснувшийся (умер без наблюдаемого
  handle), держит слот до дедлайна протухшего захвата (§i.3: слот не
  подключён, claim ≠ 0). Handle процесса клиента закрывает диспетчер на
  следующем проходе (до 0.10 — worker после пробуждения).
- `stop()` (ревизия 2): после join диспетчера (до 0.10 — worker-ов) — каждому подключённому слоту
  `IDLE` → `NtSetEvent(S2C_DISCONNECT)` (claim не трогается, колбэков нет).
  Раньше клиенты узнавали об остановке только при Drop последнего `Arc`
  сервера.
- Осиротевший подключённый слот (claim уже `0`: клиент ушёл штатно и снял
  claim раньше, чем worker разобрал его `DISCONNECT`) — сначала дочитать
  кольцо, потом колбэк (ревизия 2; раньше хвост терялся).

### i.3 Дедлайны вместо тика

Таймаут ожидания сервера (0.10 — порта диспетчера, до 0.10 — worker-а
группы) = время до **ближайшего** дедлайна (или
`None`, если дедлайнов нет; `poll_timeout = Some(t)` — необязательная
страховка). Дедлайны пересчитываются после каждого пробуждения:

| Ситуация | Дедлайн | Действие |
|---|---|---|
| Свободный слот, claim ≠ 0 (клиент захватил и не подключился — упал между CAS и `HELLO`) | момент **первого наблюдения** этого token + `RESERVE_TIMEOUT` (10 с) | `CAS token → 0` |
| Подключённый слот (разово) | handshake + 2 с (`HANDSHAKE_VERIFY_DELAY`) | claim = 0 → «брошенное рукопожатие» (клиент отвалился по своему `slot_timeout` ровно в момент ответа, снял claim без `DISCONNECT`) → отключить (`Graceful`). С ревизии 2 клиент по R15 такого не оставляет (CAS-отзыв либо проигрывает — и клиент подключён, либо выигрывает — и сервер рукопожатие не фиксирует); проверка остаётся для старых клиентов |
| Слот, отключённый `disconnect_client` (ревизия 2) | как свободный слот с claim ≠ 0: первое наблюдение + `RESERVE_TIMEOUT` | клиент не снял claim (умер) → `CAS token → 0` |
| **Single-client канал** (ревизия 3, R18; `SharedServer::start*`, Auto, лобби и каналы Dispatch): сервер не подключён, claim ≠ 0 без `HELLO` | **только по толчку** — дедлайна-таймера нет: на `CONNECT_REQ` без `HELLO` (`wait_for_client`, `wait_for_client_or`, `accept_async`) — первое наблюдение token; толчок не раньше `RESERVE_TIMEOUT` от первого наблюдения при всё ещё отсутствующем `HELLO` | `CAS token → 0`; новый token — новый отсчёт. Ожидание сервера остаётся `[CONNECT_REQ, wake]` без таймаута: пока каналом никто не интересуется, отъём не нужен |

«Первое наблюдение» случается при любом пробуждении worker-а. Чтобы
брошенный захват не висел, пока сервер спит, **клиент, получивший
`NoFreeSlot`, толкает сервер**: для каждого слота с `claim ≠ 0` и
`server_state ≠ SERVER_READY` взводит его `C2S_CONNECT_REQ`
(`nudge_stale_claims`). Сервер просыпается, видит `CONNECT_REQ` без `HELLO`,
claim не трогает и назначает дедлайн. Клиент повторяет попытку раз в
`retry_delay` — сервер будят только клиенты, которым реально нужен слот.
Сервер толчки не ограничивает по частоте: клиент, шлющий `CONNECT_REQ` без
`HELLO` непрерывно, стоит серверу только CPU (пробуждение на сигнал), других
слотов не заслоняет (0.10: порт завершения отдаёт сигналы по очереди).
Клиентский `slot_timeout` по-прежнему клампится ниже `RESERVE_TIMEOUT − 2 с`.

### i.4 Клиент Multi: что ждёт и что ОБЯЗАН сигналить пир

Подключённый клиент ждёт `[S2C_DISCONNECT, S2C_DATA, процесс сервера (0.8+),
wake]` без таймаута; `send`/`stop`/Drop взводят `wake`. На `DISCONNECT` и
смерть сервера — дочитать кольцо, затем `on_disconnect_reason(Graceful |
PeerDied)`; в ответ `DISCONNECT` клиент **не** сигналит (поздний сигнал
достался бы следующему клиенту слота), claim снимает `CAS token → 0` —
ревизия 2: это **единственный** путь освобождения слота после
`disconnect_client` живого клиента (R17). На любом другом пробуждении
(`DATA`, `wake`, страховочный таймаут) клиент сверяет `generation` слота с
запомненным при рукопожатии (ревизия 2): сменился — слот уже у другого
клиента (сервер старой версии освободил claim, и чужой `complete_handshake`
сбросил наш `DISCONNECT`) → если разбудил `S2C_DATA`, он адресован новому
клиенту — **вернуть** его (`NtSetEvent(S2C_DATA)`), затем
`on_disconnect_reason(Graceful)`, без `DISCONNECT`, `CAS token → 0` (не
сработает — claim чужой). Ожидание `S2C_CONNECT` при захвате слота
прерывается `wake` остановки (заявка отзывается по R15, claim снимается).
Вне подключения (сервера нет, свободных слотов нет, ошибка) — пауза
`retry_delay`, прерываемая `wake`.

Ревизия 3: рукопожатие слота — §d.2 шаг 3a с уже захваченным claim (Rust:
`ClaimMode::Held`): шаги C1/C2 пропускаются (их сделал перебор), C6 — `CAS
client_state: IDLE → CLIENT_HELLO` (не удался — `HandshakeFailed`: снять
claim и повторить захват), C8 — сверка `slot_claim == token`, C10 и отзыв —
правила ревизии 3; уход — L: записи состояния и `DISCONNECT` только при
`generation == G`. Claim по-прежнему снимает клиент после ухода
(`release_claim`), порядок R13/R14 сохранён. Claim сессии сервер Multi
берёт из рукопожатия (S6), а не перечитывает после него.

Нативный **клиент** Multi ОБЯЗАН (R13, R15, R17): после CAS захвата записать
свой PID в `slot_owner_pid`; рукопожатие слота — по R15 (§d.2 шаг 3a); до
`CLIENT_HELLO` — PID в `client_pid`; после `HELLO` —
`NtSetEvent(C2S_CONNECT_REQ)`; писать данные с `C2S_DATA` по R1; уходя сам —
состояние `IDLE` (CAS), `NtSetEvent(S2C_DISCONNECT)`, и только потом `CAS
token → 0` (R14); получив `DISCONNECT` от сервера — `CAS token → 0` без
встречного `DISCONNECT`. ЖЕЛАТЕЛЬНО: при `NoFreeSlot` толкать сервер (§i.3);
сверять `generation` на пробуждениях (см. выше).
Нативный **сервер** Multi ОБЯЗАН: рукопожатие слота — по R16; сбрасывать
`S2C_DISCONNECT` перед `S2C_CONNECT` (п. 19 §a); при `disconnect_client` и
`stop` — `IDLE` → `DISCONNECT` **без** освобождения claim (R14, R17); при
штатном уходе клиента — `CAS <claim сессии> → 0`, не store; писать данные с
`S2C_DATA` по R1 — Rust-клиент 0.9 не просыпается ни от чего другого.

### i.5 Совместимость со старыми пирами

| Пир | Что происходит |
|---|---|
| Клиент 0.8 (PID в `client_pid`) | Полностью событийно, как 0.9 |
| Клиент 0.5–0.7 (без `client_pid`) | Наблюдается процесс владельца claim-а (`slot_owner_pid`) — тоже событие. Если handle не открылся (нет прав, другая сессия): смерть такого клиента после рукопожатия замечается **только при пробуждении сервера по другой причине** (разовая проверка PID при пробуждении, не чаще раза в 3 с); периодического тика ради неё нет. Клиент ≤ 0.4 (`slot_owner_pid = 0`) после смерти не замечается вовсе (как и в 0.8 — проверка по PID 0 считает процесс живым) |
| Клиент ≤ 0.8 (без толчка) | Брошенный им захват освобождается по дедлайну, как только сервер проснётся по любой причине; толкать будут клиенты 0.9 |
| Клиент ≤ 0.9 до ревизии 2 | Claim после `DISCONNECT` снимает (так было всегда) — работает; рукопожатие без R15: сервер, отключивший его сразу, для него «не ответил» (откат, повтор захвата) |
| Сервер 0.9 до ревизии 2 / ≤ 0.8 | `disconnect_client` освобождает claim сам — гонка двух клиентов на слоте остаётся; клиент ревизии 2 замечает её по `generation` на ближайшем пробуждении |
| Черновой C-клиент 0.10 (R15 ревизии 2: store `HELLO`, без claim) ↔ сервер ревизии 3 | Заявка без claim принимается «наследным» путём (claim сессии `0`, как у anonymous/драйвера): один такой клиент — безопасно. Смесь с клиентами ревизии 3 на одном канале — его store может перетереть их `HELLO`: дефект ревизии 2 остаётся **только** в смешанном режиме (§h п. 10) |
| Клиент ревизии 3 ↔ черновой C-сервер (R16 ревизии 2) | Клиенты взаимно исключены через claim независимо от сервера; C10 и отзыв совместимы со старым порядком сервера (`generation` до фиксации). Старый сервер не освобождает claim умершего и не делает дедлайн: после краха держателя канал отдаёт `AlreadyConnected`, пока сервер не перезапущен (отказ закрыт — без двух писателей); G1 (сброс `DISCONNECT` после фиксации) остаётся со старым сервером |
| Старый Multi-сервер ↔ клиент Multi ревизии 3 | Как строка выше; claim слота дедлайном снимает он сам |
| Сервер ≤ 0.8 | Клиент 0.9 ждёт `DATA`/`DISCONNECT` без таймаута — старый сервер сигналит их по R1/R7, работает. Сервер ≤ 0.8 не сбрасывает `DISCONNECT` перед `S2C_CONNECT` (риск §h.3) и на `CONNECT_REQ` без `HELLO` (толчок) сбрасывает claim в `0` — толчок клиента 0.9 может освободить чужой ещё не подключённый захват; тот клиент тогда получит отказ рукопожатия и повторит попытку |

### i.6 `wait_for_space` (SharedServer/SharedClient)

Одно `NtWaitForMultipleObjects` на весь остаток таймаута вызывающего
(`None` — без конца), набор `[SPACE направления, S2C_DISCONNECT, процесс
пира]`:

| Индекс | Сигналит | Результат |
|---|---|---|
| 0 `S2C_SPACE` (сервер) / `C2S_SPACE` (клиент) | читатель: сняв заявку `space_waiter` (R3) или опустошив кольцо (R2) | перепроверка места → `Ok(true)` или снова ждать |
| 1 `S2C_DISCONNECT` | пир при штатном уходе (R7) | если состояние пира уже не `SERVER_READY` (сервер смотрит `client_state`, клиент — `server_state`) → событие **взводится обратно** (для других ждущих) и `Err(NotConnected)`; иначе — устаревший сигнал, поглощён, ждать дальше |
| 2 процесс пира (если PID получен) | ядро | место так и не освободилось → `Err(PeerDied)` |
| таймаут | — | `Ok(false)` |

Писатель ставит заявку `space_waiter = need(W, frame_size(len))` (PAD + кадр, §c.3, §c.4) и перепроверяет место до
сна (Dekker, §e.2 п. 2) — потерянного пробуждения нет. Старые пиры:
читатель ≤ 0.7 заявку не снимает и будит только опустошив кольцо (R2) —
задержка, пока он читает, но не зависание; пир без PID, упавший без
`DISCONNECT`, не будит ничем — единственная граница тогда таймаут
вызывающего. Anonymous-сервер (нет объектов ядра) — опрос раз в 1 мс
внутри вызова.

**Синхронный читатель `poll_client`/`poll_server` (0.10).** Набор
`[DATA направления, S2C_DISCONNECT, процесс пира]`, меньший индекс важнее;
до ожидания — перепроверка кольца с барьером (§e.2 п. 1) и состояния пира
в памяти. `DISCONNECT` разбирается по тому же правилу, что в
`wait_for_space`: состояние пира уже не `SERVER_READY` (клиент Rust также
сверяет `generation`) — событие взводится обратно, кольцо **сначала
дочитывается** (`Ok(true)`, пока есть кадры), затем `Err(NotConnected)` и
сторона помечается отключённой; иначе — устаревший сигнал, поглощён,
ожидание продолжается остатком таймаута. Сервер Rust при этом claim
штатно ушедшего клиента не трогает (его снимает клиент, R18), клиент Rust,
увидевший уход сервера, ответного `DISCONNECT` не сигналит. Новых
обязанностей у пира нет: читатель опирается на R14 (уходя, сначала своё
состояние → `IDLE`, потом `DISCONNECT`; данные — до состояния). Пир,
сигналящий `DISCONNECT` до перевода состояния, будет принят за устаревший
сигнал, и синхронный Rust-читатель продолжит ждать до таймаута.
Anonymous-сервер (событий нет): при пустом кольце `client_state !=
SERVER_READY` — `Err(NotConnected)` без ожидания.

---

## Изменения после ревизии 2 (25.09.2026)

Журнал дельты для C/C++-реализации: что поменялось в пир-видимых правилах
после первой редакции этого документа (повторный аудит 25.09.2026). Формат
памяти, имена и типы объектов ядра, протокол регистрации лобби **не**
менялись. Основной текст выше уже приведён к этим правилам; ниже — сводка,
чтобы повторить изменения, не перечитывая документ целиком.

| # | Правило | Было | Стало | Где |
|---|---|---|---|---|
| 1 | Признак принятия заявки клиентом | `S2C_CONNECT` **и** `server_state == SERVER_READY`, иначе откат с `HandshakeFailed` | `S2C_CONNECT` **и** `generation != g0` (`g0` прочитан **до** `CLIENT_HELLO`); `server_state` не проверяется — сервер мог уже отключить клиента, тогда `DISCONNECT` взведён и кольцо надо дочитать | R15, §d.2 шаг 3a |
| 2 | `S2C_CONNECT` без смены `generation` | принимался | устаревший — поглотить, ждать дальше | R15, §d.2 шаг 3a п. 6 |
| 3 | Сброс `S2C_CONNECT` клиентом | нет | `NtResetEvent(S2C_CONNECT)` после чтения `g0` и **до** `CLIENT_HELLO` | R15, §d.2 шаг 3a п. 2, §e.2 п. 4 |
| 4 | Отзыв заявки клиентом (таймаут/остановка/ошибка) | store `client_state = IDLE` | `CAS client_state: CLIENT_HELLO → IDLE`; не удался и `generation != g0` — клиент **подключён**; не удался при `generation == g0` — откат; удался — откат (`client_pid = 0`, заголовки `IDLE`) | R15, §d.2 шаг 3a п. 7 |
| 5 | Фиксация рукопожатия сервером | store `client_state = SERVER_READY`, `S2C_CONNECT` всегда | порядок: `swap(client_pid,0)` → сброс заголовков с `generation+1` → публикация `generation` → заголовки и `server_state` = `SERVER_READY` → `CAS client_state: CLIENT_HELLO → SERVER_READY`; не удался — `server_state` и заголовки `IDLE`, **без** `S2C_CONNECT`, handle процесса не открывать; удался — handle процесса → `NtResetEvent(S2C_DISCONNECT)` → `NtSetEvent(S2C_CONNECT)` | R16, §d.2 шаг 4 |
| 6 | Drop/уход клиента | `client_pid = 0`, store `client_state = IDLE`, `DISCONNECT` | `CAS client_state: SERVER_READY → IDLE` (`client_pid` не трогать), заголовки `IDLE`, затем `DISCONNECT` | R15, R14, §d.2 шаг 7, §e.2 п. 5 |
| 7 | `CONNECT_REQ` без `CLIENT_HELLO` в лобби | документ: «лобби пересоздаётся» (код уже игнорировал) | игнорируется, лобби цело | §d.2 шаги 2, 4 |
| 8 | Выделенный канал Dispatch, неудача первого подключения | повтор раз в 250 мс вечно | `on_error` + `on_disconnect_reason(Error)`, клиент завершён | §d.2 шаг 8 |
| 9 | Multi `disconnect_client` | `IDLE` → claim `0` → `DISCONNECT` | `IDLE` → `DISCONNECT` → `wake`; claim **не** трогать | R17, §i.2 |
| 10 | Multi: освобождение claim после `disconnect_client` | сервер | клиент (`CAS token → 0`, получив `DISCONNECT`), мёртвый клиент — дедлайн `RESERVE_TIMEOUT` | R17, §i.3, §i.4 |
| 11 | Multi: штатный уход клиента на сервере | store claim `0` | `CAS <claim, запомненный при рукопожатии> → 0` | R17, §i.2 |
| 12 | Multi: сверка сессии клиентом | нет | на каждом пробуждении кроме `DISCONNECT`/смерти: `generation` сменился → вернуть поглощённый `S2C_DATA`, отключиться `Graceful` без `DISCONNECT` | §i.4 (ЖЕЛАТЕЛЬНО) |
| 13 | Multi `stop()` | `DISCONNECT` клиентам — только при Drop последнего `Arc` | сразу: подключённым слотам `IDLE` → `DISCONNECT`, claim не трогать | R17, §i.2 |
| 14 | Multi: осиротевший слот (claim уже `0`) | колбэк без дочитывания | сначала дочитать кольцо | §i.2 |
| 15 | prof-shm, отказ без `ProcessExit` вьюера | ждать маяк поколения без таймаута | не дольше `BACKOFF_MAX` = 30 с, затем к маяку лобби | R6, `prof-shm/PROTOCOL.md` §6.5 |
| 16 | Ожидание `S2C_CONNECT` в фоновых клиентах | до таймаута рукопожатия | прерывается своим событием остановки (внутреннее, ЖЕЛАТЕЛЬНО) | §e.1, §e.5 |
| 17 | `DispatchServer::stop()` из колбэка | взаимная блокировка | отложенная остановка без join (внутреннее, ЖЕЛАТЕЛЬНО) | §d.3, §f |

Исправления документа без изменения поведения: §d.2 шаг 4 (реакция на
`HandshakeFailed`, сброс `S2C_DISCONNECT` перед `S2C_CONNECT`), §h.1
(межсессионный Dispatch в 0.9 работает — п. 22 §a), §e.4 (Rust выполняет
R1–R17), §g (фактический прогон).

После ревизии 2 (26.09.2026) добавлен только Rust API без пир-видимых
последствий: `set_thread_start_hook` (§f) — нативной реализации повторять
нечего.

Минимальный порядок внедрения для C/C++: (1) клиент — пункты 1–4, 6;
(2) сервер — пункт 5; (3) Multi — пункты 9–11, 13 (сервер) и 10, 12
(клиент); (4) Dispatch-клиент — пункт 8; (5) prof-shm-продюсер — пункт 15.
Тесты-эталоны — §g п. 12.

## Изменения 0.10 (16.10.2026)

Аудит 16.10.2026 и запрос «настраиваемые размеры по каждому направлению».
Формат памяти сменился (`SHARED_VERSION = 0x00020000`), имена и типы
объектов ядра, рукопожатие (R2–R17), событийная модель и протокол лобби
(`DISPATCH_VERSION = 2`, байты сообщений) — **прежние**; строгость разбора и ожидания лобби — п. 18–21; правило сигнала `DATA` (R1) — п. 24; PAD-кадры и непрерывный payload — п. 26; рукопожатие ревизии 3 (R15/R16/R18, исключительная заявка single-client канала) — п. 28.

| # | Правило | Было (0.9) | Стало (0.10) | Где |
|---|---|---|---|---|
| 1 | Версия формата | `0x00010000` | `0x00020000`; чужая версия — отказ на обеих сторонах | §c |
| 2 | Раскладка | фиксированная: 2 × 2 МиБ, смещение кольца B вычисляется | `LayoutBlock` @64: `segment_size`, `flags`, дескрипторы `s2c`/`c2s` (`offset`, `capacity`, `max_messages`, `max_message_size`); кольцо S2C канонически с 128 | §c.2 |
| 3 | Поля `ControlBlock` | `reserved[0..3]` по индексам | именованные поля на тех же смещениях: `slot_claim`, `slot_owner_pid`, `server_pid`, `client_pid` | §c.1 |
| 4 | Кадр | `u16 len` + `u16 flags`, без выравнивания, payload ≤ 65 535 | `u32 len` + `u32 flags`, кадр выровнен на 8, payload ≤ `max_message_size` кольца (до `capacity / 2 − 8`, п. 26) | §c.4 |
| 5 | Минимальное сообщение | 2 байта | 1 байт | §c.4 |
| 6 | Лимит сообщений в кольце | константа 500 | `max_messages` направления | §c.2 |
| 7 | Публикация сегмента | `magic`/`version` — обычные записи в начале инициализации | `magic` пишется **последним** (release); `magic == 0` у клиента — «не готово», а не «повреждён» | §c.5 |
| 8 | Проверка чужой секции клиентом | view ≥ фиксированного размера | раскладка из `LayoutBlock` против фактического размера view | §c.5 |
| 9 | Открытие событий канала | без проверки | тип ОБЯЗАН быть SynchronizationEvent, иначе отказ | §c.5 п. 5 |
| 10 | Читатель кольца | доверял `message_count` | пустота — по `write_pos == read_pos`; кадр сверяется с `write_pos`, лимитом и выравниванием; мусор — `Corrupted` и разрыв | §c.5 |
| 11 | Лобби Dispatch | 2 × 2 МиБ | по умолчанию 2 × 16 КиБ (самоописываемая раскладка — клиенту безразлично) | §c.2 |
| 12 | Отказ Dispatch-сервера | только ошибки | также политика (`DispatchHandler::admit` → `Reject`) и лимит `max_clients` (по умолчанию 64): ответ `STATUS_REJECTED` | §d |
| 13 | Личность процесса пира | только PID (`server_pid`/`client_pid`/`slot_owner_pid`): переиспользованный или подделанный PID наблюдался как пир | рядом с каждым PID — время создания процесса (`server_start_*`@36, `client_start_*`@44, `owner_start_*`@52, половины `lo`/`hi`); процесс с тем же PID, но другим `CreateTime` — не пир; `0` — сверки нет. Пир ДОЛЖЕН писать время до соответствующего PID | §c.1 |
| 14 | `Local\`/без префикса при ошибке `NtQueryInformationProcess(ProcessSessionInformation)` | сессия считалась `0` — объект молча уходил в глобальный `\BaseNamedObjects` (мимо пира из своей сессии, навстречу чужим объектам) | ошибка `WindowsError { context: "NtQueryInformationProcess(session)" }`, объект не создаётся и не открывается | §b.1 |
| 15 | `C2S_CONNECT_REQ` без `CLIENT_HELLO` (толчок Multi, откатившийся клиент, чужой процесс) у ждущего `SharedServer::wait_for_client`/worker-а Auto/лобби Dispatch | `wait_for_client` возвращал `HandshakeFailed`, worker Auto звал `on_error` | автосбросный сигнал поглощается, ожидание продолжается на остаток таймаута; пиру слать такой сигнал безопасно. Multi (`accept_pending`) — без изменений: claim не трогается | §d.2, §i.3 |
| 16 | Маяк: клиент, не нашедший лобби при взведённом маяке | `lower` + перепроверка лобби + `raise`: при ≥ 2 клиентах один гасил маяк, только что взведённый новым сервером, — другой спал вечно | новый объект `<B>_beacon_state` (секция `SEC_COMMIT` 4096, `OBJ_OPENIF`, `magic`@0 = `0x4E434258`, `generation`@4, `server_pid`@8, `server_start_lo/hi`@12/16); сервер в `raise` пишет время, PID, `generation += 1`, затем `NtSetEvent`; в `lower` — `server_pid = 0`, время `0`, затем `NtResetEvent`; секция сначала открывается, создаётся только при `NAME_NOT_FOUND`, при `PRIVILEGE_NOT_HELD`/`ACCESS_DENIED` — маяк без состояния (поведение 0.9, гонка остаётся; так же с нативным сервером без состояния); клиент — `clear_if_stale`: процесс сервера не подтверждённо мёртв — маяк не трогать, иначе `NtResetEvent` и перепроверка `generation` (изменилось — `NtSetEvent` обратно). Клиент `raise`/`lower` не вызывает | §b.2, §b.3 |
| 17 | Число слотов Multi | `max_clients ≤ 31`; клиент перебирал `B_0..B_30` до первого не открывшегося слота, `slot_count` не было | `max_clients ≤ 256`; клиент перебирает `B_0..B_255` до первого не открывшегося слота (`B_0` не открылся — сервера нет); сервер пишет `slot_count`@60 = `max_clients` в каждый слот до `magic` — подсказка, клиенты по ней перебор НЕ ограничивают (поле пишет любой клиент: иначе отказ в обслуживании) | §c.1, §i.1 |
| 18 | Разбор сообщений лобби | хвостовые байты отбрасывались, битый UTF-8 → U+FFFD, `name_len > 64` при достаточном буфере принимался; на ошибку разбора сервер **молчал** (клиент ждал `response_timeout`) | сообщение — ровно заголовок + имя: `len > 64`, хвост, не-UTF-8 — `Corrupted`; сервер отвечает `STATUS_REJECTED` + `S2C_DATA` | §d.1 |
| 19 | Конец сессии лобби сервером | `mark_disconnected` без сигнала | после `IDLE` — `NtSetEvent(S2C_DISCONNECT)`, в том числе когда ответа не будет (таймаут чтения, ошибка, остановка) | §d.2 шаг 6 |
| 20 | Ожидание ответа клиентом лобби | `S2C_DATA` до `response_timeout` | `[S2C_DATA, S2C_DISCONNECT, процесс сервера]`; кольцо читается первым; `DISCONNECT` при ушедшей сессии — `NotConnected`, смерть сервера — `PeerDied` | §d.2 шаг 7, §e.4 таблица ожиданий |
| 21 | Согласованность самоописания клиента канала Dispatch (не аутентификация) | не проверялась | PID лобби сверен (PID + ненулевое `client_start_*` совпали с процессом) — пир канала ОБЯЗАН сообщить тот же PID и ненулевое совпадающее время, иначе (в т. ч. время `0`) канал закрывается до `on_client_connect`; не сверен — сверки нет; `ClientRegistration::handshake_pid`. Подмену личности не ловит: PID и время пир пишет сам (§h п. 6) | §d.2 шаг 9, §h |
| 22 | Дескриптор безопасности создаваемых именованных объектов (секция и события канала, слоты Multi, лобби и `B_lock`, каналы Dispatch, маяк и `_beacon_state`) | NULL DACL у всех | задаёт создатель (`ObjectSecurity`), по умолчанию DACL = SID пользователя токена + `S-1-5-18`, `GENERIC_ALL` (`CurrentUserAllowLowIntegrity` — + SACL `S:(ML;;NW;;;LW)`, `Everyone` — NULL DACL, `Descriptor` — свой); открывающий по-прежнему передаёт `SecurityDescriptor = NULL`, отказ — `STATUS_ACCESS_DENIED`. Сервер Dispatch сам создаёт `B_lock` (`OBJ_OPENIF`, политика лобби) в `start` и держит его (существующий принимает с его DACL — §h п. 7). Клиент Multi: отказ в доступе к слоту перебор не обрывает, итог — `AccessDenied` (§i.1). Служба SYSTEM ↔ пользователь через `Global\` — `Everyone` или свой дескриптор у создающей стороны | §b.2, §h п. 4, 6, 7, §i.1 |
| 23 | Где живут именованные объекты | только `\BaseNamedObjects` (`Local\`/`Global\`/NT-путь) | по выбору — частное пространство имён: граница = имя + SID пользователя (+ метка Low), `NtCreatePrivateNamespace` (`ObjectName = NULL`, коллизия — `NtOpenPrivateNamespace`), объекты — `RootDirectory` = handle пространства + относительное имя без префикса; каналы Dispatch — в пространстве лобби; псевдоним ядром не используется; пространство не удаляется (`NtDeletePrivateNamespace` не вызывается), исчезает с последним handle. Без пространства — поведение прежнее | §b.4, §h п. 7, 8 |
| 24 | Сигнал `DATA` (R1) | писатель сигналил при `prev message_count == 0`; читатель считал пустоту по позициям и уменьшал счётчик после сдвига `read_pos` — интерливинг «читатель сдвинул `read_pos` → писатель `message_count += 1` (видит 1, без сигнала) → читатель уменьшил счётчик и увидел пусто до публикации `write_pos` → сон» оставлял кадр в кольце без сигнала (взаимное ожидание) | писатель: `store(write_pos)` → полный барьер → `load(read_pos)`, сигнал при `read_pos ==` прежнему `write_pos`; читатель: перед сном полный барьер и перепроверка `write_pos` (изменился — читать снова). `message_count` — только лимит и статистика. Пир со старым правилом с Rust 0.10 теряет пробуждения так же | §e.2 п. 1, §c.5 |
| 25 | Лимит `max_messages` у писателя | по `message_count` при любом заполнении; пустое кольцо при «полном» счётчике — `Corrupted` | лимит (вытеснение/«полно») — только при `write_pos − read_pos > 0`; пустое по позициям кольцо принимает кадр при любом счётчике. Счётчик законно отстаёт от `read_pos` на 1 и может кратковременно быть `max_messages + 1` — читатель это порчей не считает | §c.5, §e.2 п. 1 |
| 26 | Непрерывный payload (zero-copy) | payload кадра мог разрываться границей кольца (хвост — с начала кольца); `flags` всегда `0`; `max_message_size ≤ capacity − 8`; `space_waiter = frame_size(len)` | кадр никогда не пересекает конец кольца: если `frame_size(len) > tail(W)`, писатель пишет PAD-кадр (`flags` бит 0 = `FRAME_PAD`, `len = tail(W) − 8`, может быть 0) на остаток и кладёт кадр с индекса 0; PAD + кадр — одна публикация `write_pos`, сигнал `DATA` по позиции до PAD; место и `space_waiter` — `need(W, F)` = PAD + кадр; PAD не сообщение (`message_count`, `drop_count`, `max_messages` не трогает), не доставляется, вытесняется перезаписью без учёта. Читатель: PAD обязан кончаться ровно на границе, стоять не на индексе 0 и лежать до `write_pos`; кадр сообщения через границу, PAD в другом месте, неизвестные биты `flags` — `Corrupted`. `max_message_size ≤ capacity / 2 − 8` (вывод — §c.2), сегмент с большим — `Corrupted`. Пир ОБЯЗАН реализовать PAD на чтении и на записи | §c.2, §c.3, §c.4, §c.5, §e.2 п. 1–2 |
| 27 | Лимит `max_messages` при счётчике на пределе (Task 18, решение 28) | непустое кольцо при `message_count ≥ max_messages` — вытеснение старейшего (`send`) или «полно» (`try_send`) по одному счётчику: отставший на 1 счётчик (читатель между CAS `read_pos` и декрементом) вытеснял сообщение на кадр раньше | один снимок `W`/`R`/`C` на решение о байтах и лимите; при `C ≥ max_messages`: `max_messages ≤ RECOUNT_MAX` (= 64) — пересчёт сообщений по кадрам `[R, W)` (без PAD, до `max_messages`, ≤ 129 заголовков; кадр не по §c.4 — «полно»), «полно» при пересчитанных `max_messages`; `max_messages > 64` — «полно» только при `C > max_messages` (O(1), кольцо может держать `max_messages + 1`). Писатель ОБЯЗАН применять то же правило с тем же порогом; гарантия лимита — при честном читателе (§h п. 9); читателю не видно | §c.5, §h |
| 28 | **Ревизия 3 рукопожатия (Task 22): исключительная заявка single-client канала** | — | — | R15, R16, R18, §d.2 шаги 3–4, 7 |
| 28.1 | C1: захват | single-client — без claim, конкурирующие клиенты писали заявку поверх друг друга | `CAS slot_claim: 0 → T` до любых записей; проигрыш — толчок `CONNECT_REQ` (если `server_state != SERVER_READY`) и `AlreadyConnected`, ничего не записано; затем `owner_start`, `slot_owner_pid` | §d.2 шаг 3a п. 1–2 |
| 28.2 | C6: заявка | store `client_state = CLIENT_HELLO` (даже поверх `SERVER_READY` чужой сессии) | `CAS client_state: IDLE → CLIENT_HELLO`; не удался — откат PID/времени, `CAS T → 0`, `AlreadyConnected`; после заявки C8 — сверка `slot_claim == T` | §d.2 шаг 3a п. 6, 8 |
| 28.3 | C10: принятие | `S2C_CONNECT` при `generation != g0` | `S2C_CONNECT` при `client_state == SERVER_READY` или `IDLE` и `generation != g0`; при `CLIENT_HELLO` — устаревший при любом `generation` | §d.2 шаг 3a п. 10 |
| 28.4 | W: отзыв | CAS не удался и `generation != g0` — подключён | CAS не удался: наблюдён `SERVER_READY` — подключён; `IDLE` и `generation != g0` — подключён; иначе откат; откат — затем `CAS T → 0` | §d.2 шаг 3a п. 11 |
| 28.5 | L: уход клиента | `CAS SERVER_READY → IDLE` и `DISCONNECT` всегда | только при `generation == G` (иначе — ничего); затем всегда `CAS T → 0` **последним** | §d.2 шаг 7, §e.2 п. 5 |
| 28.6 | S2: заявка умершего | фиксировалась (сессия с мёртвым клиентом) | процесс не открылся, `pid == slot_owner_pid` и подтверждённо ушёл по `client_start` и `owner_start`, а перед CAS `slot_claim` всё ещё `T` (иначе S2 пропускается) — обнулить самоописание и заголовки, затем `CAS HELLO → IDLE`, `CAS T → 0`, `HandshakeFailed` без `generation` и `S2C_CONNECT` | §d.2 шаг 4 |
| 28.7 | S4: сброс `DISCONNECT` | после фиксации, перед `S2C_CONNECT` | **до** фиксации | §d.2 шаг 4, §e.2 п. 4 |
| 28.8 | S1/S6: самоописание клиента | `swap(client_pid, 0)` и `swap(client_start, 0)` до публикации `generation` | S1 — чтение без потребления; S6 — потребление CAS-ом и claim сессии, только застав владельца в сессии (`client_state == SERVER_READY`), с возвратом при сменившемся claim; иная зафиксированная заявка (ABA) — наблюдать её процесс | §c.1, §d.2 шаг 4 |
| 28.9 | `mark_disconnected` | состояние `IDLE`, claim не трогался (single-client claim не было) | после сброса состояния: handle процесса клиента сигнален (подтверждённая смерть) — `CAS <claim сессии> → 0`; штатный уход и отключение сервером claim не трогают | R18 |
| 28.10 | Протухший захват single-client | — | на `CONNECT_REQ` без `HELLO` у неподключённого сервера: первое наблюдение token; не раньше `RESERVE_TIMEOUT` (10 с) от него и без `HELLO` — `CAS token → 0`; без таймера | §i.3, §e.5 |
| 28.11 | Токен claim-а | Multi: `RandomState` на вызов, внутри процесса мог повториться (2⁻³² на пару) | общий генератор `fmix32(seed + n)` (уникален в процессе) | §i.1 |
| 28.12 | Лобби: мьютекс достался брошенным | — | ЖЕЛАТЕЛЬНО перехватить claim умершего держателя: `CAS c → T` после проигранного `CAS 0 → T`, только при `client_state == IDLE` (стоящую заявку/сессию умершего разбирает сервер); сервер в S2 обнуляет самоописание умершего **до** CAS `HELLO → IDLE`, поэтому перехват в окне «заявка снята, claim ещё стоит» безопасен. Клиент Rust при стоящей заявке или сессии умершего получает `AlreadyConnected` сразу, без повтора внутри `DispatchClient::connect`; повтор после того, как сервер снимет заявку (S2 или наблюдение процесса), проходит | §d.2 шаги 3–4 |
| 29 | Разрыв сессии по своей ошибке (`Corrupted` при чтении/записи кольца, отказ ожидания) — канал Auto и выделенный канал Dispatch (final review I1/I2) | Rust (0.9 и 0.10 до исправления): клиент помечал себя отключённым и уходил **без** `IDLE`/`DISCONNECT` — сервер оставался «подключённым»; в 0.9 это лечил store `CLIENT_HELLO` следующего подключения, а с исключительной заявкой ревизии 3 (п. 28.2) повторный захват упирался в C6 (`AlreadyConnected`) навсегда; сервер Rust ставил `IDLE` **без** `S2C_DISCONNECT` — клиент спал «подключённым» к никому | клиент — полный уход L (`CAS SERVER_READY → IDLE`, заголовки `IDLE`, `DISCONNECT`, claim последним); сервер — `IDLE`, затем `S2C_DISCONNECT` (как Multi `force_disconnect`); так же сервер закрывает сессию, если не удался сигнал `S2C_CONNECT` (S7, §d.2 шаг 4). Нативный пир ОБЯЗАН так же; со своей стороны получит `DISCONNECT` от Rust при ошибке в своём кольце | §c.5, §d.2 шаг 7, §e.2 п. 5, R7 |
| 30 | Сверка сессии клиентом Auto (final review M1) | клиент Auto не сверял `generation`: «не заметивший» клиент (claim отнят по дедлайну, принят другой) писал в кольцо новой сессии | на каждом пробуждении и в каждой отправке/приёме — `generation == G`; иначе уход без записей, без `DISCONNECT`, потреблённый сигнал (`DATA`/`SPACE`/`DISCONNECT`) ЖЕЛАТЕЛЬНО вернуть `NtSetEvent` (как MultiClient; иначе следующий владелец сессии теряет пробуждение до своего сигнала) | общее правило ухода (§e.2 п. 5); для лобби — §d.2 шаг 7 |
| 31 | Атрибуты кэширования чужой секции (надёжность, 17.10.2026) | открывающая сторона отвергала только `SEC_RESERVE` (`0x04000000`), `SEC_IMAGE` (`0x01000000`), `SEC_FILE` (`0x00800000`) и отсутствие `SEC_COMMIT` (`0x08000000`): секция `SEC_COMMIT \| SEC_NOCACHE` или `SEC_COMMIT \| SEC_WRITECOMBINE`, занявшая имя канала, принималась — на WC-памяти записи не упорядочены (TSO нарушен, публикации кольца Release/Acquire теряют смысл), на UC-памяти каждая атомарная операция мимо кэша (DoS по латентности, `lock` — шинная блокировка) | в `NtQuerySection(SectionBasicInformation).AllocationAttributes` также отвергаются `SEC_NOCACHE` (`0x10000000`) и `SEC_WRITECOMBINE` (`0x40000000`) → `ShmError::Corrupted` до отображения; запрет-список, а не «ровно `SEC_COMMIT`»: `SEC_LARGE_PAGES` (`0x80000000`) и неизвестные служебные биты допускаются. Пути: открытие секции канала клиентом (`NtOpenSection`, отображение целиком), открытие заголовка слота Multi при переборе, существующая секция состояния маяка (`NtOpenSection` и ветка `STATUS_OBJECT_NAME_EXISTS` у `NtCreateSection(OBJ_OPENIF)`). Свою секцию создатель делает `SEC_COMMIT` без этих битов (как и прежде). Нативный пир ОБЯЗАН так же (маска отказа дополняется `SEC_NOCACHE \| SEC_WRITECOMBINE`) | §b.2, §c.5 п. 1, §g п. 14 |

Порядок внедрения для C/C++: (1) §c.1–§c.4 — структуры и кадр; (2) клиент —
§c.5 (проверки раскладки и типа событий); (3) сервер — порядок инициализации
§c.5 (`magic` последним); (4) читатель — проверки границы доверия;
(5) ревизия 3 (п. 28): клиент — генератор токена, C1–C10, отзыв W с разбором
наблюдённого, уход L (сверка `generation`, claim последним), толчок при
проигрыше, Multi — заявка по уже захваченному claim; сервер — S1–S7
(S2 и S6 — `NtOpenProcess` + `NtQueryInformationProcess(ProcessTimes)`, §c.1),
`NtResetEvent(DISCONNECT)` до фиксации, освобождение claim умершего в
`mark_disconnected`, проверка протухшего захвата на каждом `CONNECT_REQ` без
`HELLO` (минимальная безопасная реализация — без дедлайна: хуже только
живучесть); клиент лобби — перехват claim при брошенном мьютексе
(ЖЕЛАТЕЛЬНО). Тесты-эталоны — §g п. 13.
