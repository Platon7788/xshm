# CHANGELOG — xshm

Журнал изменений (до 0.9.0 вёлся в неотслеживаемом `Docs/CHANGELOG.md`;
теперь канонический — этот файл).

## [0.9.0] — не выпущено (ветка `feat/beacon`)

Запросы транспорта профайлера (`prof-shm`) и его аудита 2026-09-25: строго
событийная модель — ни одного пробуждения в простое, ни опроса по таймеру.

Контракт совместимости для нативных (C/C++) реализаций — что видно пиру, какие
события он обязан сигналить, чтобы Rust 0.9 не ждал вечно, объекты ядра маяка —
[`INTEROP.md`](INTEROP.md).

### Добавлено
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
