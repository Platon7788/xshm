//! Lock-free SPSC кольцевой буфер для shared memory IPC.
//!
//! ВАЖНО: Код оптимизирован для x86/x86_64 с TSO (Total Store Order).
//! На этих архитектурах stores видны в порядке программы, что упрощает
//! синхронизацию. НЕ портировать на ARM/RISC-V без доработки!
//!
//! Кадр (формат `0x0002_0000`): `u32 len LE` + `u32 flags` + payload,
//! дополненный до кратного `FRAME_ALIGN` (8). Позиции кадров кратны 8, поэтому
//! заголовок кадра не разрывается границей кольца.
//!
//! Непрерывные кадры (0.10): кадр целиком (заголовок + payload) лежит
//! внутри кольца одним куском. Если до конца кольца кадр не помещается,
//! писатель занимает остаток PAD-кадром (`flags = FRAME_PAD`, `len` =
//! остаток − 8) и кладёт кадр с начала кольца; PAD и кадр публикуются ОДНОЙ
//! записью `write_pos`. Поэтому payload можно отдать срезом прямо из кольца
//! (`read_with`) и заполнить на месте (`write_with`). Цена -- предел
//! `max_message_size <= capacity / 2 - 8` (`layout::max_payload_for`).
//!
//! Автомат чтения кадра в позиции `read` (`frame_at`):
//! - `write - read == 0` -- пусто;
//! - занятость больше ёмкости или `read` не кратна 8 -- `Corrupted`;
//! - `flags` с битами кроме `FRAME_PAD` -- `Corrupted`;
//! - PAD: обязан кончаться ровно на границе кольца (`index + 8 + len ==
//!   capacity`), стоять НЕ на индексе 0 и лежать в опубликованных данных,
//!   иначе `Corrupted`;
//!   читатель сдвигает `read_pos` за него (CAS) без доставки и без учёта;
//! - сообщение: `len` в `MIN_MESSAGE_SIZE..=max_message_size`, кадр в
//!   опубликованных данных и НЕ пересекает конец кольца, иначе `Corrupted`.
//!
//! Граница доверия: содержимое кольца и `RingHeader` пишет другой процесс.
//! Читатель сверяет каждый кадр с `write_pos` (кадр не выходит за
//! опубликованные данные), с лимитом размера сообщения кольца, с
//! выравниванием и с границей кольца -- мусор пира даёт `Corrupted`, а не
//! зависание, панику или выход за пределы кольца.

use std::marker::PhantomData;
use std::os::windows::io::{AsHandle, BorrowedHandle};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering, compiler_fence, fence};
use std::time::{Duration, Instant};

use crate::constants::*;
use crate::error::{Result, ShmError};
use crate::layout::{RingConfig, RingHeader};
use crate::win::{EventHandle, ProcessWatch, wait_any};

/// Шаг опроса, когда событий нет вовсе (anonymous-сервер: секция без имени,
/// ни одного объекта ядра, кроме самой секции). Это единственный путь
/// `wait_for_space` с опросом -- и он действует только пока вызывающий сам
/// блокируется в `wait_for_space`, а не в простое. Именованный канал (0.9+)
/// ждёт события без нарезки на срезы.
const SPACE_POLL_INTERVAL: Duration = Duration::from_millis(1);

#[cfg(test)]
thread_local! {
    /// Число ожиданий в ядре внутри `wait_for_space` на этом потоке: тесты
    /// доказывают, что ожидание не нарезано на срезы (одно событие -- одно
    /// ожидание).
    pub(crate) static SPACE_KERNEL_WAITS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// Число ожиданий в ядре внутри `wait_for_data` (`poll_*`) на этом
    /// потоке: тесты спина доказывают, что данные, пришедшие во время
    /// спина, читаются без сна в ядре.
    pub(crate) static DATA_KERNEL_WAITS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Политика спина перед ожиданием в ядре (0.10).
///
/// Перед тем как уснуть на событии (`DATA` у читателя, `SPACE` у писателя)
/// поток до `iterations` раз перепроверяет кольцо по позициям, с
/// `std::hint::spin_loop` между проверками. Данные (место), появившиеся за
/// это время, забираются без системного вызова и без переключения
/// контекста: задержка падает с единиц микросекунд до сотен наносекунд
/// ценой занятого ядра CPU на время спина.
///
/// Бюджет -- один на каждое засыпание, а не на каждую проверку: поток,
/// которому нечего делать, отспинивает бюджет один раз и спит в ядре без
/// таймаута, как и без спина (в простое -- ноль пробуждений). По умолчанию
/// -- [`SpinPolicy::NONE`]: спина нет, поведение 0.9.
///
/// Только локальная политика ожидания: формат памяти и протокол сигналов
/// не меняются, пиру спин не виден.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpinPolicy {
    /// Число проверок кольца до ожидания в ядре (`0` -- без спина).
    pub iterations: u32,
}

impl SpinPolicy {
    /// Без спина -- сразу ожидание в ядре (по умолчанию).
    pub const NONE: Self = Self { iterations: 0 };

    /// Спин на `iterations` проверок.
    #[must_use]
    pub const fn new(iterations: u32) -> Self {
        Self { iterations }
    }

    /// Спин включён (`iterations > 0`).
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        self.iterations > 0
    }
}

/// Спин: до `spin.iterations` раз вызвать `ready`, пока он не вернёт
/// `true` (`std::hint::spin_loop` между вызовами). `true` -- условие
/// выполнено до исчерпания бюджета, `false` -- бюджет исчерпан (или спин
/// выключен) и вызывающий уходит в обычное ожидание в ядре.
///
/// `ready` обязан быть дешёвым (атомарные чтения без блокировок) и сам
/// включать проверку остановки, если поток её ждёт: так задержка `stop()`
/// ограничена одной итерацией, а не всем бюджетом.
///
/// `deadline` -- дедлайн вызывающего (`poll_*_spin`, `wait_for_space_spin`):
/// проверяется раз в `SPIN_DEADLINE_STRIDE` итераций (`Instant::now` -- не
/// на каждой), наступил -- спин прекращается с `false`. Перебег таймаута
/// поэтому не больше `SPIN_DEADLINE_STRIDE` итераций (единицы микросекунд),
/// а не весь бюджет. Worker-ы без таймаута передают `None`.
pub(crate) fn spin_until(
    spin: SpinPolicy,
    deadline: Option<Instant>,
    mut ready: impl FnMut() -> bool,
) -> bool {
    for i in 0..spin.iterations {
        if ready() {
            return true;
        }
        if let Some(deadline) = deadline
            && i % SPIN_DEADLINE_STRIDE == SPIN_DEADLINE_STRIDE - 1
            && Instant::now() >= deadline
        {
            return false;
        }
        std::hint::spin_loop();
    }
    false
}

/// Раз во сколько итераций спин сверяется с дедлайном вызывающего.
const SPIN_DEADLINE_STRIDE: u32 = 64;

/// Дедлайн из таймаута (`None` -- без таймаута или он непредставим как
/// `Instant`: ждать «вечно» и так).
pub(crate) fn deadline_of(timeout: Option<Duration>) -> Option<Instant> {
    timeout.and_then(|t| Instant::now().checked_add(t))
}

/// Проба «есть непрочитанные данные» по позициям одного кольца -- для
/// спина worker-а Multi, который перебирает кольца нескольких слотов, не
/// беря их блокировок. Хранит только адрес `RingHeader` в отображении.
#[derive(Debug, Clone, Copy)]
pub(crate) struct UnreadProbe<'a> {
    header: NonNull<RingHeader>,
    _ring: PhantomData<&'a RingHeader>,
}

impl<'a> UnreadProbe<'a> {
    /// Та же проба с другим временем жизни.
    ///
    /// # Safety
    /// Отображение, в котором лежит заголовок кольца, живо всё время `'b`.
    pub(crate) const unsafe fn extend<'b>(self) -> UnreadProbe<'b> {
        UnreadProbe {
            header: self.header,
            _ring: PhantomData,
        }
    }

    /// В кольце есть опубликованный непрочитанный кадр
    /// (`write_pos != read_pos`, Acquire). Без барьера: «пусто» здесь --
    /// лишь повод спинить дальше, а перед сном читатель всё равно
    /// подтверждает пустоту через `confirm_empty`.
    pub(crate) fn has_unread(&self) -> bool {
        // SAFETY: заголовок валиден всё время `'a` (инвариант конструктора
        // `RingBuffer::unread_probe` / контракт `extend`); поля атомарные.
        let header = unsafe { self.header.as_ref() };
        header.write_pos.load(Ordering::Acquire) != header.read_pos.load(Ordering::Acquire)
    }
}

/// Граница дочитывания кольца ушедшего пира (Multi, 0.10): кадры
/// читаются, только пока своя позиция чтения раньше снимка `write_pos`,
/// сделанного в момент обнаружения ухода, и не больше `left` штук
/// (физическая ёмкость кольца, а не настройка). Всё, что пир дописал после
/// снимка, не доставляется; сдвиг `read_pos` пиром во время дочитывания
/// (перемотка назад, перезапись) -- `Corrupted`. Враждебный писатель не
/// удержит читателя в дочитывании и не заставит доставить кадр дважды.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DrainBound {
    /// Снимок `write_pos`.
    pub(crate) until: u32,
    /// Своя позиция чтения: снимок `read_pos`, далее -- сдвиги за
    /// прочитанные этим читателем кадры (PAD и сообщения).
    pub(crate) read: u32,
    /// Сколько кадров (PAD и сообщений) ещё можно прочитать.
    pub(crate) left: u32,
}

/// Штатное отключение пира для `wait_for_space` и `wait_for_data`: событие
/// `DISCONNECT` (одно на обе стороны, автосброс) и проверка по разделяемой
/// памяти, что пир действительно ушёл (его состояние handshake уже не
/// `SERVER_READY`; клиент сверяет и `generation`) -- отличает свежий сигнал
/// от устаревшего. Свежий сигнал ожидающий возвращает взведённым,
/// устаревший -- поглощает.
pub(crate) struct DisconnectWatch<'a> {
    pub event: &'a EventHandle,
    pub peer_left: &'a dyn Fn() -> bool,
}

/// Подключённый worker (Auto, `MultiClient`; T11c, решение AD) поглотил
/// устаревший `DISCONNECT` -- индекс 0 своего набора: пир в сессии
/// (`SharedServer::client_left`/`SharedClient::server_left` -- `false`).
/// Возвращать сигнал некому: сторона его единственный потребитель, а пир
/// сигналит только уходя (сначала состояние, потом сигнал -- R14), так
/// что сигнал при живой сессии не адресован никому.
///
/// Остальные объекты набора (`handles[1..]`: данные, место, процесс пира,
/// `wake`) проверяются одним ожиданием с нулевым таймаутом (П1/П2 §e.1):
/// поток устаревших сигналов на индексе 0 иначе скрывал бы их -- смерть
/// пира и `SPACE` без такой проверки не видны вовсе, данные и остановку
/// worker и так решает по состоянию на каждом проходе. Цена сигнала --
/// O(1): одно чтение состояния и один системный вызов без сна. Индекс --
/// в полном наборе (`handles`), `Ok(None)` -- ничего не взведено.
pub(crate) fn rest_after_stale_disconnect(handles: &[BorrowedHandle<'_>]) -> Result<Option<usize>> {
    let rest = handles.get(1..).unwrap_or_default();
    Ok(wait_any(rest, Some(Duration::ZERO))?.map(|index| index + 1))
}

/// Результат записи в кольцо.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteOutcome {
    /// Сколько старейших непрочитанных сообщений вытеснено (только запись
    /// с перезаписью; у `try_send*` всегда `0`).
    pub overwritten: u32,
    /// Сигналить `DATA`: после публикации кадра и полного барьера писатель
    /// увидел `read_pos ==` прежнему `write_pos` -- читатель уже выбрал всё,
    /// что было до этого кадра, и мог уснуть (протокол «публикация ->
    /// перепроверка», см. `RingBuffer::publish`). Не «счётчик был 0»:
    /// `message_count` читатель уменьшает ПОСЛЕ сдвига `read_pos`, и решение по
    /// нему теряло пробуждение. Лишний `true` безвреден, ложного `false` при
    /// спящем читателе нет.
    pub was_empty: bool,
}

/// Размер кадра в кольце для payload длины `payload_len`: заголовок
/// (`FRAME_HEADER_SIZE`) + payload, дополненные до кратного `FRAME_ALIGN`.
#[must_use]
pub const fn frame_size(payload_len: usize) -> usize {
    (FRAME_HEADER_SIZE + payload_len).next_multiple_of(FRAME_ALIGN)
}

/// Свободное место в исходящем кольце.
///
/// Снимок, снятый на стороне писателя, -- **нижняя граница**: единственный,
/// кто уменьшает свободное место, это сам писатель, а читатель параллельно
/// может только освобождать. Поэтому если `fits(n)` вернул `true`, следующий
/// `try_send*` того же писателя с payload длины `n` гарантированно пройдёт
/// (при одном писателе на направление -- см. SPSC-контракт).
///
/// Снимок, снятый из другого потока, чем писатель, -- просто наблюдение:
/// писатель мог уже занять часть места.
///
/// Поля читаются свободно; строит снимок только крейт (`#[non_exhaustive]`,
/// 0.10): новое поле не ломает потребителей. Снаружи -- `FreeSpace::ZERO`
/// или `Default`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct FreeSpace {
    /// Свободные байты кольца (всего, включая остаток до конца кольца,
    /// который может уйти под PAD-кадр).
    pub bytes: usize,
    /// Наибольший кадр (`frame_size(len)`, байты), который можно записать
    /// прямо сейчас с учётом PAD-кадра (0.10): кадр ложится непрерывно до
    /// конца кольца, либо после PAD -- с начала кольца. Не больше `bytes`.
    /// Решение «влезет ли сообщение» принимается по нему, а не по `bytes`.
    pub max_frame: usize,
    /// Сколько ещё сообщений можно положить до лимита `max_messages` кольца.
    pub messages: u32,
    /// Лимит размера payload этого кольца (`RingConfig::max_message_size`).
    pub max_message_size: usize,
}

impl FreeSpace {
    /// Нулевое место: канал не подключён или кольцо заполнено.
    pub const ZERO: Self = Self {
        bytes: 0,
        max_frame: 0,
        messages: 0,
        max_message_size: 0,
    };

    /// Поместится ли сейчас payload длины `payload_len` без перезаписи.
    /// Для недопустимых длин (`< MIN_MESSAGE_SIZE` или больше лимита кольца)
    /// -- всегда `false`: такие сообщения не принимаются никогда.
    ///
    /// Множество помещающихся длин -- всегда префикс `1..=max_payload()`:
    /// кадр не длиннее `max_frame` помещается (см. `max_frame_for`).
    #[must_use]
    pub const fn fits(&self, payload_len: usize) -> bool {
        payload_len >= MIN_MESSAGE_SIZE
            && payload_len <= self.max_message_size
            && self.messages > 0
            && self.bytes >= frame_size(payload_len)
            && self.max_frame >= frame_size(payload_len)
    }

    /// Максимальная длина payload, которая поместится прямо сейчас
    /// (`0` -- не поместится ни одно сообщение).
    #[must_use]
    pub const fn max_payload(&self) -> usize {
        let frame = if self.max_frame < self.bytes {
            self.max_frame
        } else {
            self.bytes
        };
        if self.messages == 0 || frame < frame_size(MIN_MESSAGE_SIZE) {
            return 0;
        }
        // Наибольший len с frame_size(len) <= frame: выравнивание поглощает
        // до FRAME_ALIGN - 1 байт хвоста.
        let room = (frame & !(FRAME_ALIGN - 1)) - FRAME_HEADER_SIZE;
        if room > self.max_message_size {
            self.max_message_size
        } else {
            room
        }
    }
}

/// Наибольший кадр, который помещается в `free` свободных байт, когда до
/// конца кольца от позиции записи `tail` байт (оба кратны 8, `tail >= 8`).
///
/// Кадр `F` помещается, если `F <= min(free, tail)` (непрерывно до конца)
/// или `tail + F <= free` (PAD на остаток + кадр с начала кольца). Первое
/// множество -- префикс до `min(free, tail)`, второе -- `(tail, free -
/// tail]`; вместе -- префикс до `max(min(free, tail), free - tail)`: если
/// `free - tail <= tail`, второе множество пусто или внутри первого.
pub(crate) const fn max_frame_for(free: usize, tail: usize) -> usize {
    let contiguous = if free < tail { free } else { tail };
    let after_pad = free.saturating_sub(tail);
    if contiguous > after_pad {
        contiguous
    } else {
        after_pad
    }
}

/// Кадр, найденный читателем по позиции `read`.
#[derive(Debug, Clone, Copy)]
struct Frame {
    /// Длина payload (у PAD -- длина заполнителя, без смысла для чтения).
    len: usize,
    /// Сколько байт кадр занимает в кольце (сдвиг `read_pos` за него).
    total: u32,
    /// PAD-кадр (`FRAME_PAD`): пропускается без доставки и без учёта.
    pad: bool,
}

/// Место под кадр, найденное писателем (`RingBuffer::place`).
#[derive(Debug, Clone, Copy)]
struct Placement {
    /// Индекс payload в данных кольца.
    payload: usize,
    /// Сдвиг `write_pos`: PAD (если есть) + кадр.
    advance: u32,
}

#[derive(Debug)]
pub struct RingBuffer {
    header: NonNull<RingHeader>,
    storage: NonNull<u8>,
    capacity: u32,
    mask: u32,
    max_messages: u32,
    max_message_size: u32,
}

// SAFETY: тип хранит только адреса внутри shared-маппинга, который живёт не
// меньше самого кольца (инвариант `RingBuffer::new`). Всё состояние кольца --
// атомарные поля `RingHeader`, доступ к данным идёт по позициям, опубликованным
// через Acquire/Release, поэтому передача между потоками безопасна.
unsafe impl Send for RingBuffer {}
// SAFETY: см. выше; SPSC-контракт (один писатель, один читатель) обеспечивает
// вызывающий код, а не система типов.
unsafe impl Sync for RingBuffer {}

impl RingBuffer {
    /// # Safety
    /// `header` -- выровненный на 64 байта `RingHeader`, `data` -- начало
    /// `config.capacity` байт данных кольца; обе области не пересекаются и
    /// валидны всё время жизни кольца. `config` проверен
    /// (`RingConfig::validate`).
    ///
    /// Нулевой указатель -- `Err(Corrupted)`, а не паника. Библиотека
    /// строит кольца из адресов отображения (`from_parts`); из сырых
    /// указателей -- только тестовые фикстуры.
    #[cfg(test)]
    pub unsafe fn new(header: *mut RingHeader, data: *mut u8, config: &RingConfig) -> Result<Self> {
        let (Some(header), Some(data)) = (NonNull::new(header), NonNull::new(data)) else {
            return Err(ShmError::Corrupted);
        };
        // SAFETY: контракт `new` совпадает с контрактом `from_parts`.
        Ok(unsafe { Self::from_parts(header, data, config) })
    }

    /// Кольцо из заведомо ненулевых указателей (`SharedView`: адреса внутри
    /// отображения).
    ///
    /// # Safety
    /// То же, что у [`RingBuffer::new`].
    pub(crate) unsafe fn from_parts(
        header: NonNull<RingHeader>,
        storage: NonNull<u8>,
        config: &RingConfig,
    ) -> Self {
        debug_assert!(config.validate().is_ok());
        RingBuffer {
            header,
            storage,
            capacity: config.capacity as u32,
            mask: config.capacity as u32 - 1,
            max_messages: config.max_messages,
            max_message_size: config.max_message_size as u32,
        }
    }

    /// Лимит размера payload.
    #[cfg(test)]
    pub const fn max_message_size(&self) -> usize {
        self.max_message_size as usize
    }

    const fn header(&self) -> &RingHeader {
        // SAFETY: указатель на заголовок валиден и выровнен на всё время жизни
        // `self` (инвариант `RingBuffer::new`); все поля атомарные, поэтому
        // shared-ссылки достаточно и для записи.
        unsafe { self.header.as_ref() }
    }

    const fn data_ptr(&self) -> *mut u8 {
        self.storage.as_ptr()
    }

    const fn mask_index(&self, pos: u32) -> usize {
        (pos & self.mask) as usize
    }

    /// Проверка длины payload для записи; возвращает размер кадра.
    pub(crate) const fn frame_len(&self, payload_len: usize) -> Result<u32> {
        if payload_len < MIN_MESSAGE_SIZE {
            return Err(ShmError::MessageTooSmall);
        }
        if payload_len > self.max_message_size as usize {
            return Err(ShmError::MessageTooLarge);
        }
        // max_message_size <= capacity / 2 - FRAME_HEADER_SIZE
        // (RingConfig::validate), поэтому кадр вместе с наихудшим PAD
        // помещается в пустое кольцо при любой позиции записи и в u32.
        Ok(frame_size(payload_len) as u32)
    }

    /// Слово заголовка кадра по индексу `index` (кратен 4, внутри кольца).
    fn header_word(&self, index: usize) -> &AtomicU32 {
        // Инвариант вызывающих (позиции проверены `frame_at`/`writer_used`);
        // нарушение -- ошибка в самом крейте, проверка дешёвая и остаётся в
        // release: запись за пределы кольца хуже паники.
        assert!(
            index.is_multiple_of(4) && index + 4 <= self.capacity as usize,
            "ring frame header index out of bounds"
        );
        // SAFETY: index + 4 <= capacity (проверено выше) -- адрес внутри
        // данных кольца.
        let word = unsafe { self.data_ptr().add(index) };
        // SAFETY: контракт `AtomicU32::from_ptr`: адрес выровнен на 4 (index
        // кратен 4, storage выровнен на 64 -- идёт сразу за RingHeader) и
        // валиден всё время жизни `&self` (инвариант `new`); к слову заголовка
        // кадра обе стороны обращаются только атомарно, поэтому гонка с
        // перезаписью писателем -- не data race.
        unsafe { AtomicU32::from_ptr(word.cast()) }
    }

    /// Адрес байта `index` данных кольца. Само вычисление безопасно
    /// (`wrapping_add`); разыменовывать адрес можно только при `index <
    /// capacity` -- это обеспечивают вызывающие (`frame_at`, `place`).
    const fn data_at(&self, index: usize) -> *mut u8 {
        self.data_ptr().wrapping_add(index)
    }

    /// Кадр в позиции `read` при опубликованном `write` (автомат -- в
    /// документации модуля).
    ///
    /// `Ok(None)` -- кольцо пусто; `Err(Corrupted)` -- позиции, заголовок
    /// кадра или флаги не согласуются (порча пиром или гонка с перезаписью
    /// -- решает вызывающий, перечитав `read_pos`). Найденный кадр всегда
    /// лежит внутри данных кольца одним куском и внутри опубликованных
    /// данных: на этом держится безопасность срезов `read_with`.
    fn frame_at(&self, read: u32, write: u32) -> Result<Option<Frame>> {
        let used = write.wrapping_sub(read);
        if used == 0 {
            return Ok(None);
        }
        if used > self.capacity || !(read as usize).is_multiple_of(FRAME_ALIGN) {
            return Err(ShmError::Corrupted);
        }
        let index = self.mask_index(read);
        // До конца кольца: кратно 8 и >= 8 (index кратен 8 и < capacity).
        let tail = self.capacity as usize - index;
        // Relaxed: видимость байтов кадра дало Acquire-чтение write_pos.
        let len = self.header_word(index).load(Ordering::Relaxed) as usize;
        let flags = self.header_word(index + 4).load(Ordering::Relaxed);
        let (total, pad) = match flags {
            0 => {
                if !(MIN_MESSAGE_SIZE..=self.max_message_size as usize).contains(&len) {
                    return Err(ShmError::Corrupted);
                }
                let total = frame_size(len);
                // Payload больше не разрывается границей кольца (0.10).
                if total > tail {
                    return Err(ShmError::Corrupted);
                }
                (total, false)
            }
            // PAD обязан занимать ровно остаток кольца до границы; `len == 0`
            // допустим (остаток -- один заголовок). PAD на индексе 0 (на всё
            // кольцо) честный писатель не пишет никогда (кадр <= capacity / 2
            // помещается с индекса 0) -- отвергаем: за PAD всегда стоит
            // сообщение с индекса 0, и два PAD подряд в опубликованных данных
            // невозможны (пропуск PAD в `next_message` ограничен).
            FRAME_PAD if index != 0 && len == tail - FRAME_HEADER_SIZE => (tail, true),
            // PAD не до границы и любые неизвестные биты флагов.
            _ => return Err(ShmError::Corrupted),
        };
        // total <= tail <= capacity -- помещается в u32.
        let total = total as u32;
        if total > used {
            return Err(ShmError::Corrupted);
        }
        Ok(Some(Frame { len, total, pad }))
    }

    /// Вытеснить старейший кадр (запись с перезаписью). `Ok(true)` --
    /// вытеснено сообщение, `Ok(false)` -- вытеснен PAD-кадр (не считается)
    /// или читатель успел сдвинуть `read_pos` (перепроверить место).
    fn discard_oldest(&self, read: u32, write: u32) -> Result<bool> {
        let header = self.header();
        let Some(frame) = self.frame_at(read, write)? else {
            return Ok(false);
        };
        // CAS: читатель может параллельно забрать этот же кадр.
        if header
            .read_pos
            .compare_exchange(
                read,
                read.wrapping_add(frame.total),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return Ok(false);
        }
        if frame.pad {
            // PAD -- не сообщение: ни message_count, ни drop_count.
            return Ok(false);
        }
        self.decrement_count();
        // Relaxed: счётчик статистики, ничего не публикует.
        header.drop_count.fetch_add(1, Ordering::Relaxed);
        Ok(true)
    }

    /// `message_count - 1` без ухода ниже нуля: пир может испортить
    /// счётчик, и обычный `fetch_sub` превратил бы `0` в `u32::MAX` (кольцо
    /// навсегда «полно по счётчику»). Возвращает прежнее значение.
    fn decrement_count(&self) -> u32 {
        self.header()
            .message_count
            .try_update(Ordering::AcqRel, Ordering::Acquire, |c| c.checked_sub(1))
            .unwrap_or(0)
    }

    /// Занятость кольца для писателя с проверкой позиций из разделяемой
    /// памяти. Читатель-пир может испортить и `read_pos`, и (через тот же
    /// заголовок) `write_pos`: невыровненная позиция записи дала бы индекс
    /// заголовка кадра у самого конца кольца, и запись кадра писала бы ЗА его
    /// пределы. Позиции обязаны быть кратны `FRAME_ALIGN`, занятость -- не
    /// больше ёмкости; иначе `Corrupted` и ни одного байта в кольцо.
    const fn writer_used(&self, write: u32, read: u32) -> Result<u32> {
        let used = write.wrapping_sub(read);
        if used > self.capacity
            || !(write as usize).is_multiple_of(FRAME_ALIGN)
            || !(read as usize).is_multiple_of(FRAME_ALIGN)
        {
            return Err(ShmError::Corrupted);
        }
        Ok(used)
    }

    /// Запись с политикой «перезаписать старейшее»: при нехватке места
    /// (байт или слотов `max_messages`) вытесняет самые старые непрочитанные
    /// сообщения, число вытесненных -- в `WriteOutcome::overwritten`.
    ///
    /// Число итераций ограничено: вытеснение идёт только при `used > 0`
    /// (см. `admits`), каждая итерация либо сдвигает `read_pos` к
    /// `write_pos` (наше вытеснение или чтение читателя), либо застаёт
    /// кольцо пустым -- а пустое кольцо кадр принимает всегда (вместе с
    /// наихудшим PAD, `RingConfig::validate`). PAD-кадры вытесняются как
    /// обычные, но в `overwritten`/`drop_count` не попадают. `Corrupted` --
    /// только по позициям и заголовкам кадров, не по счётчику.
    ///
    /// Лимит сообщений (`admits`, Task 18, решение 28, INTEROP §c.5): при
    /// `max_messages <= RECOUNT_MAX` (64) он точный -- счётчик на пределе
    /// пересчитывается по кадрам, и после записи в кольце не больше
    /// `max_messages` сообщений; выше порога вытеснение начинается только
    /// при `message_count > max_messages` -- кольцо может кратковременно
    /// держать `max_messages + 1` сообщение (запас на законное отставание
    /// счётчика). Гарантия -- при честном читателе.
    pub fn write_message(&self, payload: &[u8]) -> Result<WriteOutcome> {
        let total = self.frame_len(payload.len())?;
        let header = self.header();
        let mut overwritten = 0u32;

        loop {
            let write = header.write_pos.load(Ordering::Acquire);
            let read = header.read_pos.load(Ordering::Acquire);
            let used = self.writer_used(write, read)?;
            let count = header.message_count.load(Ordering::Acquire);
            if self.admits(read, write, used, total, count) {
                let place = self.place(write, payload.len(), total);
                // SAFETY: `place` вернул индекс payload, за которым до конца
                // кольца не меньше `payload.len()` байт; диапазон свободен
                // (`admits`) и не опубликован -- читатель его не трогает;
                // источник -- локальный срез, с разделяемой памятью не
                // пересекается.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        payload.as_ptr(),
                        self.data_at(place.payload),
                        payload.len(),
                    );
                }
                let was_empty = self.publish(write, place.advance);
                return Ok(WriteOutcome {
                    overwritten,
                    was_empty,
                });
            }
            if used == 0 {
                // Недостижимо: пустое кольцо принимает любой допустимый кадр
                // (`admits`, `RingConfig::validate`). Ветка держит цикл
                // конечным, если инвариант конфигурации когда-нибудь
                // нарушат: вытеснять из пустого кольца нечего.
                return Err(ShmError::Corrupted);
            }
            if self.discard_oldest(read, write)? {
                overwritten += 1;
            }
        }
    }

    /// Запись без перезаписи: либо сообщение целиком ложится в свободное
    /// место кольца, либо `Err(QueueFull)` и кольцо не меняется вовсе.
    ///
    /// Семантика:
    /// - **никогда** не трогает `read_pos` и непрочитанные данные
    ///   (`overwritten` в результате всегда `0`, `drop_count` не растёт);
    /// - атомарность сообщения та же, что у `write_message`: данные
    ///   копируются до публикации `write_pos`, читатель видит сообщение
    ///   целиком или не видит вовсе;
    /// - `QueueFull` -- не хватает байт (`frame_size(len)`, а если кадр не
    ///   помещается до конца кольца -- PAD на остаток + кадр) или исчерпан
    ///   лимит сообщений (`admits`, решение 28): при `max_messages <=
    ///   RECOUNT_MAX` (64) лимит точный -- `QueueFull`, когда в кольце по
    ///   кадрам `max_messages` сообщений (отставший счётчик пересчитывается);
    ///   выше порога -- при `message_count > max_messages`, то есть кольцо
    ///   может кратковременно держать `max_messages + 1` сообщение
    ///   (`free_space` консервативно считает лимитом `max_messages`). Ложных
    ///   успехов нет, ложный `QueueFull` возможен только если читатель
    ///   освободил место в тот же момент -- повтор увидит его;
    /// - длина вне `MIN_MESSAGE_SIZE..=max_message_size` --
    ///   `MessageTooSmall`/`MessageTooLarge` (кадр максимальной длины всегда
    ///   помещается в пустое кольцо, «больше кольца» не бывает);
    /// - писатель на направление ровно один (SPSC), как и у `write_message`.
    ///
    /// Ordering: `read_pos` читается с `Acquire`. Читатель двигает его CAS-ом
    /// (`AcqRel`) строго ПОСЛЕ того, как докопировал сообщение, поэтому всё,
    /// что писатель затем пишет в освобождённый диапазон, happens-after
    /// чтения этого диапазона читателем.
    pub fn try_write_message(&self, payload: &[u8]) -> Result<WriteOutcome> {
        let (write, place) = self.reserve(payload.len())?;
        // SAFETY: `reserve` вернул индекс payload, за которым до конца кольца
        // не меньше `payload.len()` байт; диапазон свободен и не опубликован
        // -- читатель его не трогает; источник -- локальный срез, с
        // разделяемой памятью не пересекается.
        unsafe {
            std::ptr::copy_nonoverlapping(
                payload.as_ptr(),
                self.data_at(place.payload),
                payload.len(),
            );
        }
        let was_empty = self.publish(write, place.advance);
        Ok(WriteOutcome {
            overwritten: 0,
            was_empty,
        })
    }

    /// Запись без перезаписи с заполнением на месте (zero-copy, 0.10):
    /// `f` получает срез ровно из `len` байт payload прямо в кольце и
    /// заполняет его; после возврата `f` кадр публикуется. Семантика места и
    /// ошибок -- как у `try_write_message` (`QueueFull` -- `f` не
    /// вызывается, кольцо не меняется).
    ///
    /// Срез -- ещё не опубликованная часть кольца: читатель-пир по
    /// протоколу его не читает и не пишет, содержимое принадлежит писателю.
    /// Начальное содержимое не определено (остатки прежних кадров): `f`
    /// обязан записать все `len` байт, иначе уйдут старые байты. Паника в
    /// `f` -- кадр не публикуется (кольцо как до вызова, кроме байтов
    /// неопубликованной области).
    ///
    /// Граница доверия: враждебный пир, нарушая протокол, может писать в
    /// эту область параллельно с `f` -- страдает только сообщение, которое
    /// получит он сам (его же кольцо приёма); памяти писателя это не
    /// касается.
    ///
    /// # Safety
    /// Вызывающий -- ЕДИНСТВЕННЫЙ писатель этого направления всё время `f`:
    /// ни вложенной записи в это кольцо из `f`, ни записи из другого потока
    /// (иначе вторая запись получит ту же область -- алиасинг `&mut`, а
    /// кадрирование разойдётся). Публичные обёртки (`Shared*::send_with`)
    /// обеспечивают это заимствованием `&mut self`.
    pub(crate) unsafe fn write_with(
        &self,
        len: usize,
        f: impl FnOnce(&mut [u8]),
    ) -> Result<WriteOutcome> {
        let (write, place) = self.reserve(len)?;
        // SAFETY: `reserve` вернул индекс payload, за которым до конца кольца
        // не меньше `len` байт (кадр не пересекает границу); диапазон
        // свободен и не опубликован, у честного пира к нему нет доступа до
        // `publish` ниже, других писателей этого направления на время `f`
        // нет (контракт функции) -- `&mut` уникален. Срез живёт только
        // внутри `f`.
        let buf = unsafe { std::slice::from_raw_parts_mut(self.data_at(place.payload), len) };
        f(buf);
        let was_empty = self.publish(write, place.advance);
        Ok(WriteOutcome {
            overwritten: 0,
            was_empty,
        })
    }

    /// Общая часть записи без перезаписи: проверка длины, позиций и места
    /// (PAD + кадр), запись заголовков PAD/кадра. Возвращает прежний
    /// `write_pos` и место payload; публикует вызывающий (`publish`).
    ///
    /// write_pos меняет только этот (единственный) писатель; read_pos и
    /// message_count читатель может лишь уменьшать «занятость», поэтому
    /// снимок ниже -- консервативная оценка свободного места.
    fn reserve(&self, payload_len: usize) -> Result<(u32, Placement)> {
        let total = self.frame_len(payload_len)?;
        let header = self.header();
        let write = header.write_pos.load(Ordering::Acquire);
        let read = header.read_pos.load(Ordering::Acquire);
        let count = header.message_count.load(Ordering::Acquire);
        let used = self.writer_used(write, read)?;
        if !self.admits(read, write, used, total, count) {
            return Err(ShmError::QueueFull);
        }
        Ok((write, self.place(write, payload_len, total)))
    }

    /// Сколько байт займёт кадр `total` байт, записанный с позиции `write`:
    /// сам кадр, если он помещается до конца кольца, иначе PAD на остаток +
    /// кадр с начала кольца (`<= 2 * total - 8 <= capacity`, см.
    /// `layout::max_payload_for`).
    pub(crate) const fn frame_need(&self, write: u32, total: u32) -> u32 {
        let tail = self.capacity - (write & self.mask);
        if total <= tail { total } else { tail + total }
    }

    /// Принять ли кадр `total` байт с позиции `write` при снимке `read`,
    /// занятости `used` (позиции уже проверены `writer_used`) и значении
    /// `message_count == count`. Место -- PAD (если нужен) + кадр
    /// (`frame_need`).
    ///
    /// `message_count` -- подсказка, а не истина: его пишет и пир
    /// (недоверенный), и законно отстаёт на 1 -- читатель уменьшает его
    /// ПОСЛЕ сдвига `read_pos` (CAS), так что пустое по позициям кольцо
    /// может показывать `count == max_messages` (при `max_messages = 1` --
    /// обычная гонка). Поэтому лимит сообщений вытесняет/отказывает только
    /// при `used > 0`; пустое кольцо принимает любой кадр, помещающийся по
    /// байтам (а помещается любой допустимый вместе с PAD: `max_message_size
    /// <= capacity / 2 - FRAME_HEADER_SIZE`). PAD-кадры в лимит не входят.
    ///
    /// Task 18, решение 28 (INTEROP §c.5): отставший на 1 счётчик не должен
    /// вытеснять сообщение на кадр раньше (`write_message`) или давать
    /// ложный `QueueFull`. Правило зависит от порога `RECOUNT_MAX`:
    /// - `max_messages <= RECOUNT_MAX`: счётчик на пределе (`count >=
    ///   max_messages`) -- повод пересчитать сообщения по кадрам того же
    ///   снимка `[read, write)` (`messages_between`, не больше `2 *
    ///   RECOUNT_MAX + 1` заголовков) -- лимит точный;
    /// - выше порога: «полно» только при `count > max_messages` -- O(1),
    ///   законное отставание поглощено, кольцо может держать `max_messages +
    ///   1` сообщение.
    ///
    /// Счётчик здесь не правим -- его сводит насыщающий `decrement_count`
    /// читателя.
    fn admits(&self, read: u32, write: u32, used: u32, total: u32, count: u32) -> bool {
        self.capacity - used >= self.frame_need(write, total)
            && (used == 0
                || count < self.max_messages
                || if self.max_messages <= RECOUNT_MAX {
                    self.messages_between(read, write) < self.max_messages
                } else {
                    count == self.max_messages
                })
    }

    /// Сколько кадров сообщений (без PAD) лежит между снимком `read` и
    /// опубликованным `write` -- по заголовкам кадров, а не по счётчику;
    /// счёт останавливается на `max_messages` (больше знать не нужно).
    ///
    /// Вызывает только писатель: кадры в `[read, write)` опубликовал он сам,
    /// честный читатель их заголовки не меняет, а уже забранные им (снимок
    /// `read` мог устареть) писатель ещё не перезаписал -- такие кадры лишь
    /// завышают ответ (консервативно: в худшем случае -- вытеснение, как без
    /// пересчёта). Кадры, которые не сходятся (порча пиром), -- ответ
    /// `max_messages`: решение остаётся за прежним правилом, а `Corrupted`
    /// сообщит вытеснение (`frame_at`). Цикл конечен и короток: вызывается
    /// только при `max_messages <= RECOUNT_MAX` (`admits`), счёт стоп на
    /// `max_messages`, а PAD не стоит дважды подряд (за PAD -- кадр с
    /// индекса 0, `frame_at`) -- не больше `2 * RECOUNT_MAX + 1` шагов;
    /// каждый шаг сдвигает позицию к `write`, не перескакивая его.
    fn messages_between(&self, read: u32, write: u32) -> u32 {
        let mut pos = read;
        let mut messages = 0u32;
        while pos != write && messages < self.max_messages {
            let Ok(Some(frame)) = self.frame_at(pos, write) else {
                return self.max_messages;
            };
            if !frame.pad {
                messages += 1;
            }
            pos = pos.wrapping_add(frame.total);
        }
        messages
    }

    /// Записать заголовки кадра длины `len` (`total` байт) в позицию
    /// `write`: при нужде сначала PAD-кадр на остаток кольца, затем
    /// заголовок кадра (с начала кольца). Ничего не публикует.
    ///
    /// Вызывающий обязан предварительно убедиться (`admits`), что
    /// `frame_need(write, total)` байт начиная с `write` свободны.
    fn place(&self, write: u32, len: usize, total: u32) -> Placement {
        let index = self.mask_index(write);
        let tail = self.capacity as usize - index;
        // Relaxed: заголовки публикует Release-запись write_pos в `publish`.
        let (frame, pad) = if total as usize <= tail {
            (index, 0)
        } else {
            // PAD до границы: len = остаток - заголовок (может быть 0).
            self.header_word(index)
                .store((tail - FRAME_HEADER_SIZE) as u32, Ordering::Relaxed);
            self.header_word(index + 4)
                .store(FRAME_PAD, Ordering::Relaxed);
            (0, tail as u32)
        };
        self.header_word(frame).store(len as u32, Ordering::Relaxed);
        self.header_word(frame + 4).store(0, Ordering::Relaxed);
        Placement {
            payload: frame + FRAME_HEADER_SIZE,
            advance: pad + total,
        }
    }

    /// Публикует записанные с позиции `write` PAD (если был) и кадр ОДНОЙ
    /// записью `write_pos += advance` и решает, нужен ли сигнал `DATA`.
    /// Возвращает `true`, если читатель мог уснуть, не увидев кадр, --
    /// вызывающий сигналит `DATA` (`WriteOutcome::was_empty`).
    ///
    /// Протокол «публикация -> перепроверка» (Dekker, пара к
    /// `confirm_empty` читателя; зеркало `arm_space_waiter`/
    /// `take_space_waiter`): писатель публикует `write_pos`, ставит
    /// `fence(SeqCst)` и ЗАТЕМ читает `read_pos`; читатель, увидев пустоту
    /// (`read_pos == write_pos`), ставит `fence(SeqCst)` и ЗАТЕМ перечитывает
    /// `write_pos`. Сдвиг `read_pos` читателем (его CAS) предшествует его
    /// фенсу. Два SeqCst-фенса гарантируют, что хотя бы одна сторона увидит
    /// запись другой: либо читатель при перепроверке увидит новый
    /// `write_pos` и прочитает кадр, либо писатель увидит `read_pos ==
    /// write` (всё до кадра выбрано) и просигналит событие -- потерянного
    /// пробуждения нет. PAD и кадр видны читателю одновременно, поэтому
    /// сравнение -- с `write` ДО PAD, правило §e.2 п. 1 применяется один раз.
    /// `message_count` и `sequence` -- лимит и статистика, на решение о
    /// сигнале не влияют.
    fn publish(&self, write: u32, advance: u32) -> bool {
        let header = self.header();
        // Заголовки и байты payload (копия или `write_with`) записаны выше по
        // программе: Release-запись write_pos публикует их читателю.
        // ВАЖНО: сначала увеличиваем message_count, потом обновляем write_pos:
        // писатель, проверяющий лимит, не увидит «лишнего» места. PAD
        // сообщением не считается. Прежнее значение счётчика на сигнал НЕ
        // влияет (см. протокол выше).
        header.message_count.fetch_add(1, Ordering::AcqRel);
        header
            .write_pos
            .store(write.wrapping_add(advance), Ordering::Release);

        // Публикация write_pos упорядочена перед чтением read_pos -- первая
        // половина Dekker-пары с `confirm_empty` (на x86 -- mfence: TSO сам
        // по себе разрешает обгон store -> load).
        fence(Ordering::SeqCst);
        // Acquire -- по конвенции; порядок с перепроверкой читателя дал фенс.
        // `read_pos == write`: читатель (или наше же вытеснение в
        // `write_message`) выбрал всё до этого кадра. Больше `write` -- кадр
        // уже прочитан, будить некого; меньше -- читатель ещё не дочитал
        // старые кадры и не уснёт, не дойдя до этого.
        let was_empty = header.read_pos.load(Ordering::Acquire) == write;
        if was_empty {
            // Relaxed: счётчик статистики переходов empty -> non-empty.
            header.sequence.fetch_add(1, Ordering::Relaxed);
        }
        was_empty
    }

    /// Читатель: перед тем как признать кольцо пустым (и, возможно,
    /// уснуть на `DATA`), убедиться, что писатель не опубликовал кадр после
    /// снимка `write_seen` (при котором `read_pos == write_seen`). `true` --
    /// пусто и спать безопасно: писатель, публикующий кадр позже, увидит
    /// `read_pos == write_seen` и просигналит `DATA` (Dekker-пара с
    /// `publish`, см. там). `false` -- кадр появился, читать снова.
    fn confirm_empty(&self, write_seen: u32) -> bool {
        // Парный фенс к `publish`: сдвиг read_pos этим читателем (CAS в
        // `read_message`) упорядочен перед повторным чтением write_pos.
        fence(Ordering::SeqCst);
        self.header().write_pos.load(Ordering::Acquire) == write_seen
    }

    /// Свободное место кольца (байты и слоты сообщений).
    ///
    /// Для писателя -- нижняя граница (см. `FreeSpace`): порядок загрузок
    /// тот же, что в `try_write_message`, и каждое из значений может только
    /// устареть в сторону «занято больше, чем на самом деле».
    pub fn free_space(&self) -> FreeSpace {
        let header = self.header();
        let write = header.write_pos.load(Ordering::Acquire);
        let read = header.read_pos.load(Ordering::Acquire);
        let count = header.message_count.load(Ordering::Acquire);
        let used = write.wrapping_sub(read);
        let messages = self.max_messages.saturating_sub(count);
        // Занятость больше ёмкости -- только на испорченном заголовке:
        // трактуем как «места нет».
        let bytes = self.capacity.saturating_sub(used) as usize;
        // Остаток до конца кольца от позиции записи (под возможный PAD).
        let tail = (self.capacity - (write & self.mask)) as usize;
        FreeSpace {
            bytes,
            max_frame: max_frame_for(bytes, tail),
            // Пустое кольцо принимает кадр при любом (отстающем) счётчике --
            // то же правило, что у записи (`admits`).
            messages: if used == 0 { messages.max(1) } else { messages },
            max_message_size: self.max_message_size as usize,
        }
    }

    /// Писатель: заявка на место под payload длины `payload_len` в текущей
    /// позиции записи (`arm_space_waiter` с `frame_need`: PAD + кадр, если
    /// кадр не помещается до конца кольца). Позицию записи меняет только
    /// этот писатель, поэтому заявка остаётся верной, пока он ждёт.
    pub(crate) fn arm_space_waiter_for(&self, payload_len: usize) {
        // Длина проверена вызывающим (`frame_len`); `min` -- только чтобы
        // испорченная длина не дала заявку больше кольца.
        let total = frame_size(payload_len.min(self.max_message_size as usize)) as u32;
        let write = self.header().write_pos.load(Ordering::Acquire);
        self.arm_space_waiter(self.frame_need(write, total));
    }

    /// Писатель: заявить, что ждём `need` свободных байт (кадр с
    /// выравниванием и, если нужен, PAD перед ним -- `frame_need`).
    ///
    /// Протокол «заявка -> перепроверка -> сон» (Dekker): писатель пишет
    /// заявку, ставит `fence(SeqCst)` и ЗАТЕМ перепроверяет место; читатель
    /// двигает `read_pos`, ставит `fence(SeqCst)` и ЗАТЕМ читает заявку
    /// (`take_space_waiter`). Два SeqCst-фенса гарантируют, что хотя бы одна
    /// сторона увидит запись другой: либо писатель при перепроверке увидит
    /// освобождённое место, либо читатель увидит заявку и просигналит
    /// событие -- потерянного пробуждения нет.
    pub(crate) fn arm_space_waiter(&self, need: u32) {
        // Relaxed: упорядочивание с последующей перепроверкой даёт fence ниже.
        self.header().space_waiter.store(need, Ordering::Relaxed);
        fence(Ordering::SeqCst);
    }

    /// Писатель: снять свою заявку (место нашлось без ожидания).
    pub(crate) fn disarm_space_waiter(&self) {
        // Relaxed: заявка -- только подсказка для пробуждения; лишний сигнал
        // события после снятия безвреден (писатель перепроверяет место).
        self.header().space_waiter.store(0, Ordering::Relaxed);
    }

    /// Читатель (после успешного `read_message`): если писатель ждёт места и
    /// его теперь достаточно -- атомарно снимает заявку и возвращает `true`
    /// (вызывающий сигналит событие `SPACE`).
    pub(crate) fn take_space_waiter(&self) -> bool {
        let header = self.header();
        // Парный фенс к `arm_space_waiter` (см. там): сдвиг read_pos в
        // read_message упорядочен перед чтением заявки.
        fence(Ordering::SeqCst);
        // Relaxed: упорядочивание с read_pos обеспечил фенс выше. Заявка --
        // байты под PAD + кадр в позиции записи писателя (`frame_need`),
        // поэтому сравнивается со всеми свободными байтами.
        let frame = header.space_waiter.load(Ordering::Relaxed);
        if frame == 0 {
            return false;
        }
        let free = self.free_space();
        if free.messages == 0 || free.bytes < frame as usize {
            // Места пока мало -- не будим писателя зря, заявка остаётся.
            return false;
        }
        // CAS, а не store: писатель мог уже снять/переподать заявку; сигналим,
        // только если сняли именно ту, которую проверяли. AcqRel -- по
        // конвенции для CAS, сам фенс выше уже дал нужный порядок.
        header
            .space_waiter
            .compare_exchange(frame, 0, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    }

    /// Прочитать старейшее сообщение в `out` (содержимое заменяется).
    /// Возвращает длину payload; `Err(QueueEmpty)` -- сообщений нет,
    /// `Err(Corrupted)` -- пир испортил кольцо (позиции/заголовок кадра).
    pub fn read_message(&self, out: &mut Vec<u8>) -> Result<usize> {
        self.read_message_checked(out, || true)
    }

    /// `read_message` со сверкой «сессия ещё наша» перед фиксацией (Task 6
    /// надёжности): `still_current()` вызывается после копирования, прямо
    /// перед CAS `read_pos`; `false` -- кадр НЕ забирается (он мог
    /// принадлежать уже следующей сессии канала), `out` очищается,
    /// `Err(NotConnected)`. Одна проверка на кадр (у клиента -- одна
    /// Acquire-загрузка `generation`), без системных вызовов.
    pub(crate) fn read_message_checked(
        &self,
        out: &mut Vec<u8>,
        still_current: impl Fn() -> bool,
    ) -> Result<usize> {
        loop {
            let (read, frame) = self.next_message()?;

            // ОПТИМИСТИЧНОЕ копирование ДО фиксации read_pos (seqlock-паттерн,
            // ADR 11). Если producer перезапишет слот во время копирования,
            // CAS ниже провалится, и мы отбросим эту (потенциально битую)
            // копию. Копирование -- сырыми указателями, без `&[u8]` на
            // разделяемую память: при перезаписи эти байты меняются
            // одновременно.
            out.clear();
            out.reserve(frame.len);
            // SAFETY: payload кадра лежит внутри кольца одним куском
            // (`frame_at`: кадр не пересекает границу), в `out` зарезервировано
            // не меньше frame.len байт, области не пересекаются.
            unsafe {
                std::ptr::copy_nonoverlapping(self.payload_ptr(read), out.as_mut_ptr(), frame.len);
            }
            // SAFETY: первые frame.len байт только что инициализированы
            // (копия выше), ёмкость зарезервирована.
            unsafe { out.set_len(frame.len) };

            // Барьер компилятора: копирование не должно «переехать» НИЖЕ CAS,
            // иначе валидация теряет смысл. На x86 успешный lock cmpxchg также
            // даёт аппаратный барьер.
            compiler_fence(Ordering::Release);

            // Сессия сменилась, пока шло чтение: кадр не наш -- не забираем
            // и не отдаём (Acquire-загрузка в `still_current` не опускается
            // ниже CAS фиксации).
            if !still_current() {
                out.clear();
                return Err(ShmError::NotConnected);
            }

            // Фиксация: атомарно забираем кадр. Провал => producer сдвинул
            // read_pos (перезапись) => скопированные байты невалидны.
            if self.commit_read(read, frame) {
                return Ok(frame.len);
            }
        }
    }

    /// Zero-copy чтение (0.10): `f` получает payload старейшего сообщения
    /// срезом прямо из кольца (кадр непрерывен -- PAD-кадры пропускаются),
    /// после возврата `f` кадр забирается (`read_pos` сдвигается).
    ///
    /// `Err(QueueEmpty)` -- сообщений нет, `Err(Corrupted)` -- пир испортил
    /// кольцо (в обоих случаях `f` не вызывается); `Err(Overrun)` -- писатель
    /// с перезаписью вытеснил кадр, пока шёл `f` (CAS `read_pos` не прошёл):
    /// результат `f` отброшен, сообщение потеряно, следующее читается
    /// следующим вызовом. Паника в `f` -- кадр остаётся непрочитанным.
    ///
    /// # Safety
    /// Пока выполняется `f`, никто не изменяет байты этого кадра. Срез
    /// указывает в разделяемую память: если её меняют во время `f`, это
    /// гонка данных (UB), а проверка «после проверки» в `f` может читать уже
    /// другие байты (TOCTOU: компилятор вправе перечитать «неизменяемую»
    /// память). На практике контракт выполняется, когда писатель этого
    /// направления -- честный пир xshm, пишущий БЕЗ перезаписи (`try_send*`,
    /// `write_with`, `wait_for_space`; не `send` с overwrite-oldest), и пир
    /// доверенный. Для недоверенных пиров -- копирующий `read_message`.
    /// `Overrun` -- лишь защита в глубину: о перезаписи он сообщает уже
    /// после `f`.
    ///
    /// Кроме того, вызывающий -- ЕДИНСТВЕННЫЙ читатель этого направления
    /// всё время `f`: никакого другого чтения (вложенного из `f` или из
    /// другого потока). Второе чтение забрало бы кадр и освободило место, и
    /// честный писатель без перезаписи затёр бы заимствованные байты, пока
    /// срез жив. Публичные обёртки (`Shared*::receive_with`) обеспечивают
    /// это заимствованием `&mut self`.
    pub(crate) unsafe fn read_with<R>(&self, f: impl FnOnce(&[u8]) -> R) -> Result<R> {
        // SAFETY: контракт `read_with` -- контракт этой функции.
        unsafe { self.read_with_checked(|| true, f) }
    }

    /// `read_with` со сверкой «сессия ещё наша» (Task 6 надёжности):
    /// `still_current()` -- после поиска кадра, ДО вызова `f`; `false` --
    /// `f` не вызывается, кадр не забирается, `Err(NotConnected)`.
    ///
    /// # Safety
    /// Контракт `read_with`.
    pub(crate) unsafe fn read_with_checked<R>(
        &self,
        still_current: impl Fn() -> bool,
        f: impl FnOnce(&[u8]) -> R,
    ) -> Result<R> {
        let (read, frame) = self.next_message()?;
        if !still_current() {
            return Err(ShmError::NotConnected);
        }
        // SAFETY: payload кадра -- `frame.len` байт внутри кольца одним
        // куском и внутри опубликованных данных (`frame_at`), выровнен на 8,
        // отображение живо всё время `&self`. Неизменность байтов на время
        // `f` -- контракт вызывающего (`# Safety`).
        let data = unsafe { std::slice::from_raw_parts(self.payload_ptr(read), frame.len) };
        let result = f(data);
        // Барьер компилятора: чтения из среза внутри `f` не должны
        // «переехать» ниже CAS (см. `read_message`).
        compiler_fence(Ordering::Release);
        if self.commit_read(read, frame) {
            Ok(result)
        } else {
            Err(ShmError::Overrun)
        }
    }

    /// Адрес payload кадра в позиции `read`.
    /// Для кадра, найденного `frame_at`, индекс кадра + заголовок + `len` <=
    /// capacity: кадр лежит внутри кольца целиком.
    const fn payload_ptr(&self, read: u32) -> *const u8 {
        self.data_at(self.mask_index(read) + FRAME_HEADER_SIZE)
            .cast_const()
    }

    /// Найти старейшее сообщение: пропустить PAD-кадр (CAS `read_pos` за
    /// него, без учёта), перепроверить пустоту по Dekker-протоколу.
    /// Возвращает позицию и кадр сообщения (ещё не забранного).
    ///
    /// Граница цикла по PAD: PAD всегда кончается на границе кольца и не
    /// стоит на индексе 0 (`frame_at`), поэтому после пропуска `read_pos`
    /// стоит на индексе 0, где PAD быть не может, -- два PAD подряд в
    /// опубликованных данных невозможны, и при неподвижных позициях
    /// пропуск ровно один на вызов. Каждая следующая итерация требует, чтобы
    /// писатель сам сдвинул позиции (публикация или вытеснение), -- та же
    /// живучесть, что у перепроверки пустоты.
    fn next_message(&self) -> Result<(u32, Frame)> {
        let header = self.header();
        loop {
            let read = header.read_pos.load(Ordering::Acquire);
            // Acquire: всё, что писатель записал до публикации write_pos
            // (заголовки PAD/кадра и payload), видно ниже.
            let write = header.write_pos.load(Ordering::Acquire);
            match self.frame_at(read, write) {
                Ok(Some(frame)) if frame.pad => {
                    // PAD -- не сообщение: сдвинуть read_pos за него и искать
                    // дальше. Провал CAS -- писатель вытеснил его сам.
                    let _ = header.read_pos.compare_exchange(
                        read,
                        read.wrapping_add(frame.total),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                }
                Ok(Some(frame)) => return Ok((read, frame)),
                // Пусто по снимку -- перепроверка после полного барьера
                // (вторая половина Dekker-пары с `publish`): кадр,
                // опубликованный после снимка, читаем сейчас, а не ждём
                // `DATA`, которого писатель тогда может не дать.
                Ok(None) if self.confirm_empty(write) => return Err(ShmError::QueueEmpty),
                Ok(None) => {}
                Err(err) => {
                    // Заголовок мог быть «порван» перезаписью писателя: если
                    // read_pos уже сдвинулся -- это гонка, повторяем.
                    if header.read_pos.load(Ordering::Acquire) != read {
                        continue;
                    }
                    return Err(err);
                }
            }
        }
    }

    /// Забрать сообщение `frame` в позиции `read`: CAS `read_pos`. `false` --
    /// писатель с перезаписью уже вытеснил кадр (прочитанное невалидно).
    fn commit_read(&self, read: u32, frame: Frame) -> bool {
        let header = self.header();
        if header
            .read_pos
            .compare_exchange(
                read,
                read.wrapping_add(frame.total),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        if self.decrement_count() <= 1 {
            // Relaxed: счётчик статистики переходов non-empty -> empty.
            header.sequence.fetch_add(1, Ordering::Relaxed);
        }
        true
    }

    /// Писатель: дождаться, пока в кольце появится место под payload длины
    /// `payload_len`. `Ok(true)` -- место есть (следующий `try_write_message`
    /// этого писателя пройдёт), `Ok(false)` -- истёк `timeout`,
    /// `Err(PeerDied)` -- процесс читателя умер (место не освободится
    /// никогда), `Err(NotConnected)` -- читатель штатно отключился
    /// (`DISCONNECT`). `timeout = None` -- ждать без ограничения: безопасно
    /// только при наблюдаемом пире (`peer`), иначе упавший читатель подвесит
    /// навсегда -- единственный таймаут тогда тот, что задал вызывающий.
    ///
    /// Ожидание -- ОДНО `NtWaitForMultipleObjects` на весь остаток таймаута
    /// по набору `[SPACE, DISCONNECT, процесс читателя]`. `SPACE` читатель
    /// сигналит, сняв заявку (`take_space_waiter`) или опустошив кольцо. Без
    /// событий вовсе (anonymous-сервер) -- опрос с шагом `SPACE_POLL_INTERVAL`.
    ///
    /// `spin` (0.10): до заявки -- один бюджет спина на вызов с
    /// перепроверкой места по позициям; заявка `space_waiter` во время спина
    /// НЕ ставится (читатель не тратит на нас сигнал). При нулевом таймауте
    /// спина нет: вызов не блокирующий. Спин уважает дедлайн `timeout`:
    /// перебег не больше `SPIN_DEADLINE_STRIDE` итераций (см. `spin_until`).
    pub(crate) fn wait_for_space(
        &self,
        payload_len: usize,
        space_event: Option<&EventHandle>,
        peer: Option<&ProcessWatch>,
        disconnect: Option<&DisconnectWatch<'_>>,
        timeout: Option<Duration>,
        spin: SpinPolicy,
    ) -> Result<bool> {
        self.frame_len(payload_len)?;
        let deadline = deadline_of(timeout);
        if timeout != Some(Duration::ZERO)
            && spin_until(spin, deadline, || self.free_space().fits(payload_len))
        {
            return Ok(true);
        }
        loop {
            if self.free_space().fits(payload_len) {
                return Ok(true);
            }
            // Заявка -- PAD + кадр в текущей позиции записи (её меняет только
            // этот писатель, пока он ждёт, -- никто).
            self.arm_space_waiter_for(payload_len);
            // Перепроверка ПОСЛЕ заявки -- вторая половина Dekker-протокола.
            if self.free_space().fits(payload_len) {
                self.disarm_space_waiter();
                return Ok(true);
            }
            let remaining = match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        self.disarm_space_waiter();
                        return Ok(false);
                    }
                    Some(remaining)
                }
                None => None,
            };

            let Some(space_event) = space_event else {
                // Anonymous: объектов ядра нет -- ждать нечего, кроме опроса.
                std::thread::sleep(
                    remaining.map_or(SPACE_POLL_INTERVAL, |r| r.min(SPACE_POLL_INTERVAL)),
                );
                if peer.is_some_and(ProcessWatch::has_exited)
                    && !self.free_space().fits(payload_len)
                {
                    self.disarm_space_waiter();
                    return Err(ShmError::PeerDied);
                }
                continue;
            };

            // Индекс 0 -- место (приоритет: при одновременном сигнале NT
            // отдаёт наименьший индекс), затем отключение, затем смерть.
            // Хвост массива -- заполнитель, в ожидание уходит `[..count]`.
            let mut handles = [space_event.as_handle(); 3];
            let mut count = 1;
            let disconnect_index = disconnect.map(|d| {
                handles[count] = d.event.as_handle();
                count += 1;
                count - 1
            });
            let peer_index = peer.map(|p| {
                handles[count] = p.as_handle();
                count += 1;
                count - 1
            });
            #[cfg(test)]
            SPACE_KERNEL_WAITS.with(|c| c.set(c.get() + 1));
            match wait_any(&handles[..count], remaining) {
                Err(err) => {
                    self.disarm_space_waiter();
                    return Err(err);
                }
                // Конец таймаута: верх цикла перепроверит место и дедлайн.
                Ok(None) => {}
                // SPACE: верх цикла перепроверит место. Места нет -- сигнал
                // пустой; процесс читателя стоит в наборе после `SPACE`, и
                // поток пустых сигналов скрывал бы его смерть до таймаута
                // (Task 10 N1, П1): одна проверка -- нулевое ожидание.
                Ok(Some(0)) => {
                    if !self.free_space().fits(payload_len)
                        && peer.is_some_and(ProcessWatch::has_exited)
                    {
                        self.disarm_space_waiter();
                        return Err(ShmError::PeerDied);
                    }
                }
                Ok(Some(i)) if Some(i) == disconnect_index => {
                    if let Some(d) = disconnect
                        && (d.peer_left)()
                    {
                        // Событие одно на обе стороны и автосбросное: мы его
                        // поглотили -- возвращаем взведённым для остальных
                        // ждущих (worker, `poll_*` другого потока).
                        let _ = d.event.set();
                        self.disarm_space_waiter();
                        return Err(ShmError::NotConnected);
                    }
                    // Устаревший сигнал прошлой сессии -- поглощён, ждём
                    // дальше. Процесс читателя -- за ним в наборе: смерть,
                    // скрытая потоком устаревших сигналов, видна здесь (N1).
                    if !self.free_space().fits(payload_len)
                        && peer.is_some_and(ProcessWatch::has_exited)
                    {
                        self.disarm_space_waiter();
                        return Err(ShmError::PeerDied);
                    }
                }
                Ok(Some(i)) => {
                    debug_assert_eq!(Some(i), peer_index);
                    // Читатель мог успеть освободить место перед смертью --
                    // тогда честно отвечаем «место есть».
                    if !self.free_space().fits(payload_len) {
                        self.disarm_space_waiter();
                        return Err(ShmError::PeerDied);
                    }
                }
            }
        }
    }

    /// Нет опубликованных непрочитанных данных (`write_pos == read_pos`).
    ///
    /// Для читателя «пусто» подтверждается после полного барьера
    /// (`confirm_empty`): ответ `true` перед сном на `DATA` (`poll_*`)
    /// безопасен -- кадр, опубликованный позже, писатель просигналит.
    pub fn is_empty(&self) -> bool {
        let header = self.header();
        let read = header.read_pos.load(Ordering::Acquire);
        let write = header.write_pos.load(Ordering::Acquire);
        read == write && self.confirm_empty(write)
    }

    /// Проба непрочитанных данных этого кольца для спина (см.
    /// `UnreadProbe`).
    pub(crate) const fn unread_probe(&self) -> UnreadProbe<'_> {
        UnreadProbe {
            header: self.header,
            _ring: PhantomData,
        }
    }

    /// Дешёвая проверка «есть непрочитанное» по позициям, без барьера --
    /// для спина (`UnreadProbe::has_unread`). Решение «пусто, можно спать»
    /// принимает только `is_empty`.
    pub(crate) fn has_unread(&self) -> bool {
        self.unread_probe().has_unread()
    }

    /// Читатель: граница дочитывания ушедшего пира -- снимки
    /// опубликованного `write_pos` и `read_pos` (Acquire) и лимит кадров.
    ///
    /// Лимит не зависит от настройки `max_messages` (её может задать
    /// большой пользователь): `min(max_messages, capacity /
    /// frame_size(MIN_MESSAGE_SIZE)) + 1` -- не больше кадров, чем физически
    /// помещается в кольце (+1 -- PAD перед кадром с индекса 0).
    pub(crate) fn drain_snapshot(&self) -> DrainBound {
        let header = self.header();
        // `frame_size(MIN_MESSAGE_SIZE)` = 16 -- в u32 без потерь.
        let min_frame = u32::try_from(frame_size(MIN_MESSAGE_SIZE)).unwrap_or(u32::MAX);
        let physical = self.capacity / min_frame;
        DrainBound {
            until: header.write_pos.load(Ordering::Acquire),
            read: header.read_pos.load(Ordering::Acquire),
            left: self.max_messages.min(physical).saturating_add(1),
        }
    }

    /// Дочитывание ушедшего пира (Multi): прочитать следующее сообщение до
    /// границы `bound` в `out`. `Ok(Some(len))` -- сообщение, `Ok(None)` --
    /// граница достигнута (снимок `write_pos` или лимит кадров),
    /// `Err(Corrupted)` -- пир сдвинул `read_pos` (общий не равен своей
    /// позиции `bound.read`) или испортил кадр.
    ///
    /// Кадры проверяются по снимку `write_pos`, а не по текущему: дописанное
    /// после снимка не читается вовсе. Своя позиция сдвигается только за
    /// кадры, забранные этим читателем (CAS `read_pos` со своей позиции):
    /// перемотанный пиром `read_pos` не вернёт уже доставленный кадр.
    pub(crate) fn drain_message(
        &self,
        out: &mut Vec<u8>,
        bound: &mut DrainBound,
    ) -> Result<Option<usize>> {
        let header = self.header();
        loop {
            let read = bound.read;
            if header.read_pos.load(Ordering::Acquire) != read {
                return Err(ShmError::Corrupted);
            }
            if bound.left == 0 || bound.until.wrapping_sub(read).cast_signed() <= 0 {
                return Ok(None);
            }
            bound.left -= 1;
            let Some(frame) = self.frame_at(read, bound.until)? else {
                return Ok(None);
            };
            let next = read.wrapping_add(frame.total);
            if frame.pad {
                // PAD -- не сообщение: забрать CAS-ом со своей позиции.
                if header
                    .read_pos
                    .compare_exchange(read, next, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    return Err(ShmError::Corrupted);
                }
                bound.read = next;
                continue;
            }
            // Копирование до фиксации (как `read_message`): перезапись во
            // время копирования провалит CAS ниже.
            out.clear();
            out.reserve(frame.len);
            // SAFETY: payload кадра лежит внутри кольца одним куском
            // (`frame_at`), в `out` зарезервировано не меньше frame.len байт,
            // области не пересекаются.
            unsafe {
                std::ptr::copy_nonoverlapping(self.payload_ptr(read), out.as_mut_ptr(), frame.len);
            }
            // SAFETY: первые frame.len байт только что инициализированы.
            unsafe { out.set_len(frame.len) };
            compiler_fence(Ordering::Release);
            if !self.commit_read(read, frame) {
                // Пир сдвинул `read_pos` во время копирования.
                return Err(ShmError::Corrupted);
            }
            bound.read = next;
            return Ok(Some(frame.len));
        }
    }

    /// Читатель: дождаться данных (`poll_*`). `Ok(true)` -- в кольце есть
    /// кадр ИЛИ сигнален `DATA` (возможно, устаревший: вызывающий читает
    /// до `QueueEmpty`), `Ok(false)` -- истёк `timeout`, `Err(PeerDied)` --
    /// процесс писателя завершился и всё, что он записал, прочитано,
    /// `Err(NotConnected)` -- писатель штатно ушёл (`DISCONNECT` при
    /// `peer_left`) и всё, что он записал, прочитано.
    ///
    /// Порядок: `is_empty` (с барьером, Dekker-пара с `publish`), затем
    /// проверка ухода по разделяемой памяти (`peer_left`: пир, ушедший до
    /// вызова, не требует ни спина, ни ожидания), затем `spin` -- один
    /// бюджет на вызов с проверкой по позициям, затем ожидание в ядре по `[DATA,
    /// DISCONNECT, процесс писателя]` (меньший индекс важнее: при
    /// одновременном сигнале последние данные ушедшего пира не теряются).
    /// Спин, увидевший кадр, возвращает `Ok(true)` без ядра; `DATA`,
    /// который писатель мог при этом взвести, остаётся взведённым и даст
    /// позже одно лишнее пробуждение -- сбрасывать его нельзя (можно
    /// потерять сигнал следующего кадра). При нулевом таймауте спина нет;
    /// дедлайн `timeout` спин сверяет раз в `SPIN_DEADLINE_STRIDE` итераций.
    ///
    /// `DISCONNECT` -- по правилу `DisconnectWatch` (как `wait_for_space`):
    /// при `peer_left` событие возвращается взведённым (оно одно на обе
    /// стороны), при живом пире -- устаревший сигнал прошлой сессии,
    /// поглощён, ожидание продолжается остатком таймаута (одно лишнее
    /// пробуждение -- только на событие пира, не таймер). Пир пишет данные
    /// до перевода состояния в `IDLE` (R14), поэтому перепроверка кольца
    /// после `peer_left` видит всё, что он успел записать.
    pub(crate) fn wait_for_data(
        &self,
        data: &EventHandle,
        peer: Option<&ProcessWatch>,
        disconnect: Option<&DisconnectWatch<'_>>,
        timeout: Option<Duration>,
        spin: SpinPolicy,
    ) -> Result<bool> {
        if !self.is_empty() {
            return Ok(true);
        }
        // Пир уже ушёл (его сигнал мог достаться другому ждущему, который
        // вернул его, или ещё в пути): решаем по памяти, событие не трогаем.
        // До спина -- ушедший пир ничего не допишет, бюджет спина не тратим.
        if let Some(d) = disconnect
            && (d.peer_left)()
        {
            return self.left_outcome(ShmError::NotConnected);
        }
        let deadline = deadline_of(timeout);
        if timeout != Some(Duration::ZERO) && spin_until(spin, deadline, || self.has_unread()) {
            return Ok(true);
        }

        // Индекс 0 -- данные, затем отключение, затем смерть. Хвост массива
        // -- заполнитель, в ожидание уходит `[..count]`.
        let mut handles = [data.as_handle(); 3];
        let mut count = 1;
        let disconnect_index = disconnect.map(|d| {
            handles[count] = d.event.as_handle();
            count += 1;
            count - 1
        });
        if let Some(peer) = peer {
            handles[count] = peer.as_handle();
            count += 1;
        }
        loop {
            // Остаток до дедлайна; таймаут, не представимый как `Instant`,
            // -- без ограничения (как `deadline_of`).
            let remaining = deadline.map(|at| at.saturating_duration_since(Instant::now()));
            #[cfg(test)]
            DATA_KERNEL_WAITS.with(|c| c.set(c.get() + 1));
            match wait_any(&handles[..count], remaining)? {
                Some(0) => {
                    // Пустой `DATA` (кольцо пусто): процесс писателя стоит в
                    // наборе после него, и поток пустых сигналов скрывал бы
                    // его смерть (Task 10 N1, П1) -- одна проверка, только
                    // на пустом сигнале (на сигнале с данными -- ни одной).
                    if self.is_empty() && peer.is_some_and(ProcessWatch::has_exited) {
                        return self.left_outcome(ShmError::PeerDied);
                    }
                    return Ok(true);
                }
                None => return Ok(false),
                Some(i) if Some(i) == disconnect_index => {
                    if let Some(d) = disconnect
                        && (d.peer_left)()
                    {
                        // Событие одно на обе стороны и автосбросное: мы его
                        // поглотили -- возвращаем взведённым для остальных
                        // ждущих (`wait_for_space`, worker).
                        let _ = d.event.set();
                        return self.left_outcome(ShmError::NotConnected);
                    }
                    // Устаревший сигнал прошлой сессии -- поглощён. Процесс
                    // писателя -- за ним в наборе: смерть, скрытая потоком
                    // устаревших сигналов, видна здесь (Task 10 N1). Дедлайн
                    // истёк -- таймаут без лишнего ожидания в ядре.
                    if peer.is_some_and(ProcessWatch::has_exited) {
                        return self.left_outcome(ShmError::PeerDied);
                    }
                    if deadline.is_some_and(|at| Instant::now() >= at) {
                        return Ok(false);
                    }
                }
                // Процесс писателя завершился.
                Some(_) => return self.left_outcome(ShmError::PeerDied),
            }
        }
    }

    /// Писатель ушёл (`reason`): пока в кольце есть недочитанное --
    /// `Ok(true)` (сначала дочитать), пусто -- `Err(reason)`.
    fn left_outcome(&self, reason: ShmError) -> Result<bool> {
        if self.is_empty() {
            Err(reason)
        } else {
            Ok(true)
        }
    }
}

#[cfg(test)]
mod overflow_race_tests {
    use super::*;

    pub(super) const RING_CAPACITY: usize = DEFAULT_RING_CAPACITY;
    use crate::layout::RingHeader;
    use std::alloc::{Layout, alloc_zeroed, dealloc};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as O};
    use std::thread;

    /// Владелец сырой выровненной памяти под один RingHeader + RING_CAPACITY.
    pub(super) struct RingMem {
        ptr: *mut u8,
        layout: Layout,
    }
    // SAFETY: владеет единственным блоком памяти, освобождает его ровно один раз
    // в Drop; доступ к содержимому идёт только через `RingBuffer` (атомарные
    // операции), поэтому передача владения между потоками безопасна.
    unsafe impl Send for RingMem {}
    // SAFETY: см. выше -- сам `RingMem` не даёт доступа к памяти, он только
    // владеет ей и освобождает.
    unsafe impl Sync for RingMem {}
    impl Drop for RingMem {
        fn drop(&mut self) {
            // SAFETY: ptr/layout получены из alloc_zeroed в make_ring.
            unsafe { dealloc(self.ptr, self.layout) };
        }
    }

    pub(super) fn make_ring() -> (RingBuffer, RingMem) {
        make_ring_with(&RingConfig::DEFAULT)
    }

    /// Кольцо с произвольной (проверенной) конфигурацией.
    pub(super) fn make_ring_with(config: &RingConfig) -> (RingBuffer, RingMem) {
        config.validate().unwrap();
        let header_size = size_of::<RingHeader>();
        let total = header_size + config.capacity;
        let layout = Layout::from_size_align(total, 64).unwrap();
        // SAFETY: ненулевой размер; зануление валидно для AtomicU32 полей.
        let ptr = unsafe { alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "alloc failed");
        let header = ptr.cast::<RingHeader>();
        // SAFETY: ptr выровнен на 64 и указывает на зануленный RingHeader.
        unsafe { (*header).reset(1) };
        // SAFETY: data сразу за заголовком, в пределах выделения.
        let data = unsafe { ptr.add(header_size) };
        // SAFETY: header и data валидны, не пересекаются, живут пока жив RingMem.
        let ring = unsafe { RingBuffer::new(header, data, config) }.unwrap();
        (ring, RingMem { ptr, layout })
    }

    // Большие сообщения: ~34 сообщения заполняют 2 МБ кольца ПО БАЙТАМ
    // (а не по счётчику MAX_MESSAGES=500). Torn-read возможен только в
    // байт-заполненном режиме, где write_pos & MASK == read_pos & MASK и
    // producer физически перезаписывает слот, который читает consumer.
    // С маленькими сообщениями переполнение наступает по счётчику задолго
    // до байтового, write далеко впереди read, и гонка не открывается.
    const PAYLOAD: usize = 60_000;
    // seq-маркеры в трёх точках сообщения. Если producer перезапишет слот
    // в середине копирования, маркеры начала/середины/конца разойдутся.
    const MARK0: usize = 0;
    const MARK1: usize = PAYLOAD / 2;
    const MARK2: usize = PAYLOAD - 4;

    /// Проставить seq в три маркера переиспользуемого буфера (без аллокаций
    /// в горячем цикле — producer должен быть быстрым, чтобы успевать
    /// перезаписывать слот во время копирования consumer-ом).
    fn stamp(buf: &mut [u8], seq: u32) {
        let s = seq.to_le_bytes();
        buf[MARK0..MARK0 + 4].copy_from_slice(&s);
        buf[MARK1..MARK1 + 4].copy_from_slice(&s);
        buf[MARK2..MARK2 + 4].copy_from_slice(&s);
    }

    /// Err, если сообщение «порвано»: маркеры начала/середины/конца не совпали.
    // Примечание: `use super::*` втягивает crate-овый `Result<T>` (ошибка = ShmError),
    // поэтому здесь используем полностью квалифицированный std-Result для String-ошибки.
    fn check(msg: &[u8]) -> std::result::Result<(), String> {
        if msg.len() != PAYLOAD {
            return Err(format!("bad len {}", msg.len()));
        }
        let m0 = u32::from_le_bytes([msg[MARK0], msg[MARK0 + 1], msg[MARK0 + 2], msg[MARK0 + 3]]);
        let m1 = u32::from_le_bytes([msg[MARK1], msg[MARK1 + 1], msg[MARK1 + 2], msg[MARK1 + 3]]);
        let m2 = u32::from_le_bytes([msg[MARK2], msg[MARK2 + 1], msg[MARK2 + 2], msg[MARK2 + 3]]);
        if m0 != m1 || m0 != m2 {
            return Err(format!("torn: m0={m0} m1={m1} m2={m2}"));
        }
        Ok(())
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "seqlock-чтение при перезаписи -- гонка по модели Rust by design"
    )]
    fn overflow_does_not_tear_messages() {
        let (ring, _mem) = make_ring();
        let ring = Arc::new(ring);
        let stop = Arc::new(AtomicBool::new(false));
        let torn = Arc::new(AtomicU64::new(0));
        let reads = Arc::new(AtomicU64::new(0));

        let producer = {
            let ring = ring.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                // Переиспользуемый буфер: producer на полной скорости держит
                // кольцо байт-заполненным, постоянно перезаписывая старое.
                let mut buf = vec![0u8; PAYLOAD];
                let mut seq: u32 = 1;
                while !stop.load(O::Acquire) {
                    stamp(&mut buf, seq);
                    let _ = ring.write_message(&buf); // overwrite разрешён
                    seq = seq.wrapping_add(1);
                }
            })
        };

        let consumer = {
            let stop = stop.clone();
            let torn = torn.clone();
            let reads = reads.clone();
            thread::spawn(move || {
                let mut out = Vec::with_capacity(PAYLOAD);
                while !stop.load(O::Acquire) {
                    match ring.read_message(&mut out) {
                        Ok(_) => {
                            if check(&out).is_err() {
                                torn.fetch_add(1, O::AcqRel);
                            }
                            reads.fetch_add(1, O::AcqRel);
                            // Лёгкая задержка: consumer чуть медленнее producer-а
                            // -> кольцо остаётся заполненным -> producer пишет
                            // ровно в слот, который мы читаем.
                            for _ in 0..400 {
                                std::hint::spin_loop();
                            }
                        }
                        Err(_) => thread::yield_now(),
                    }
                }
            })
        };

        thread::sleep(Duration::from_millis(800));
        stop.store(true, O::Release);
        producer.join().unwrap();
        consumer.join().unwrap();

        let reads = reads.load(O::Acquire);
        let torn = torn.load(O::Acquire);
        assert!(reads > 0, "consumer не прочитал ни одного сообщения");
        assert_eq!(
            torn, 0,
            "обнаружены порванные сообщения: {torn} (из {reads} прочитанных)"
        );
    }
}

#[cfg(test)]
mod try_write_tests {
    use super::overflow_race_tests::{RING_CAPACITY, make_ring, make_ring_with};
    use super::*;

    const MAX_MESSAGES: u32 = DEFAULT_MAX_MESSAGES;
    const MAX_MESSAGE_SIZE: usize = DEFAULT_MAX_MESSAGE_SIZE;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as O};
    use std::thread;

    /// Детерминированный xorshift64* -- без внешних зависимостей (proptest
    /// в крейт не тянем: правило «никаких зависимостей без крайней нужды»).
    pub(super) struct Rng(pub(super) u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        pub(super) fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    pub(super) fn drop_count(ring: &RingBuffer) -> u32 {
        ring.header().drop_count.load(O::Acquire)
    }

    /// Payload с seq в начале/середине/конце и детерминированным телом --
    /// по нему видно и потерю/перестановку, и порванное сообщение.
    pub(super) fn make_payload(seq: u32, len: usize) -> Vec<u8> {
        let mut v: Vec<u8> = (0..len)
            .map(|i| (i as u32).wrapping_mul(31).wrapping_add(seq) as u8)
            .collect();
        if len >= 12 {
            let s = seq.to_le_bytes();
            v[..4].copy_from_slice(&s);
            let mid = len / 2 - 2;
            v[mid..mid + 4].copy_from_slice(&s);
            v[len - 4..].copy_from_slice(&s);
        }
        v
    }

    #[test]
    fn free_space_of_empty_ring() {
        let (ring, _mem) = make_ring();
        let free = ring.free_space();
        assert_eq!(free.bytes, RING_CAPACITY);
        assert_eq!(free.messages, MAX_MESSAGES);
        assert_eq!(free.max_payload(), MAX_MESSAGE_SIZE);
        assert!(free.fits(MAX_MESSAGE_SIZE));
    }

    #[test]
    fn free_space_helpers_edge_cases() {
        let fs = |bytes, messages| FreeSpace {
            bytes,
            max_frame: bytes,
            messages,
            max_message_size: MAX_MESSAGE_SIZE,
        };
        assert_eq!(FreeSpace::ZERO.max_payload(), 0);
        assert!(!FreeSpace::ZERO.fits(MIN_MESSAGE_SIZE));
        // Хватает ровно на минимальный кадр (8 + 1 -> 16 с выравниванием).
        assert!(fs(16, 1).fits(8));
        assert_eq!(fs(16, 1).max_payload(), 8);
        assert!(!fs(15, 1).fits(1));
        assert_eq!(fs(15, 1).max_payload(), 0);
        // Байт много, слотов нет.
        assert!(!fs(RING_CAPACITY, 0).fits(10));
        assert_eq!(fs(RING_CAPACITY, 0).max_payload(), 0);
        // Недопустимые длины не «влезают» никогда.
        assert!(!fs(RING_CAPACITY, 10).fits(0));
        assert!(!fs(RING_CAPACITY, 10).fits(MAX_MESSAGE_SIZE + 1));
        assert_eq!(fs(1000, 1).max_payload(), 992);
    }

    #[test]
    fn try_write_rejects_bad_lengths_without_side_effects() {
        let (ring, _mem) = make_ring();
        assert_eq!(
            ring.try_write_message(&[]).err(),
            Some(ShmError::MessageTooSmall)
        );
        assert_eq!(
            ring.try_write_message(&vec![0u8; MAX_MESSAGE_SIZE + 1])
                .err(),
            Some(ShmError::MessageTooLarge)
        );
        assert_eq!(ring.free_space().bytes, RING_CAPACITY);
        assert!(ring.try_write_message(&vec![7u8; MAX_MESSAGE_SIZE]).is_ok());
    }

    #[test]
    fn free_space_tracks_writes_and_reads_exactly() {
        let (ring, _mem) = make_ring();
        let mut out = Vec::new();
        ring.try_write_message(&[1u8; 100]).unwrap();
        ring.try_write_message(&[2u8; 10]).unwrap();
        let free = ring.free_space();
        assert_eq!(free.bytes, RING_CAPACITY - frame_size(100) - frame_size(10));
        assert_eq!(free.messages, MAX_MESSAGES - 2);
        ring.read_message(&mut out).unwrap();
        assert_eq!(ring.free_space().bytes, RING_CAPACITY - frame_size(10));
        assert_eq!(ring.free_space().messages, MAX_MESSAGES - 1);
    }

    /// Кольцо, заполненное по БАЙТАМ: try_write отвечает QueueFull, ничего
    /// не вытесняет; после чтения одного сообщения запись снова проходит.
    #[test]
    fn try_write_full_by_bytes_never_overwrites_and_resumes() {
        let (ring, _mem) = make_ring();
        const LEN: usize = 60_000;
        let mut written = 0u32;
        loop {
            match ring.try_write_message(&make_payload(written, LEN)) {
                Ok(outcome) => {
                    assert_eq!(outcome.overwritten, 0);
                    written += 1;
                }
                Err(ShmError::QueueFull) => break,
                Err(err) => panic!("unexpected {err:?}"),
            }
        }
        assert_eq!(written as usize, RING_CAPACITY / frame_size(LEN));
        let before = ring.free_space();
        assert!(!before.fits(LEN));
        assert_eq!(drop_count(&ring), 0);
        // Повторные попытки ничего не меняют.
        for _ in 0..3 {
            assert_eq!(
                ring.try_write_message(&make_payload(999, LEN)).err(),
                Some(ShmError::QueueFull)
            );
        }
        assert_eq!(ring.free_space(), before);
        assert_eq!(ring.header().message_count.load(O::Acquire), written);

        let mut out = Vec::new();
        ring.read_message(&mut out).unwrap();
        assert_eq!(
            out,
            make_payload(0, LEN),
            "первое сообщение не должно быть затёрто"
        );
        assert!(ring.free_space().fits(LEN));
        ring.try_write_message(&make_payload(written, LEN)).unwrap();
        // Всё прочитанное -- строго по порядку, без пропусков.
        for seq in 1..=written {
            ring.read_message(&mut out).unwrap();
            assert_eq!(out, make_payload(seq, LEN));
        }
        assert!(ring.is_empty());
        assert_eq!(drop_count(&ring), 0);
    }

    /// Граница доверия (аудит 2026-10-16): пир пишет мусор в заголовок
    /// кольца или кадра -- читатель отвечает `Corrupted`, не выходя за
    /// опубликованные данные, не зацикливаясь и не паникуя.
    #[test]
    fn hostile_reader_side_corruption_is_detected() {
        let (ring, _mem) = make_ring();
        let header = ring.header();
        let mut out = Vec::new();

        // Длина кадра больше лимита кольца.
        ring.try_write_message(b"payload").unwrap();
        ring.header_word(0).store(u32::MAX, O::Relaxed);
        assert_eq!(ring.read_message(&mut out), Err(ShmError::Corrupted));

        // Кадр длиннее опубликованных данных (write_pos указывает внутрь).
        ring.header_word(0).store(64, O::Relaxed);
        assert_eq!(ring.read_message(&mut out), Err(ShmError::Corrupted));

        // Занятость больше ёмкости.
        header
            .write_pos
            .store(RING_CAPACITY as u32 + 64, O::Relaxed);
        assert_eq!(ring.read_message(&mut out), Err(ShmError::Corrupted));
        assert_eq!(ring.free_space().bytes, 0);
        assert_eq!(ring.try_write_message(b"xx"), Err(ShmError::Corrupted));
        assert_eq!(ring.write_message(b"xx"), Err(ShmError::Corrupted));

        // Невыровненная позиция чтения.
        header.write_pos.store(64, O::Relaxed);
        header.read_pos.store(3, O::Relaxed);
        assert_eq!(ring.read_message(&mut out), Err(ShmError::Corrupted));
    }

    /// Регрессия (security review 2026-10-16): читатель-пир ставит
    /// невыровненную позицию записи у конца кольца -- раньше `commit`
    /// писал второе слово заголовка кадра за пределы кольца.
    #[test]
    fn hostile_writer_position_cannot_write_out_of_bounds() {
        let (ring, _mem) = make_ring();
        let header = ring.header();
        let edge = RING_CAPACITY as u32 - 4;
        header.write_pos.store(edge, O::Relaxed);
        header.read_pos.store(edge, O::Relaxed);
        assert_eq!(ring.try_write_message(b"xx"), Err(ShmError::Corrupted));
        assert_eq!(ring.write_message(b"xx"), Err(ShmError::Corrupted));
        // Выровненная позиция записи при невыровненной позиции чтения.
        header.write_pos.store(64, O::Relaxed);
        header.read_pos.store(60, O::Relaxed);
        assert_eq!(ring.try_write_message(b"xx"), Err(ShmError::Corrupted));
        assert_eq!(ring.write_message(b"xx"), Err(ShmError::Corrupted));
    }

    /// Испорченный счётчик сообщений: пустое кольцо со «счётчиком» на
    /// пределе не зацикливает запись с перезаписью, а нулевой счётчик при
    /// чтении не уходит в `u32::MAX` (кольцо навсегда «полно»).
    #[test]
    fn hostile_message_count_is_contained() {
        let (ring, _mem) = make_ring();
        let header = ring.header();
        let mut out = Vec::new();

        // Счётчик -- подсказка: пустое по позициям кольцо принимает кадр при
        // любом его значении (ни `Corrupted`, ни `QueueFull`).
        header.message_count.store(u32::MAX, O::Relaxed);
        assert!(ring.free_space().fits(2));
        let outcome = ring.write_message(b"xx").unwrap();
        assert_eq!(outcome.overwritten, 0);
        assert_eq!(ring.read_message(&mut out), Ok(2));
        header.message_count.store(u32::MAX, O::Relaxed);
        ring.try_write_message(b"yy").unwrap();
        assert_eq!(ring.read_message(&mut out), Ok(2));
        assert_eq!(ring.read_message(&mut out), Err(ShmError::QueueEmpty));

        // Непустое кольцо при «исчерпанном» счётчике, лимит 500 выше
        // `RECOUNT_MAX` (правило по счётчику, O(1)): try_write отказывает,
        // write вытесняет до пустого кольца и пишет -- цикл конечен.
        // Пересчёт по кадрам при малом лимите -- `lagging_count_tests`.
        ring.try_write_message(b"a1").unwrap();
        ring.try_write_message(b"a2").unwrap();
        header.message_count.store(u32::MAX, O::Relaxed);
        assert_eq!(ring.try_write_message(b"zz"), Err(ShmError::QueueFull));
        let outcome = ring.write_message(b"zz").unwrap();
        assert_eq!(outcome.overwritten, 2);
        assert!(outcome.was_empty, "вытеснение опустошило кольцо");
        assert_eq!(ring.read_message(&mut out), Ok(2));
        assert_eq!(out, b"zz");
        assert_eq!(ring.read_message(&mut out), Err(ShmError::QueueEmpty));

        header.message_count.store(0, O::Relaxed);
        ring.try_write_message(b"one").unwrap();
        header.message_count.store(0, O::Relaxed);
        assert_eq!(ring.read_message(&mut out), Ok(3));
        assert_eq!(
            header.message_count.load(O::Relaxed),
            0,
            "без ухода ниже нуля"
        );
        assert!(ring.free_space().fits(8));
    }

    /// Кольцо, заполненное по СЧЁТЧИКУ (`MAX_MESSAGES`), при почти пустых байтах.
    /// Лимит 500 выше `RECOUNT_MAX`: запись принимает ещё одно сообщение
    /// (запас на законное отставание счётчика, решение 28), `free_space`
    /// консервативно показывает 0.
    #[test]
    fn try_write_full_by_count() {
        let (ring, _mem) = make_ring();
        for i in 0..MAX_MESSAGES {
            ring.try_write_message(&i.to_le_bytes()).unwrap();
        }
        let free = ring.free_space();
        assert_eq!(free.messages, 0);
        assert!(free.bytes > RING_CAPACITY / 2);
        ring.try_write_message(b"+1").unwrap();
        assert_eq!(
            ring.try_write_message(b"xx").err(),
            Some(ShmError::QueueFull)
        );
        let mut out = Vec::new();
        ring.read_message(&mut out).unwrap();
        assert_eq!(out, 0u32.to_le_bytes());
        ring.try_write_message(b"xx").unwrap();
        assert_eq!(drop_count(&ring), 0);
    }

    /// Итог прогона модели кадрирования.
    pub(super) struct ModelRun {
        /// Сколько раз запись вставила PAD-кадр.
        pub(super) pads: u32,
        /// Сколько раз запись упёрлась в полное кольцо.
        pub(super) fulls: u32,
        /// Сколько раз позиция записи обошла кольцо.
        pub(super) wraps: usize,
    }

    /// Рандомизированная модель кадрирования (VecDeque + позиция записи):
    /// случайные длины (с упором на границы `1`/`max_message_size`),
    /// случайное чередование записи (`try_write_message` или `write_with`)
    /// и чтения (`read`), многократный wrap позиций. Модель знает правило
    /// PAD (0.10): кадр, не помещающийся непрерывно до конца кольца,
    /// предваряется PAD-кадром до границы, и место под запись -- PAD + кадр.
    /// Однопоточно предсказание `QueueFull` точное, поэтому модель сверяет
    /// и сам факт отказа, и `free_space`, и содержимое каждого сообщения.
    pub(super) fn run_framing_model(
        config: &RingConfig,
        steps: usize,
        seed: u64,
        read: impl Fn(&RingBuffer) -> Result<Vec<u8>>,
    ) -> ModelRun {
        let (ring, _mem) = make_ring_with(config);
        let cap = config.capacity;
        let max_len = config.max_message_size;
        let mut rng = Rng(seed);
        // Сообщение и PAD перед ним (0 -- без PAD).
        let mut model: VecDeque<(Vec<u8>, usize)> = VecDeque::new();
        let mut used = 0usize;
        let mut write_index = 0usize;
        let mut seq = 0u32;
        let (mut pads, mut fulls) = (0u32, 0u32);
        for step in 0..steps {
            if rng.below(100) < 55 {
                let len = match rng.below(10) {
                    0 => MIN_MESSAGE_SIZE,
                    1 => max_len,
                    2..=5 => 1 + rng.below(64) as usize,
                    _ => 1 + rng.below(max_len as u64) as usize,
                };
                let payload = make_payload(seq, len);
                let frame = frame_size(len);
                let tail = cap - write_index;
                let pad = if frame <= tail { 0 } else { tail };
                let fits_model = used + pad + frame <= cap
                    && (model.is_empty() || (model.len() as u64) < u64::from(config.max_messages));
                assert_eq!(ring.free_space().fits(len), fits_model, "шаг {step}");
                let result = if rng.below(2) == 0 {
                    ring.try_write_message(&payload)
                } else {
                    // SAFETY: однопоточная модель -- этот поток единственный
                    // писатель, `f` в кольцо не пишет.
                    unsafe { ring.write_with(len, |buf| buf.copy_from_slice(&payload)) }
                };
                match result {
                    Ok(o) => {
                        assert!(fits_model, "запись прошла, хотя места нет по модели");
                        assert_eq!(o.overwritten, 0);
                        assert_eq!(o.was_empty, model.is_empty());
                        used += pad + frame;
                        write_index = (write_index + pad + frame) % cap;
                        if pad > 0 {
                            pads += 1;
                        }
                        model.push_back((payload, pad));
                        seq += 1;
                    }
                    Err(ShmError::QueueFull) => {
                        assert!(!fits_model, "ложный QueueFull на шаге {step}");
                        fulls += 1;
                    }
                    Err(err) => panic!("unexpected {err:?}"),
                }
            } else {
                match read(&ring) {
                    Ok(got) => {
                        let (expect, pad) = model.pop_front().expect("кольцо отдало лишнее");
                        assert_eq!(got.len(), expect.len());
                        assert_eq!(got, expect, "шаг {step}");
                        used -= pad + frame_size(got.len());
                    }
                    Err(ShmError::QueueEmpty) => assert!(model.is_empty()),
                    Err(err) => panic!("unexpected {err:?}"),
                }
            }
            let free = ring.free_space();
            assert_eq!(free.bytes, cap - used, "шаг {step}");
            assert_eq!(
                free.messages,
                config.max_messages.saturating_sub(model.len() as u32)
            );
            assert_eq!(
                ring.header().message_count.load(O::Acquire),
                model.len() as u32,
                "PAD не считается сообщением"
            );
        }
        assert_eq!(drop_count(&ring), 0);
        ModelRun {
            pads,
            fulls,
            wraps: ring.header().write_pos.load(O::Acquire) as usize / cap,
        }
    }

    /// Модель кадрирования на кольце по умолчанию с копирующим чтением.
    #[test]
    fn randomized_framing_matches_model() {
        let steps = if cfg!(miri) { 300 } else { 60_000 };
        let run = run_framing_model(&RingConfig::DEFAULT, steps, 0x9E37_79B9_7F4A_7C15, |ring| {
            let mut out = Vec::new();
            ring.read_message(&mut out).map(|_| out)
        });
        if !cfg!(miri) {
            assert!(run.fulls > 0, "тест ни разу не упёрся в полное кольцо");
            assert!(run.pads > 0, "тест ни разу не вставил PAD");
            assert!(
                run.wraps >= 10,
                "позиции обернулись всего {} раз",
                run.wraps
            );
        }
    }

    /// Детерминированный интерливинг потерянного сигнала `DATA` (Task 21):
    /// читатель CAS-ом забрал последний кадр и «замер» до уменьшения
    /// `message_count`, его снимок позиций -- «пусто»; писатель кладёт
    /// следующий кадр; читатель досчитывает счётчик. Если читатель уснёт по
    /// своему снимку, разбудить его может только сигнал писателя: запись
    /// обязана вернуть `was_empty`. Оба пути записи: без перезаписи и с ней.
    #[test]
    fn data_signal_survives_reader_draining_concurrently() {
        type Write = fn(&RingBuffer, &[u8]) -> Result<WriteOutcome>;
        let paths: [(&str, Write); 2] = [
            ("try_write_message", RingBuffer::try_write_message),
            ("write_message", RingBuffer::write_message),
        ];
        for (path, write) in paths {
            let (ring, _mem) = make_ring();
            let header = ring.header();
            let mut out = Vec::new();
            write(&ring, &[1u8; 16]).unwrap();

            // Читатель: шаги `read_message` до `decrement_count`.
            let read0 = header.read_pos.load(O::Acquire);
            let write0 = header.write_pos.load(O::Acquire);
            let frame = ring.frame_at(read0, write0).unwrap().unwrap();
            header
                .read_pos
                .compare_exchange(read0, read0 + frame.total, O::AcqRel, O::Acquire)
                .unwrap();
            // Снимок читателя ДО публикации нового кадра: пусто по позициям.
            let snapshot_read = header.read_pos.load(O::Acquire);
            let snapshot_write = header.write_pos.load(O::Acquire);
            assert!(
                ring.frame_at(snapshot_read, snapshot_write)
                    .unwrap()
                    .is_none()
            );

            // Писатель кладёт следующий кадр, пока счётчик ещё 1.
            let outcome = write(&ring, &[2u8; 16]).unwrap();
            // Перепроверка читателя по устаревшему снимку после барьера
            // обязана провалиться: кадр опубликован.
            assert!(
                !ring.confirm_empty(snapshot_write),
                "{path}: перепроверка не увидела новый write_pos"
            );
            // Читатель досчитывает message_count.
            ring.decrement_count();

            assert!(
                outcome.was_empty,
                "{path}: читатель уже выбрал всё до этого кадра -- без сигнала \
                 DATA он уснёт навсегда"
            );
            assert_eq!(outcome.overwritten, 0);
            assert_eq!(ring.read_message(&mut out).unwrap(), 16, "{path}");
            assert_eq!(out, [2u8; 16]);
            assert_eq!(
                ring.read_message(&mut out).err(),
                Some(ShmError::QueueEmpty)
            );
        }
    }

    /// Ветка лимита сообщений `write_message`, пока читатель «замер» между
    /// CAS `read_pos` и `decrement_count`. Task 18 (решение 28): лимит 500
    /// выше `RECOUNT_MAX` -- «полно» только при `count > max_messages`,
    /// отставший на 1 счётчик (`count == MAX_MESSAGES`) запись принимает,
    /// вытеснения нет (до Task 18 здесь вытеснялся кадр 1 -- на кадр
    /// раньше). Кольцо НЕ пусто -- сигнал не нужен (`was_empty == false`), а
    /// устаревший снимок читателя перепроверкой не подтверждается. Затем:
    /// ещё одно сообщение принимается (запас на отставание, `MAX_MESSAGES +
    /// 1`), следующее -- вытеснение ровно одного кадра, тоже без сигнала.
    #[test]
    fn data_signal_on_eviction_by_count_with_reader_mid_step() {
        let (ring, _mem) = make_ring();
        let header = ring.header();
        let mut out = Vec::new();
        for i in 0..MAX_MESSAGES {
            ring.write_message(&i.to_le_bytes()).unwrap();
        }
        // Читатель забрал кадр 0 (CAS), счётчик ещё MAX_MESSAGES.
        let read0 = header.read_pos.load(O::Acquire);
        let write0 = header.write_pos.load(O::Acquire);
        let frame = ring.frame_at(read0, write0).unwrap().unwrap();
        header
            .read_pos
            .compare_exchange(read0, read0 + frame.total, O::AcqRel, O::Acquire)
            .unwrap();

        let outcome = ring.write_message(&u32::MAX.to_le_bytes()).unwrap();
        assert_eq!(outcome.overwritten, 0, "счётчик отстал -- места хватает");
        assert!(
            !outcome.was_empty,
            "читатель не выбрал всё до кадра -- он не уснёт, сигнал не нужен"
        );
        assert!(!ring.confirm_empty(write0), "снимок устарел -- кадр виден");
        ring.decrement_count();
        assert_eq!(header.message_count.load(O::Acquire), MAX_MESSAGES);

        // Запас на законное отставание: `count == MAX_MESSAGES` -- ещё
        // принимается, в кольце становится `MAX_MESSAGES + 1`.
        let extra = u32::MAX - 2;
        assert_eq!(
            ring.write_message(&extra.to_le_bytes())
                .unwrap()
                .overwritten,
            0
        );

        // `count > MAX_MESSAGES`: вытесняется кадр 1, сигнал не нужен.
        let write1 = header.write_pos.load(O::Acquire);
        let outcome = ring.write_message(&(u32::MAX - 1).to_le_bytes()).unwrap();
        assert_eq!(outcome.overwritten, 1, "лимит сообщений -- вытеснение");
        assert!(!outcome.was_empty);
        assert!(!ring.confirm_empty(write1));

        // Читатель получает 2..MAX_MESSAGES и оба новых кадра.
        for i in 2..MAX_MESSAGES {
            assert_eq!(ring.read_message(&mut out).unwrap(), 4);
            assert_eq!(out, i.to_le_bytes());
        }
        for last in [u32::MAX, extra, u32::MAX - 1] {
            assert_eq!(ring.read_message(&mut out).unwrap(), 4);
            assert_eq!(out, last.to_le_bytes());
        }
        assert_eq!(
            ring.read_message(&mut out).err(),
            Some(ShmError::QueueEmpty)
        );
    }

    /// Ветка вытеснения по байтам, опустошающая кольцо: писатель сам выбрал
    /// всё до своего кадра (`read_pos ==` прежнему `write_pos`) -- решение
    /// «сигналить» верно (читатель с прежним снимком «пусто» иначе уснул
    /// бы), снимок до записи перепроверкой не подтверждается.
    ///
    /// 0.10 (PAD): кадр больше половины кольца недопустим, поэтому «не
    /// входит» здесь из-за PAD -- пустое кольцо стоит на смещении 2048, кадр
    /// 4096 байт ложится в [2048, 6144), второй такой же до конца кольца не
    /// помещается (хвост 2048), нужно PAD 2048 + кадр = 6144 при свободных
    /// 4096 -- первый кадр вытесняется, кольцо пустеет, пишутся PAD + кадр.
    #[test]
    fn data_signal_on_eviction_by_bytes_that_empties_ring() {
        let config = RingConfig::new(8192);
        let (ring, _mem) = make_ring_with(&config);
        let header = ring.header();
        let mut out = Vec::new();
        const LEN: usize = 4088;
        assert_eq!(config.max_message_size, LEN);
        header.write_pos.store(2048, O::Relaxed);
        header.read_pos.store(2048, O::Relaxed);
        ring.write_message(&[1u8; LEN]).unwrap();
        let write0 = header.write_pos.load(O::Acquire);
        assert_eq!(write0, 6144);

        let outcome = ring.write_message(&[2u8; LEN]).unwrap();
        assert_eq!(
            outcome.overwritten, 1,
            "PAD + второй кадр в остаток кольца не входят"
        );
        assert_eq!(header.write_pos.load(O::Acquire), write0 + 2048 + 4096);
        assert_eq!(drop_count(&ring), 1, "PAD не считается вытесненным");
        assert!(
            outcome.was_empty,
            "вытеснение выбрало всё до кадра -- read_pos == прежний write_pos"
        );
        assert_eq!(header.read_pos.load(O::Acquire), write0);
        assert!(!ring.confirm_empty(write0), "снимок устарел -- кадр виден");
        assert_eq!(ring.read_message(&mut out).unwrap(), LEN);
        assert_eq!(out, [2u8; LEN]);
        assert_eq!(
            ring.read_message(&mut out).err(),
            Some(ShmError::QueueEmpty)
        );
    }

    /// Счётчик отстаёт от `read_pos` (`max_messages = 1`): читатель CAS-ом
    /// забрал единственный кадр и «замер» до `decrement_count` -- кольцо
    /// пусто по позициям, а `message_count == max_messages`. Это законная
    /// гонка, а не порча: оба пути записи обязаны принять кадр (пустое
    /// кольцо принимает любой кадр, помещающийся по байтам), без вытеснения
    /// и без `QueueFull`; счётчик потом сходится с насыщением.
    #[test]
    fn lagging_count_does_not_reject_write_into_empty_ring() {
        type Write = fn(&RingBuffer, &[u8]) -> Result<WriteOutcome>;
        let paths: [(&str, Write); 2] = [
            ("write_message", RingBuffer::write_message),
            ("try_write_message", RingBuffer::try_write_message),
        ];
        for (path, write) in paths {
            let config = RingConfig::new(4096).max_messages(1);
            let (ring, _mem) = make_ring_with(&config);
            let header = ring.header();
            let mut out = Vec::new();
            write(&ring, &[1u8; 16]).unwrap();

            // Читатель: CAS read_pos за кадр, decrement_count ещё не сделан.
            let read0 = header.read_pos.load(O::Acquire);
            let write0 = header.write_pos.load(O::Acquire);
            let frame = ring.frame_at(read0, write0).unwrap().unwrap();
            header
                .read_pos
                .compare_exchange(read0, read0 + frame.total, O::AcqRel, O::Acquire)
                .unwrap();
            assert_eq!(header.message_count.load(O::Acquire), 1);

            let outcome = write(&ring, &[2u8; 16])
                .unwrap_or_else(|e| panic!("{path}: пустое кольцо отвергло кадр: {e:?}"));
            assert_eq!(outcome.overwritten, 0, "{path}: вытеснять нечего");
            assert!(outcome.was_empty, "{path}: читатель выбрал всё до кадра");

            // Читатель досчитывает; счётчик -- 1 (новый кадр), не ниже нуля.
            ring.decrement_count();
            assert_eq!(header.message_count.load(O::Acquire), 1, "{path}");
            assert_eq!(ring.read_message(&mut out).unwrap(), 16, "{path}");
            assert_eq!(out, [2u8; 16]);
            assert_eq!(header.message_count.load(O::Acquire), 0, "{path}");
            assert_eq!(
                ring.read_message(&mut out).err(),
                Some(ShmError::QueueEmpty)
            );
            assert_eq!(header.message_count.load(O::Acquire), 0, "{path}");
            assert_eq!(drop_count(&ring), 0, "{path}");
        }
    }

    /// Вторая половина протокола: снимок читателя «пусто» снят ДО
    /// публикации, перепроверка после барьера -- ПОСЛЕ. Читатель обязан
    /// увидеть кадр (не уснуть), а писатель в этом порядке тоже сигналит:
    /// хотя бы одна сторона видит запись другой, здесь -- обе.
    #[test]
    fn reader_recheck_sees_frame_published_after_empty_snapshot() {
        let (ring, _mem) = make_ring();
        let header = ring.header();
        let mut out = Vec::new();
        let snapshot_write = header.write_pos.load(O::Acquire);
        assert_eq!(header.read_pos.load(O::Acquire), snapshot_write);
        assert!(ring.confirm_empty(snapshot_write), "пустое кольцо -- пусто");

        let outcome = ring.try_write_message(&[3u8; 16]).unwrap();
        assert!(outcome.was_empty);
        assert!(
            !ring.confirm_empty(snapshot_write),
            "перепроверка после барьера обязана увидеть новый write_pos"
        );
        assert!(!ring.is_empty());
        assert_eq!(ring.read_message(&mut out).unwrap(), 16);
        assert!(ring.is_empty());

        // Читатель отстаёт (в кольце два кадра): сигнал не нужен -- он не
        // уснёт, не дочитав до нового кадра.
        ring.try_write_message(&[4u8; 16]).unwrap();
        let outcome = ring.try_write_message(&[5u8; 16]).unwrap();
        assert!(!outcome.was_empty);
    }

    /// Заявка писателя: читатель снимает её и «будит» только когда места
    /// реально хватает под заявленный кадр.
    #[test]
    fn space_waiter_is_taken_only_when_enough_space() {
        let (ring, _mem) = make_ring();
        const LEN: usize = 60_000;
        while ring.try_write_message(&[0u8; LEN]).is_ok() {}
        let mut out = Vec::new();

        // Никто не ждёт -- сигналить нечего.
        ring.read_message(&mut out).unwrap();
        assert!(!ring.take_space_waiter());
        while ring.try_write_message(&[0u8; LEN]).is_ok() {}

        // Ждём место под 2 кадра: после первого чтения его ещё мало.
        let need = 2 * frame_size(LEN) as u32;
        ring.arm_space_waiter(need);
        ring.read_message(&mut out).unwrap();
        assert!(!ring.take_space_waiter(), "места под заявку ещё нет");
        ring.read_message(&mut out).unwrap();
        assert!(
            ring.take_space_waiter(),
            "места хватает -- заявку надо снять"
        );
        assert!(!ring.take_space_waiter(), "заявка снимается ровно один раз");
        assert_eq!(ring.header().space_waiter.load(O::Acquire), 0);

        // Снятая самим писателем заявка не будит.
        ring.arm_space_waiter(frame_size(LEN) as u32);
        ring.disarm_space_waiter();
        assert!(!ring.take_space_waiter());
    }

    /// `wait_for_space` без события (anonymous-путь или читатель старой
    /// версии, не знающий про заявку): заявка + опрос.
    #[test]
    fn wait_for_space_polls_without_event() {
        let (ring, _mem) = make_ring();
        let ring = Arc::new(ring);
        const LEN: usize = 60_000;
        while ring.try_write_message(&[0u8; LEN]).is_ok() {}
        assert_eq!(
            ring.wait_for_space(
                LEN,
                None,
                None,
                None,
                Some(Duration::from_millis(20)),
                SpinPolicy::NONE
            ),
            Ok(false)
        );
        assert_eq!(
            ring.header().space_waiter.load(O::Acquire),
            0,
            "таймаут снимает заявку"
        );

        let reader = {
            let ring = ring.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(30));
                let mut out = Vec::new();
                ring.read_message(&mut out).unwrap();
                // «Старый» читатель: заявку не смотрит.
            })
        };
        assert_eq!(
            ring.wait_for_space(
                LEN,
                None,
                None,
                None,
                Some(Duration::from_secs(5)),
                SpinPolicy::NONE
            ),
            Ok(true)
        );
        reader.join().unwrap();
        ring.try_write_message(&[1u8; LEN]).unwrap();
        assert_eq!(
            ring.wait_for_space(0, None, None, None, None, SpinPolicy::NONE),
            Err(ShmError::MessageTooSmall),
            "длина проверяется до ожидания"
        );
    }

    #[test]
    fn huge_space_timeout_does_not_overflow_instant() {
        // `Duration::MAX` непредставим как `Instant`: ожидание без дедлайна,
        // а не паника на переполнении (место есть -- возврат сразу).
        let (ring, _mem) = make_ring();
        assert_eq!(
            ring.wait_for_space(
                16,
                None,
                None,
                None,
                Some(Duration::MAX),
                SpinPolicy::new(64)
            ),
            Ok(true)
        );
    }

    fn kernel_waits() -> u32 {
        SPACE_KERNEL_WAITS.with(std::cell::Cell::get)
    }

    /// Заполнить кольцо кадрами по `len` байт.
    fn fill(ring: &RingBuffer, len: usize) {
        while ring.try_write_message(&vec![0u8; len]).is_ok() {}
    }

    /// 0.9: пробуждение по месту -- ОДНО ожидание в ядре на всё время
    /// ожидания (раньше -- срезы по 50 мс: за 300 мс было бы ~6 пробуждений).
    #[test]
    fn wait_for_space_wakes_on_space_without_slicing() {
        let (ring, _mem) = make_ring();
        let ring = Arc::new(ring);
        const LEN: usize = 60_000;
        fill(&ring, LEN);
        let space = Arc::new(EventHandle::create_unnamed(false).unwrap());
        let reader = {
            let (ring, space) = (ring.clone(), space.clone());
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(300));
                let mut out = Vec::new();
                ring.read_message(&mut out).unwrap();
                // Читатель 0.8+: снимает заявку и сигналит SPACE.
                assert!(
                    ring.take_space_waiter(),
                    "заявка писателя должна быть видна"
                );
                space.set().unwrap();
            })
        };
        let before = kernel_waits();
        let t0 = Instant::now();
        assert_eq!(
            ring.wait_for_space(LEN, Some(&space), None, None, None, SpinPolicy::NONE),
            Ok(true)
        );
        let waited = t0.elapsed();
        reader.join().unwrap();
        assert_eq!(kernel_waits() - before, 1, "ожидание нарезано на срезы");
        assert!(
            waited >= Duration::from_millis(250),
            "проснулись раньше места"
        );
    }

    /// 0.9: смерть читателя будит писателя сразу и без срезов, даже при
    /// `timeout = None` (единственное ожидание -- до сигнала процесса).
    #[test]
    fn wait_for_space_wakes_on_peer_death_without_slicing() {
        let (ring, _mem) = make_ring();
        const LEN: usize = 60_000;
        fill(&ring, LEN);
        let space = EventHandle::create_unnamed(false).unwrap();
        // «Читатель» -- процесс, живущий ~1 с.
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "ping -n 2 127.0.0.1 >NUL"])
            .spawn()
            .expect("spawn child");
        let watch = ProcessWatch::open(child.id()).expect("watch child");
        let before = kernel_waits();
        let t0 = Instant::now();
        assert_eq!(
            ring.wait_for_space(
                LEN,
                Some(&space),
                Some(&watch),
                None,
                None,
                SpinPolicy::NONE
            ),
            Err(ShmError::PeerDied)
        );
        let waited = t0.elapsed();
        child.wait().unwrap();
        assert_eq!(kernel_waits() - before, 1, "ожидание нарезано на срезы");
        assert!(
            waited >= Duration::from_millis(300),
            "PeerDied до смерти: {waited:?}"
        );
        assert_eq!(
            ring.header().space_waiter.load(O::Acquire),
            0,
            "заявка снята"
        );
    }

    /// 0.9: штатное отключение читателя (`DISCONNECT` + состояние handshake)
    /// будит писателя (`NotConnected`) и возвращает событие взведённым для
    /// других ждущих; устаревший сигнал (пир на месте) поглощается.
    #[test]
    fn wait_for_space_wakes_on_disconnect_and_ignores_stale_signal() {
        let (ring, _mem) = make_ring();
        let ring = Arc::new(ring);
        const LEN: usize = 60_000;
        fill(&ring, LEN);
        let space = Arc::new(EventHandle::create_unnamed(false).unwrap());
        let disconnect = Arc::new(EventHandle::create_unnamed(false).unwrap());
        let left = Arc::new(AtomicBool::new(false));
        let peer_left = {
            let left = left.clone();
            move || left.load(O::Acquire)
        };
        let watch = DisconnectWatch {
            event: &disconnect,
            peer_left: &peer_left,
        };

        // Устаревший DISCONNECT (пир на месте), затем настоящее место.
        disconnect.set().unwrap();
        let reader = {
            let (ring, space) = (ring.clone(), space.clone());
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(150));
                let mut out = Vec::new();
                ring.read_message(&mut out).unwrap();
                assert!(ring.take_space_waiter());
                space.set().unwrap();
            })
        };
        let before = kernel_waits();
        assert_eq!(
            ring.wait_for_space(
                LEN,
                Some(&space),
                None,
                Some(&watch),
                None,
                SpinPolicy::NONE
            ),
            Ok(true)
        );
        reader.join().unwrap();
        assert_eq!(kernel_waits() - before, 2, "устаревший сигнал + место");
        assert!(
            !disconnect.wait(Some(Duration::ZERO)).unwrap(),
            "устаревший сигнал поглощён"
        );

        // Настоящее отключение.
        fill(&ring, LEN);
        let leaver = {
            let disconnect = disconnect.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(150));
                left.store(true, O::Release);
                disconnect.set().unwrap();
            })
        };
        let before = kernel_waits();
        assert_eq!(
            ring.wait_for_space(
                LEN,
                Some(&space),
                None,
                Some(&watch),
                None,
                SpinPolicy::NONE
            ),
            Err(ShmError::NotConnected)
        );
        leaver.join().unwrap();
        assert_eq!(kernel_waits() - before, 1);
        assert!(
            disconnect.wait(Some(Duration::ZERO)).unwrap(),
            "сигнал отключения возвращён остальным ждущим"
        );
    }

    /// Читатель (`poll_*`): `DISCONNECT` в наборе ожидания данных с тем же
    /// правилом, что у `wait_for_space` -- устаревший сигнал поглощается и
    /// ожидание продолжается остатком таймаута; настоящий уход при пустом
    /// кольце -- `NotConnected` (событие взведено для остальных), при
    /// непустом -- сначала дочитать (`Ok(true)` без ожидания в ядре).
    #[test]
    fn wait_for_data_wakes_on_disconnect_and_ignores_stale_signal() {
        let data_waits = || DATA_KERNEL_WAITS.with(std::cell::Cell::get);
        let (ring, _mem) = make_ring();
        let data = EventHandle::create_unnamed(false).unwrap();
        let disconnect = EventHandle::create_unnamed(false).unwrap();
        let left = AtomicBool::new(false);
        let peer_left = || left.load(O::Acquire);
        let watch = DisconnectWatch {
            event: &disconnect,
            peer_left: &peer_left,
        };
        let timeout = Duration::from_millis(150);

        // (а) Устаревший сигнал: поглощён, ожидание -- остаток таймаута.
        disconnect.set().unwrap();
        let before = data_waits();
        let started = Instant::now();
        assert_eq!(
            ring.wait_for_data(&data, None, Some(&watch), Some(timeout), SpinPolicy::NONE),
            Ok(false)
        );
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(140), "{elapsed:?}");
        assert!(elapsed < Duration::from_millis(1000), "{elapsed:?}");
        assert_eq!(data_waits() - before, 2, "устаревший сигнал + таймаут");
        assert!(
            !disconnect.wait(Some(Duration::ZERO)).unwrap(),
            "устаревший сигнал поглощён"
        );

        // (б) Настоящий уход, кольцо пусто.
        left.store(true, O::Release);
        disconnect.set().unwrap();
        let started = Instant::now();
        assert_eq!(
            ring.wait_for_data(&data, None, Some(&watch), Some(timeout), SpinPolicy::NONE),
            Err(ShmError::NotConnected)
        );
        assert!(started.elapsed() < Duration::from_millis(50));
        assert!(
            disconnect.wait(Some(Duration::ZERO)).unwrap(),
            "сигнал отключения возвращён остальным ждущим"
        );

        // (в) Настоящий уход, в кольце кадр: сначала дочитать.
        disconnect.set().unwrap();
        ring.try_write_message(&[5u8; 16]).unwrap();
        let before = data_waits();
        assert_eq!(
            ring.wait_for_data(&data, None, Some(&watch), Some(timeout), SpinPolicy::NONE),
            Ok(true)
        );
        assert_eq!(data_waits() - before, 0);
    }

    /// Главный путь: читатель УЖЕ спит в ядре (без таймаута-страховки),
    /// когда пир уходит (R14: состояние, потом `DISCONNECT`). Одно ожидание
    /// в ядре, `NotConnected` сразу после сигнала, событие возвращено
    /// взведённым для остальных ждущих.
    #[test]
    fn wait_for_data_blocked_reader_wakes_on_leave() {
        let data_waits = || DATA_KERNEL_WAITS.with(std::cell::Cell::get);
        let (ring, _mem) = make_ring();
        let data = EventHandle::create_unnamed(false).unwrap();
        let disconnect = EventHandle::create_unnamed(false).unwrap();
        let left = AtomicBool::new(false);
        let peer_left = || left.load(O::Acquire);
        let watch = DisconnectWatch {
            event: &disconnect,
            peer_left: &peer_left,
        };
        let delay = Duration::from_millis(100);
        thread::scope(|scope| {
            let leaver = scope.spawn(|| {
                thread::sleep(delay);
                left.store(true, O::Release);
                disconnect.set().unwrap();
            });
            let before = data_waits();
            let started = Instant::now();
            assert_eq!(
                ring.wait_for_data(
                    &data,
                    None,
                    Some(&watch),
                    Some(Duration::from_secs(10)),
                    SpinPolicy::NONE
                ),
                Err(ShmError::NotConnected)
            );
            let elapsed = started.elapsed();
            assert!(elapsed >= delay - Duration::from_millis(10), "{elapsed:?}");
            assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
            assert_eq!(data_waits() - before, 1, "одно ожидание в ядре");
            leaver.join().unwrap();
        });
        assert!(
            disconnect.wait(Some(Duration::ZERO)).unwrap(),
            "сигнал отключения возвращён остальным ждущим"
        );
    }

    /// Писатель быстрее читателя: try_write упирается в QueueFull, но ни одно
    /// сообщение не теряется, не переставляется и не рвётся; после того как
    /// читатель разгребает кольцо, запись продолжается.
    #[test]
    fn lossless_producer_faster_than_consumer() {
        let (ring, _mem) = make_ring();
        let ring = Arc::new(ring);
        let total: u32 = if cfg!(miri) { 60 } else { 20_000 };
        let fulls = Arc::new(AtomicU64::new(0));
        let done = Arc::new(AtomicBool::new(false));

        let producer = {
            let ring = ring.clone();
            let fulls = fulls.clone();
            thread::spawn(move || {
                let mut rng = Rng(42);
                for seq in 0..total {
                    let len = if cfg!(miri) {
                        16 + rng.below(64) as usize
                    } else {
                        12 + rng.below(40_000) as usize
                    };
                    let payload = make_payload(seq, len);
                    loop {
                        match ring.try_write_message(&payload) {
                            Ok(o) => {
                                assert_eq!(o.overwritten, 0);
                                break;
                            }
                            Err(ShmError::QueueFull) => {
                                fulls.fetch_add(1, O::Relaxed);
                                thread::yield_now();
                            }
                            Err(err) => panic!("unexpected {err:?}"),
                        }
                    }
                }
            })
        };

        let consumer = {
            let ring = ring.clone();
            let done = done.clone();
            thread::spawn(move || {
                let mut out = Vec::new();
                let mut expected = 0u32;
                while expected < total {
                    match ring.read_message(&mut out) {
                        Ok(len) => {
                            let seq = u32::from_le_bytes(out[..4].try_into().unwrap());
                            assert_eq!(seq, expected, "потеря или перестановка");
                            assert_eq!(out, make_payload(seq, len), "порванное сообщение");
                            expected += 1;
                            // Читатель заметно медленнее писателя.
                            if !cfg!(miri) && expected.is_multiple_of(8) {
                                thread::sleep(Duration::from_micros(200));
                            }
                        }
                        Err(ShmError::QueueEmpty) => thread::yield_now(),
                        Err(err) => panic!("unexpected {err:?}"),
                    }
                }
                done.store(true, O::Release);
            })
        };

        producer.join().unwrap();
        consumer.join().unwrap();
        assert!(done.load(O::Acquire));
        assert_eq!(drop_count(&ring), 0);
        assert!(ring.is_empty());
        if !cfg!(miri) {
            assert!(
                fulls.load(O::Relaxed) > 0,
                "писатель ни разу не упёрся в полное кольцо"
            );
        }
    }

    /// Микробенчмарк: `cargo test --release --lib -- --ignored --nocapture bench_`
    #[test]
    #[ignore = "бенчмарк, запускать вручную в --release"]
    fn bench_try_write_vs_write() {
        use std::hint::black_box;
        let (ring, _mem) = make_ring();
        let mut out = Vec::with_capacity(MAX_MESSAGE_SIZE);
        const ITERS: u32 = 2_000_000;
        for len in [16usize, 256, 4096] {
            let payload = vec![0xA5u8; len];
            // Прогрев.
            for _ in 0..10_000 {
                ring.write_message(&payload).unwrap();
                ring.read_message(&mut out).unwrap();
            }
            let t = Instant::now();
            for _ in 0..ITERS {
                black_box(ring.write_message(black_box(&payload)).unwrap());
                ring.read_message(&mut out).unwrap();
            }
            let write_ns = t.elapsed().as_nanos() as f64 / f64::from(ITERS);
            let t = Instant::now();
            for _ in 0..ITERS {
                black_box(ring.try_write_message(black_box(&payload)).unwrap());
                ring.read_message(&mut out).unwrap();
            }
            let try_ns = t.elapsed().as_nanos() as f64 / f64::from(ITERS);
            let t = Instant::now();
            for _ in 0..ITERS {
                black_box(ring.try_write_message(black_box(&payload)).unwrap());
                ring.read_message(&mut out).unwrap();
                black_box(ring.take_space_waiter());
            }
            let try_waiter_ns = t.elapsed().as_nanos() as f64 / f64::from(ITERS);
            let t = Instant::now();
            for _ in 0..ITERS {
                black_box(ring.free_space());
            }
            let free_ns = t.elapsed().as_nanos() as f64 / f64::from(ITERS);
            println!(
                "len={len:5}: write+read {write_ns:6.1} ns | try_write+read {try_ns:6.1} ns | \
                 try_write+read+take_space_waiter {try_waiter_ns:6.1} ns | free_space {free_ns:5.1} ns"
            );
        }
        // Отказ на полном кольце -- стоимость «холостой» попытки.
        let big = vec![0u8; 60_000];
        while ring.try_write_message(&big).is_ok() {}
        let t = Instant::now();
        for _ in 0..ITERS {
            black_box(ring.try_write_message(black_box(&big)).is_err());
        }
        let full_ns = t.elapsed().as_nanos() as f64 / f64::from(ITERS);
        println!("try_write on full ring (QueueFull): {full_ns:.1} ns");
    }
}

#[cfg(test)]
mod tests {
    //! Спин перед ожиданием в ядре (0.10, `SpinPolicy`); непрерывные кадры,
    //! PAD и zero-copy (0.10, `read_with`/`write_with`).
    use super::overflow_race_tests::{make_ring, make_ring_with};
    use super::try_write_tests::{drop_count, run_framing_model};
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::Ordering as O;
    use std::thread;

    /// Бюджет, заведомо перекрывающий задержку писателя ниже (десятки мс)
    /// на любом x86: даже при ~3 нс на итерацию -- больше 0,5 с.
    const LONG_SPIN: SpinPolicy = SpinPolicy::new(200_000_000);
    /// Задержка писателя/читателя: спин уже идёт, когда данные (место)
    /// появляются.
    const PEER_DELAY: Duration = Duration::from_millis(20);
    const LEN: usize = 60_000;

    fn data_waits() -> u32 {
        DATA_KERNEL_WAITS.with(std::cell::Cell::get)
    }

    fn space_waits() -> u32 {
        SPACE_KERNEL_WAITS.with(std::cell::Cell::get)
    }

    /// Писатель в другом потоке: через `PEER_DELAY` кладёт кадр и, как
    /// требует протокол, сигналит `DATA` при `was_empty`.
    fn spawn_writer(ring: &Arc<RingBuffer>, data: &Arc<EventHandle>) -> thread::JoinHandle<()> {
        let (ring, data) = (Arc::clone(ring), Arc::clone(data));
        thread::spawn(move || {
            thread::sleep(PEER_DELAY);
            if ring.try_write_message(&[7u8; 16]).unwrap().was_empty {
                data.set().unwrap();
            }
        })
    }

    #[test]
    fn spin_policy_defaults_to_none() {
        assert_eq!(SpinPolicy::default(), SpinPolicy::NONE);
        assert!(!SpinPolicy::NONE.is_enabled());
        assert!(SpinPolicy::new(1).is_enabled());
        assert!(
            !spin_until(SpinPolicy::NONE, None, || true),
            "без бюджета -- ни одной проверки"
        );
        let mut calls = 0;
        assert!(!spin_until(SpinPolicy::new(5), None, || {
            calls += 1;
            false
        }));
        assert_eq!(calls, 5, "бюджет -- ровно `iterations` проверок");
    }

    /// Данные, появившиеся во время спина, читаются без ожидания в ядре; без
    /// спина тот же сценарий -- ровно одно ожидание (счётчик честный).
    #[test]
    fn spin_sees_data_without_kernel_wait() {
        let (ring, _mem) = make_ring();
        let ring = Arc::new(ring);
        let data = Arc::new(EventHandle::create_unnamed(false).unwrap());
        let mut out = Vec::new();

        let writer = spawn_writer(&ring, &data);
        let before = data_waits();
        assert_eq!(
            ring.wait_for_data(&data, None, None, Some(Duration::from_secs(5)), LONG_SPIN),
            Ok(true)
        );
        writer.join().unwrap();
        assert_eq!(data_waits() - before, 0, "спин ушёл в ядро");
        assert_eq!(ring.read_message(&mut out), Ok(16));
        // Сигнал писателя спин не трогает: остался взведённым (одно лишнее
        // пробуждение потом), а не сброшен -- сброс мог бы потерять сигнал.
        assert!(data.wait(Some(Duration::ZERO)).unwrap());

        // Контроль: без спина -- одно ожидание в ядре до сигнала писателя.
        let writer = spawn_writer(&ring, &data);
        let before = data_waits();
        assert_eq!(
            ring.wait_for_data(
                &data,
                None,
                None,
                Some(Duration::from_secs(5)),
                SpinPolicy::NONE
            ),
            Ok(true)
        );
        writer.join().unwrap();
        assert_eq!(data_waits() - before, 1);
        assert_eq!(ring.read_message(&mut out), Ok(16));
    }

    /// Пир уже ушёл: ответ из памяти до спина -- длинный бюджет не
    /// тратится, ожидания в ядре нет, событие не тронуто.
    #[test]
    fn departed_peer_skips_spin() {
        let (ring, _mem) = make_ring();
        let data = EventHandle::create_unnamed(false).unwrap();
        let disconnect = EventHandle::create_unnamed(false).unwrap();
        let peer_left = || true;
        let watch = DisconnectWatch {
            event: &disconnect,
            peer_left: &peer_left,
        };
        let before = data_waits();
        let started = Instant::now();
        assert_eq!(
            ring.wait_for_data(
                &data,
                None,
                Some(&watch),
                Some(Duration::from_secs(5)),
                LONG_SPIN
            ),
            Err(ShmError::NotConnected)
        );
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_millis(50), "{elapsed:?}");
        assert_eq!(data_waits() - before, 0);
        assert!(!disconnect.wait(Some(Duration::ZERO)).unwrap());
    }

    /// Бюджет исчерпан без данных -- одно обычное ожидание в ядре (не
    /// повторный спин); нулевой таймаут не спинит вовсе.
    #[test]
    fn exhausted_spin_falls_back_to_single_kernel_wait() {
        let (ring, _mem) = make_ring();
        let data = EventHandle::create_unnamed(false).unwrap();
        let before = data_waits();
        assert_eq!(
            ring.wait_for_data(
                &data,
                None,
                None,
                Some(Duration::from_millis(30)),
                SpinPolicy::new(1_000)
            ),
            Ok(false)
        );
        assert_eq!(data_waits() - before, 1);

        let t0 = Instant::now();
        assert_eq!(
            ring.wait_for_data(&data, None, None, Some(Duration::ZERO), LONG_SPIN),
            Ok(false)
        );
        assert!(
            t0.elapsed() < Duration::from_millis(200),
            "нулевой таймаут спинил: {:?}",
            t0.elapsed()
        );
    }

    /// Спин уважает дедлайн вызывающего: бюджет `u32::MAX` (минуты) не
    /// растягивает короткий таймаут ни ожидания данных, ни ожидания места.
    #[test]
    fn spin_respects_caller_deadline() {
        let unbounded = SpinPolicy::new(u32::MAX);
        let (ring, _mem) = make_ring();
        let data = EventHandle::create_unnamed(false).unwrap();
        let t0 = Instant::now();
        assert_eq!(
            ring.wait_for_data(
                &data,
                None,
                None,
                Some(Duration::from_millis(30)),
                unbounded
            ),
            Ok(false)
        );
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "спин данных перебрал таймаут: {:?}",
            t0.elapsed()
        );

        while ring.try_write_message(&[0u8; LEN]).is_ok() {}
        let space = EventHandle::create_unnamed(false).unwrap();
        let t0 = Instant::now();
        assert_eq!(
            ring.wait_for_space(
                LEN,
                Some(&space),
                None,
                None,
                Some(Duration::from_millis(30)),
                unbounded
            ),
            Ok(false)
        );
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "спин места перебрал таймаут: {:?}",
            t0.elapsed()
        );

        // Дедлайн проверяется раз в `SPIN_DEADLINE_STRIDE` итераций, а не
        // на каждой: уже наступивший дедлайн обрывает спин ровно на первой
        // проверке, после `SPIN_DEADLINE_STRIDE` вызовов `ready`.
        let mut calls = 0;
        assert!(!spin_until(
            SpinPolicy::new(SPIN_DEADLINE_STRIDE * 3),
            Some(Instant::now()),
            || {
                calls += 1;
                false
            }
        ));
        assert_eq!(calls, SPIN_DEADLINE_STRIDE, "дедлайн сверяется на шаге");
    }

    /// Место, освобождённое читателем во время спина (без заявки и без
    /// `SPACE`), видно без ожидания в ядре; заявка на время спина не
    /// ставится.
    #[test]
    fn spin_sees_space_without_kernel_wait() {
        let (ring, _mem) = make_ring();
        let ring = Arc::new(ring);
        while ring.try_write_message(&[0u8; LEN]).is_ok() {}
        let space = EventHandle::create_unnamed(false).unwrap();
        let reader = {
            let ring = Arc::clone(&ring);
            thread::spawn(move || {
                thread::sleep(PEER_DELAY);
                let mut out = Vec::new();
                ring.read_message(&mut out).unwrap();
                // Заявки нет -- будить некого.
                assert!(!ring.take_space_waiter());
            })
        };
        let before = space_waits();
        assert_eq!(
            ring.wait_for_space(
                LEN,
                Some(&space),
                None,
                None,
                Some(Duration::from_secs(5)),
                LONG_SPIN
            ),
            Ok(true)
        );
        reader.join().unwrap();
        assert_eq!(space_waits() - before, 0, "спин ушёл в ядро");
        assert_eq!(ring.header().space_waiter.load(O::Acquire), 0);
        ring.try_write_message(&[1u8; LEN]).unwrap();
    }

    /// Слово заголовка кадра в позиции `pos`.
    fn word(ring: &RingBuffer, pos: u32) -> u32 {
        ring.header_word(ring.mask_index(pos)).load(O::Relaxed)
    }

    fn set_word(ring: &RingBuffer, pos: u32, value: u32) {
        ring.header_word(ring.mask_index(pos))
            .store(value, O::Relaxed);
    }

    /// Чтение через `read_with`: срез обязан лежать внутри данных кольца
    /// одним куском (payload не разрывается границей).
    fn read_contiguous(ring: &RingBuffer) -> Result<Vec<u8>> {
        let base = ring.data_ptr().addr();
        let end = base + ring.capacity as usize;
        // SAFETY: однопоточный тест -- пока выполняется `f`, кадр никто не
        // меняет (писатель -- этот же поток, он стоит).
        unsafe {
            ring.read_with(|data| {
                let start = data.as_ptr().addr();
                assert!(
                    start >= base && start + data.len() <= end,
                    "срез payload вне кольца"
                );
                data.to_vec()
            })
        }
    }

    /// 0.10: каждый `read_with` получает непрерывный срез, содержимое
    /// совпадает с моделью (PAD-кадры вставлены и пропущены) -- на маленьком
    /// кольце без лимита сообщений (частые PAD и обёртки) и на кольце по
    /// умолчанию.
    #[test]
    fn payload_never_wraps() {
        let steps = if cfg!(miri) { 300 } else { 40_000 };
        let run = run_framing_model(
            &RingConfig::new(64 << 10),
            steps,
            0x2545_F491_4F6C_DD1D,
            read_contiguous,
        );
        if !cfg!(miri) {
            assert!(run.pads > 0, "ни одного PAD");
            assert!(run.fulls > 0, "ни разу не упёрлись в полное кольцо");
            assert!(
                run.wraps >= 10,
                "позиции обернулись всего {} раз",
                run.wraps
            );
        }
        let run = run_framing_model(&RingConfig::DEFAULT, steps, 7, read_contiguous);
        if !cfg!(miri) {
            assert!(run.pads > 0, "ни одного PAD на кольце по умолчанию");
        }
    }

    /// PAD-кадр: вставляется, когда кадр не помещается непрерывно до конца
    /// кольца; занимает место (`free_space` учитывает PAD + кадр), но не
    /// считается сообщением и не доставляется.
    #[test]
    fn pad_frame_is_skipped_and_accounted() {
        let (ring, _mem) = make_ring_with(&RingConfig::new(4096));
        let header = ring.header();
        let mut out = Vec::new();
        assert_eq!(ring.max_message_size(), 4096 / 2 - FRAME_HEADER_SIZE);
        // A [0, 2008), B [2008, 4016); A прочитан: read_pos = 2008, хвост до
        // конца кольца -- 80 байт.
        ring.try_write_message(&[1u8; 2000]).unwrap();
        ring.try_write_message(&[2u8; 2000]).unwrap();
        assert_eq!(ring.read_message(&mut out), Ok(2000));
        let free = ring.free_space();
        assert_eq!(free.bytes, 4096 - 2008);
        // Байт хватает на кадр 2048, но непрерывно -- нет: PAD 80 + 2048 > 2088.
        assert!(free.bytes >= frame_size(2040));
        assert!(!free.fits(2040), "свободное место учитывает PAD");
        assert!(!free.fits(2001));
        assert!(free.fits(2000), "PAD 80 + кадр 2008 == 2088");
        assert_eq!(free.max_payload(), 2000);
        assert_eq!(
            ring.try_write_message(&[3u8; 2040]),
            Err(ShmError::QueueFull)
        );
        // SAFETY: однопоточно, единственный писатель; `f` не вызывается.
        let refused = unsafe { ring.write_with(2040, |_| unreachable!("места нет")) };
        assert_eq!(refused, Err(ShmError::QueueFull));
        assert_eq!(
            header.write_pos.load(O::Acquire),
            4016,
            "отказ ничего не пишет"
        );

        let outcome = ring.try_write_message(&[4u8; 1992]).unwrap();
        assert!(!outcome.was_empty, "B ещё не прочитан");
        // PAD + кадр опубликованы одной записью write_pos.
        assert_eq!(header.write_pos.load(O::Acquire), 4016 + 80 + 2000);
        // PAD: len = 80 - 8, flags = FRAME_PAD; кадр -- с начала кольца.
        assert_eq!(word(&ring, 4016), 72);
        assert_eq!(word(&ring, 4020), FRAME_PAD);
        assert_eq!(word(&ring, 0), 1992);
        assert_eq!(word(&ring, 4), 0);
        assert_eq!(
            header.message_count.load(O::Acquire),
            2,
            "PAD -- не сообщение"
        );
        assert_eq!(ring.free_space().bytes, 4096 - (6096 - 2008));

        // B, затем PAD пропускается без доставки -- сразу новое сообщение.
        assert_eq!(ring.read_message(&mut out), Ok(2000));
        assert_eq!(out, [2u8; 2000]);
        assert_eq!(ring.read_message(&mut out), Ok(1992));
        assert_eq!(out, [4u8; 1992]);
        assert_eq!(header.read_pos.load(O::Acquire), 6096);
        assert_eq!(header.message_count.load(O::Acquire), 0);
        assert_eq!(drop_count(&ring), 0);
        assert_eq!(ring.read_message(&mut out), Err(ShmError::QueueEmpty));
        assert_eq!(ring.free_space().bytes, 4096);
    }

    /// Писатель вытеснил кадр, пока `f` работал с ним: CAS `read_pos`
    /// проваливается, результат `f` отбрасывается -- `Overrun`.
    #[test]
    fn read_with_reports_overrun() {
        let (ring, _mem) = make_ring_with(&RingConfig::new(4096).max_messages(1));
        let header = ring.header();
        ring.write_message(b"first").unwrap();
        let mut ran = false;
        // SAFETY: вытеснение во время `f` -- по лимиту сообщений: писатель
        // сдвигает только `read_pos` и пишет новый кадр ПОСЛЕ заимствованного
        // ([16, 32) против [0, 16)) -- байты среза не меняются.
        let result = unsafe {
            ring.read_with(|data| {
                assert_eq!(data, b"first");
                let outcome = ring.write_message(b"second").unwrap();
                assert_eq!(outcome.overwritten, 1);
                ran = true;
                data.len()
            })
        };
        assert!(ran);
        assert_eq!(result, Err(ShmError::Overrun), "результат f отброшен");
        assert_eq!(drop_count(&ring), 1);
        assert_eq!(header.message_count.load(O::Acquire), 1);
        // SAFETY: однопоточно -- кадр никто не меняет.
        let second = unsafe { ring.read_with(<[u8]>::to_vec) };
        assert_eq!(second, Ok(b"second".to_vec()));
        // SAFETY: см. выше.
        let empty = unsafe { ring.read_with(|_| ()) };
        assert_eq!(empty, Err(ShmError::QueueEmpty));
    }

    /// Позиции -- `u32` по модулю 2³² (Review Focus 5): PAD у самой границы
    /// обёртки, чтение через неё и вытеснение старейшего PAD-кадра.
    #[test]
    fn pad_at_u32_wrap_boundary() {
        let (ring, _mem) = make_ring_with(&RingConfig::new(4096));
        let header = ring.header();
        let mut out = Vec::new();
        // Пустое кольцо за 24 байта до обёртки u32: индекс 4072, хвост 24.
        let start = 0u32.wrapping_sub(24);
        assert_eq!(ring.mask_index(start), 4072);
        let reset = || {
            header.write_pos.store(start, O::Relaxed);
            header.read_pos.store(start, O::Relaxed);
        };

        // Чтение через границу: PAD [2^32-24, 2^32), кадр с начала кольца,
        // write_pos оборачивается в 112.
        reset();
        let outcome = ring.write_message(&[1u8; 100]).unwrap();
        assert!(outcome.was_empty);
        assert_eq!(header.write_pos.load(O::Acquire), 112);
        assert_eq!(word(&ring, start), 16);
        assert_eq!(word(&ring, start.wrapping_add(4)), FRAME_PAD);
        assert_eq!(ring.free_space().bytes, 4096 - 136);
        assert_eq!(read_contiguous(&ring), Ok(vec![1u8; 100]));
        assert_eq!(header.read_pos.load(O::Acquire), 112);
        assert_eq!(ring.read_message(&mut out), Err(ShmError::QueueEmpty));

        // Вытеснение старейшего PAD у границы: M1 = PAD 24 + 112, M2 [112,
        // 2120); M3 (кадр 2008) до конца не помещается (хвост 1976) -- нужно
        // 3984 при свободных 1952: вытесняются PAD (не считается), M1 и M2.
        reset();
        ring.write_message(&[1u8; 100]).unwrap();
        ring.write_message(&[2u8; 2000]).unwrap();
        assert_eq!(header.write_pos.load(O::Acquire), 2120);
        let outcome = ring.write_message(&[3u8; 2000]).unwrap();
        assert_eq!(outcome.overwritten, 2, "PAD вытеснен, но не посчитан");
        assert!(outcome.was_empty, "вытеснение выбрало всё до кадра");
        assert_eq!(drop_count(&ring), 2);
        assert_eq!(header.read_pos.load(O::Acquire), 2120);
        assert_eq!(header.write_pos.load(O::Acquire), 2120 + 1976 + 2008);
        assert_eq!(header.message_count.load(O::Acquire), 1);
        assert_eq!(ring.read_message(&mut out), Ok(2000));
        assert_eq!(out, [3u8; 2000]);
        assert_eq!(ring.read_message(&mut out), Err(ShmError::QueueEmpty));
        assert_eq!(header.message_count.load(O::Acquire), 0);
    }

    /// Порча PAD-кадров и флагов пиром (Review Focus 1): читатель и
    /// вытесняющий писатель отвечают `Corrupted` -- без доставки мусора,
    /// выхода за кольцо и зацикливания.
    #[test]
    fn hostile_pad_frames_are_corrupted() {
        // max_messages = 1: `write_message` при непустом кольце обязан
        // вытеснять -- проверяем и путь писателя.
        let (ring, _mem) = make_ring_with(&RingConfig::new(4096).max_messages(1));
        let header = ring.header();
        let expect_corrupted = |what: &str| {
            let mut out = Vec::new();
            assert_eq!(
                ring.read_message(&mut out),
                Err(ShmError::Corrupted),
                "{what}"
            );
            // SAFETY: однопоточно; `f` не вызывается вовсе (порча).
            let zero_copy = unsafe {
                ring.read_with(|_| unreachable!("{what}: доставлен мусор"))
            };
            assert_eq!(zero_copy, Err(ShmError::Corrupted), "{what}");
            assert_eq!(ring.write_message(b"x"), Err(ShmError::Corrupted), "{what}");
        };
        let at = |pos: u32| {
            header.write_pos.store(pos, O::Relaxed);
            header.read_pos.store(pos, O::Relaxed);
            header.message_count.store(0, O::Relaxed);
        };

        // PAD с len за пределом кольца.
        at(0);
        ring.try_write_message(b"payload").unwrap();
        set_word(&ring, 0, u32::MAX - 7);
        set_word(&ring, 4, FRAME_PAD);
        expect_corrupted("PAD len за пределом кольца");

        // PAD посреди кольца (не до границы).
        set_word(&ring, 0, 8);
        expect_corrupted("PAD не до границы кольца");

        // Неизвестный бит флагов обычного кадра.
        set_word(&ring, 0, 7);
        set_word(&ring, 4, 2);
        expect_corrupted("неизвестный бит флагов");

        // Настоящий PAD у границы с лишним битом флагов.
        at(4072);
        ring.try_write_message(&[5u8; 100]).unwrap();
        assert_eq!(word(&ring, 4072), 16);
        set_word(&ring, 4076, FRAME_PAD | 2);
        expect_corrupted("PAD с неизвестным битом");

        // PAD за пределом опубликованных данных (write_pos внутри PAD).
        set_word(&ring, 4076, FRAME_PAD);
        header.write_pos.store(4072 + 16, O::Relaxed);
        expect_corrupted("PAD за write_pos");

        // PAD, выходящий за конец кольца на 8 байт.
        header.write_pos.store(4072 + 136, O::Relaxed);
        set_word(&ring, 4072, 24);
        expect_corrupted("PAD за концом кольца");

        // PAD на индексе 0 (на всё кольцо): честный писатель его не пишет,
        // а принятый он позволял бы цепочку PAD подряд.
        at(0);
        ring.try_write_message(&[7u8; 8]).unwrap();
        set_word(&ring, 0, 4096 - 8);
        set_word(&ring, 4, FRAME_PAD);
        header.write_pos.store(4096, O::Relaxed);
        expect_corrupted("PAD на индексе 0");

        // Обычный кадр, payload которого пересекает конец кольца.
        at(4072);
        ring.try_write_message(&[6u8; 8]).unwrap();
        set_word(&ring, 4072, 20);
        header.write_pos.store(4072 + 32, O::Relaxed);
        expect_corrupted("payload через границу");
    }
}

#[cfg(test)]
mod drain_tests {
    //! Дочитывание ушедшего пира (Multi, 0.10): граница снимка, своя
    //! позиция чтения, физический лимит кадров.
    use super::overflow_race_tests::make_ring_with;
    use super::*;

    /// Дописанное после снимка не читается; перемотка `read_pos` пиром --
    /// `Corrupted`, уже доставленный кадр повторно не отдаётся.
    #[test]
    fn drain_stops_at_snapshot_and_rejects_rewind() {
        let (ring, _mem) = make_ring_with(&RingConfig::new(4096).max_messages(u32::MAX));
        let mut out = Vec::new();
        for seq in 0..3u32 {
            ring.write_message(&seq.to_le_bytes()).unwrap();
        }
        let mut bound = ring.drain_snapshot();
        assert_eq!(bound.left, 4096 / 16 + 1, "лимит -- физический");
        ring.write_message(&99u32.to_le_bytes()).unwrap(); // после снимка
        for seq in 0..3u32 {
            assert_eq!(ring.drain_message(&mut out, &mut bound), Ok(Some(4)));
            assert_eq!(out, seq.to_le_bytes());
        }
        assert_eq!(ring.drain_message(&mut out, &mut bound), Ok(None));

        // Перемотка назад во время дочитывания.
        let mut bound = ring.drain_snapshot();
        let start = bound.read;
        assert_eq!(ring.drain_message(&mut out, &mut bound), Ok(Some(4)));
        ring.header().read_pos.store(start, Ordering::Release);
        assert_eq!(
            ring.drain_message(&mut out, &mut bound),
            Err(ShmError::Corrupted)
        );
    }
}

#[cfg(test)]
mod lagging_count_tests {
    //! Task 18 (наблюдение Task 21): законно отстающий `message_count`
    //! (читатель уже сдвинул `read_pos` CAS-ом, но ещё не уменьшил счётчик)
    //! не даёт ни раннего вытеснения, ни ложного `QueueFull`.
    use super::overflow_race_tests::make_ring_with;
    use super::try_write_tests::drop_count;
    use super::*;

    const LIMIT: u32 = 4;
    const LEN: usize = 8;

    /// Кольцо с `LIMIT` сообщениями; читатель «застыл» между CAS `read_pos`
    /// за первое сообщение и декрементом счётчика.
    fn ring_with_reader_in_window() -> (RingBuffer, overflow_race_tests::RingMem) {
        let (ring, mem) = make_ring_with(&RingConfig::new(4096).max_messages(LIMIT));
        for seq in 0..LIMIT {
            ring.try_write_message(&[seq as u8; LEN]).unwrap();
        }
        let header = ring.header();
        let read = header.read_pos.load(Ordering::Acquire);
        header
            .read_pos
            .store(read + frame_size(LEN) as u32, Ordering::Release);
        assert_eq!(header.message_count.load(Ordering::Acquire), LIMIT);
        (ring, mem)
    }

    /// Прочитать всё оставшееся (первые байты payload).
    fn drain(ring: &RingBuffer) -> Vec<u8> {
        let mut out = Vec::new();
        let mut seen = Vec::new();
        while ring.read_message(&mut out).is_ok() {
            seen.push(out[0]);
        }
        seen
    }

    /// Запись с перезаписью в окне отставания: в кольце `LIMIT - 1`
    /// сообщений -- место по лимиту есть, вытеснять нечего.
    #[test]
    fn overwrite_does_not_evict_early() {
        let (ring, _mem) = ring_with_reader_in_window();
        let outcome = ring.write_message(&[9u8; LEN]).unwrap();
        assert_eq!(outcome.overwritten, 0, "вытеснение на кадр раньше");
        assert_eq!(drop_count(&ring), 0);
        // Читатель завершает чтение (декремент) -- счётчик сходится с кадрами.
        ring.decrement_count();
        assert_eq!(ring.header().message_count.load(Ordering::Acquire), LIMIT);
        // Теперь кольцо полно по-настоящему: вытесняется ровно один кадр.
        assert_eq!(
            ring.try_write_message(&[10u8; LEN]),
            Err(ShmError::QueueFull)
        );
        let outcome = ring.write_message(&[10u8; LEN]).unwrap();
        assert_eq!(outcome.overwritten, 1);
        assert_eq!(drop_count(&ring), 1);
        assert_eq!(drain(&ring), [2, 3, 9, 10]);
    }

    /// То же для записи без перезаписи: ложного `QueueFull` нет.
    #[test]
    fn try_write_is_not_refused() {
        let (ring, _mem) = ring_with_reader_in_window();
        ring.try_write_message(&[9u8; LEN]).unwrap();
        ring.decrement_count();
        assert_eq!(drain(&ring), [1, 2, 3, 9]);
    }

    /// Пересчёт не верит и порче: «счётчик на пределе» при `LIMIT - 1`
    /// настоящих сообщениях -- запись без вытеснения; при `LIMIT` настоящих
    /// -- ровно одно вытеснение (лимит по кадрам соблюдён).
    #[test]
    fn recount_bounds_the_limit_by_frames() {
        let (ring, _mem) = make_ring_with(&RingConfig::new(4096).max_messages(LIMIT));
        let header = ring.header();
        for seq in 0..LIMIT - 1 {
            ring.try_write_message(&[seq as u8; LEN]).unwrap();
        }
        header.message_count.store(u32::MAX - 1, Ordering::Release);
        assert_eq!(ring.write_message(&[7u8; LEN]).unwrap().overwritten, 0);
        header.message_count.store(LIMIT, Ordering::Release);
        assert_eq!(ring.write_message(&[8u8; LEN]).unwrap().overwritten, 1);
        assert_eq!(drain(&ring), [1, 2, 7, 8]);
    }

    /// Ревью Task 18, M3: PAD-кадр в окне пересчёта и позиции у 2^32. Кольцо
    /// 4096 стоит на `2^32 - 40` (остаток до границы -- 40 байт): кадры по
    /// 16 байт ложатся на 4056 и 4072, третьему (остаток 8) нужен PAD, сам
    /// он -- с индекса 0 уже за обёрткой `u32`. Читатель в окне отставания:
    /// пересчёт идёт через PAD (не засчитан) и через обёртку -- вытеснения
    /// нет.
    #[test]
    fn recount_skips_pad_across_u32_wrap() {
        let (ring, _mem) = make_ring_with(&RingConfig::new(4096).max_messages(LIMIT));
        let header = ring.header();
        let start = 0u32.wrapping_sub(40);
        header.write_pos.store(start, Ordering::Release);
        header.read_pos.store(start, Ordering::Release);
        for seq in 0..LIMIT {
            ring.try_write_message(&[seq as u8; LEN]).unwrap();
        }
        let write = header.write_pos.load(Ordering::Acquire);
        // 4 кадра по 16 + PAD 8: позиция записи обернулась через 0.
        assert_eq!(write, start.wrapping_add(4 * 16 + 8));
        assert!(write < start, "позиции обернулись");
        // Читатель забрал первое сообщение (CAS), счётчик ещё не уменьшен.
        header.read_pos.store(
            start.wrapping_add(frame_size(LEN) as u32),
            Ordering::Release,
        );
        assert_eq!(
            ring.messages_between(start.wrapping_add(16), write),
            LIMIT - 1
        );
        let outcome = ring.write_message(&[9u8; LEN]).unwrap();
        assert_eq!(outcome.overwritten, 0, "PAD засчитан как сообщение");
        ring.decrement_count();
        assert_eq!(drain(&ring), [1, 2, 3, 9]);
    }

    /// Решение 28: лимит выше `RECOUNT_MAX` -- правило по счётчику с запасом
    /// на отставание. Читатель в окне отставания при `count ==
    /// max_messages` -- запись без вытеснения; кольцо, честно полное до
    /// `max_messages + 1`, -- вытеснение одного кадра; пересчёта нет
    /// (O(1)).
    #[test]
    fn above_recount_max_tolerates_lag_by_counter() {
        const BIG: u32 = RECOUNT_MAX + 1;
        let (ring, _mem) = make_ring_with(&RingConfig::new(8192).max_messages(BIG));
        let header = ring.header();
        for seq in 0..BIG {
            ring.try_write_message(&[seq as u8; LEN]).unwrap();
        }
        // Окно отставания: read_pos сдвинут, счётчик == BIG.
        let read = header.read_pos.load(Ordering::Acquire);
        header
            .read_pos
            .store(read + frame_size(LEN) as u32, Ordering::Release);
        assert_eq!(ring.write_message(&[200u8; LEN]).unwrap().overwritten, 0);
        ring.decrement_count();
        // count == BIG (BIG сообщений): ещё одно -- запас на отставание.
        ring.try_write_message(&[201u8; LEN]).unwrap();
        assert_eq!(
            ring.try_write_message(&[202u8; LEN]),
            Err(ShmError::QueueFull)
        );
        assert_eq!(ring.write_message(&[202u8; LEN]).unwrap().overwritten, 1);
        assert_eq!(drop_count(&ring), 1);
        let seen = drain(&ring);
        assert_eq!(seen.len(), BIG as usize + 1);
        assert_eq!(seen[..2], [2, 3]);
        assert_eq!(seen[seen.len() - 3..], [200, 201, 202]);
    }

    /// Fix round 2: ровно на пороге `max_messages == RECOUNT_MAX` действует
    /// пересчёт (`<=` в `admits`): в окне отставания запись принимается, а
    /// при настоящих 64 сообщениях `try_write_message` -- `QueueFull` без
    /// запаса `+1` (правило «по счётчику» его бы дало).
    #[test]
    fn recount_applies_exactly_at_recount_max() {
        let (ring, _mem) = make_ring_with(&RingConfig::new(8192).max_messages(RECOUNT_MAX));
        let header = ring.header();
        for seq in 0..RECOUNT_MAX {
            ring.try_write_message(&[seq as u8; LEN]).unwrap();
        }
        // Честно полное: 64 сообщения, счётчик 64 -- запаса нет.
        assert_eq!(
            ring.try_write_message(&[100u8; LEN]),
            Err(ShmError::QueueFull),
            "на пороге лимит точный"
        );
        // Окно отставания: читатель сдвинул read_pos, счётчик ещё 64.
        let read = header.read_pos.load(Ordering::Acquire);
        header
            .read_pos
            .store(read + frame_size(LEN) as u32, Ordering::Release);
        ring.try_write_message(&[101u8; LEN]).unwrap();
        ring.decrement_count();
        assert_eq!(header.message_count.load(Ordering::Acquire), RECOUNT_MAX);
        // Снова 64 настоящих сообщения: QueueFull, без +1.
        assert_eq!(
            ring.try_write_message(&[102u8; LEN]),
            Err(ShmError::QueueFull)
        );
        let seen = drain(&ring);
        assert_eq!(seen.len(), RECOUNT_MAX as usize);
        assert_eq!(seen[0], 1);
        assert_eq!(seen[seen.len() - 1], 101);
    }

    /// Бенчмарк ревью Task 18 (I1): запись с перезаписью на пределе лимита
    /// сообщений, кольцо 8 МиБ, 100 000 записей (каждая вытесняет одну).
    /// Запуск: `cargo test --release --lib write_at_count_limit --
    /// --ignored --nocapture`.
    #[test]
    #[ignore = "бенчмарк: запускать вручную в release"]
    fn bench_write_at_count_limit() {
        const WRITES: u32 = 100_000;
        for limit in [16u32, 500, 10_000, 100_000] {
            let (ring, _mem) = make_ring_with(&RingConfig::new(8 << 20).max_messages(limit));
            let payload = [7u8; LEN];
            // До предела: `limit` (+1 при правиле по счётчику).
            while ring.try_write_message(&payload).is_ok() {}
            let started = Instant::now();
            for _ in 0..WRITES {
                let outcome = ring.write_message(&payload).unwrap();
                assert_eq!(outcome.overwritten, 1);
            }
            let per_write = started.elapsed() / WRITES;
            println!("max_messages {limit}: {per_write:?} на запись");
        }
    }
}

#[cfg(test)]
mod stale_flood_tests {
    //! Task 10 N1 (перенесено в Task 11): объект пира, стоящий в наборе
    //! ПОСЛЕ поглощаемого события, проверяется на каждом пустом сигнале.
    //! «Поток» пустых сигналов моделирует событие с ручным сбросом: оно
    //! взведено на каждом ожидании, и без проверки процесс писателя/читателя
    //! (следующий индекс) не был бы виден до таймаута.

    use super::overflow_race_tests::make_ring;
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering as O};

    /// Процесс, который уже завершился (handle держит `ProcessWatch`).
    fn dead_peer() -> ProcessWatch {
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "exit", "0"])
            .spawn()
            .unwrap();
        let watch = ProcessWatch::open(child.id()).unwrap();
        child.wait().unwrap();
        assert!(watch.has_exited());
        watch
    }

    #[test]
    fn data_flood_does_not_hide_writer_death() {
        let (ring, _mem) = make_ring();
        let data = EventHandle::create_unnamed(true).unwrap();
        data.set().unwrap();
        let peer = dead_peer();
        assert_eq!(
            ring.wait_for_data(
                &data,
                Some(&peer),
                None,
                Some(Duration::from_secs(2)),
                SpinPolicy::NONE
            ),
            Err(ShmError::PeerDied)
        );
    }

    #[test]
    fn stale_disconnect_flood_does_not_hide_writer_death() {
        let (ring, _mem) = make_ring();
        let data = EventHandle::create_unnamed(false).unwrap();
        let disconnect = EventHandle::create_unnamed(true).unwrap();
        disconnect.set().unwrap();
        let left = AtomicBool::new(false);
        let peer_left = || left.load(O::Acquire);
        let watch = DisconnectWatch {
            event: &disconnect,
            peer_left: &peer_left,
        };
        let peer = dead_peer();
        let started = Instant::now();
        assert_eq!(
            ring.wait_for_data(
                &data,
                Some(&peer),
                Some(&watch),
                Some(Duration::from_secs(2)),
                SpinPolicy::NONE
            ),
            Err(ShmError::PeerDied)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn space_and_disconnect_floods_do_not_hide_reader_death() {
        let (ring, _mem) = make_ring();
        const LEN: usize = 60_000;
        while ring.try_write_message(&vec![0u8; LEN]).is_ok() {}
        let peer = dead_peer();
        let left = AtomicBool::new(false);
        let peer_left = || left.load(O::Acquire);
        // Пустой SPACE.
        let space = EventHandle::create_unnamed(true).unwrap();
        space.set().unwrap();
        let started = Instant::now();
        assert_eq!(
            ring.wait_for_space(
                LEN,
                Some(&space),
                Some(&peer),
                None,
                Some(Duration::from_secs(2)),
                SpinPolicy::NONE
            ),
            Err(ShmError::PeerDied)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        // Устаревший DISCONNECT.
        let space = EventHandle::create_unnamed(false).unwrap();
        let disconnect = EventHandle::create_unnamed(true).unwrap();
        disconnect.set().unwrap();
        let watch = DisconnectWatch {
            event: &disconnect,
            peer_left: &peer_left,
        };
        let started = Instant::now();
        assert_eq!(
            ring.wait_for_space(
                LEN,
                Some(&space),
                Some(&peer),
                Some(&watch),
                Some(Duration::from_secs(2)),
                SpinPolicy::NONE
            ),
            Err(ShmError::PeerDied)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
