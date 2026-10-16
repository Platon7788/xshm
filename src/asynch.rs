//! Async API single-client каналов (0.10) поверх реактора IOCP
//! ([`crate::reactor`]): `SharedServer::{accept_async, recv_async,
//! send_async}` и `SharedClient::{connect_async, recv_async, send_async}`.
//!
//! Исполнитель-агностично: только `core::task` (`Waker`), никаких таймеров
//! и потоков на канал -- ожидание объектов ядра ведёт общий поток реактора
//! `xsr`. Таймаутов нет: отмена -- `Drop` future (таймаут исполнителя).
//!
//! # Набор ожидания
//!
//! Один `Waker` на набор из трёх одноразовых регистраций (`WaitSet`):
//! событие операции (`DATA` входящего кольца для приёма, `SPACE` исходящего
//! для отправки, `CONNECT_REQ`/`S2C_CONNECT` для рукопожатия), `DISCONNECT`
//! и процесс пира (если известен). Регистрация -- это ожидание: сигнал
//! автосбросного события она **потребляет**. Поэтому:
//!
//! - на каждом опросе сработавшие регистрации разбираются (`harvest`) и
//!   снимаются, несработавшим только обновляется `Waker` (`will_wake` --
//!   без замены, если тот же); заново регистрируется лишь недостающее --
//!   регистрации не копятся между опросами;
//! - когда future завершился или брошен, оставшиеся регистрации снимаются
//!   (`WaitSet::clear`); сигнал, который регистрация успела потребить, но
//!   который никто не разобрал (`FiredNotDelivered` и `AlreadyDelivered`),
//!   возвращается событию (`NtSetEvent`). Лишний сигнал протокол допускает
//!   (читатель решает «пусто» по позициям, писатель перепроверяет место,
//!   сервер поглощает `CONNECT_REQ` без `CLIENT_HELLO`, устаревший
//!   `DISCONNECT` отсекает проверка состояния пира), потерянный -- нет.
//! - `DISCONNECT`, разобранный при ушедшем пире, взводится обратно (правило
//!   `ring::DisconnectWatch`: событие одно на обе стороны); при живом пире
//!   это устаревший сигнал -- поглощается.
//!
//! # Приём без потерянного пробуждения
//!
//! Порядок опроса `recv_async`: разобрать сработавшее -> читать -> пусто:
//! зарегистрировать `[DATA, DISCONNECT, процесс пира]` -> **затем**
//! перепроверка `RingBuffer::is_empty` (полный барьер + повторное чтение
//! `write_pos`, Dekker-пара с `publish` писателя) -> `Pending`.
//! Писатель публикует кадр, ставит барьер и сигналит `DATA`, если увидел
//! `read_pos ==` прежнему `write_pos`. Барьеры гарантируют: либо наша
//! перепроверка увидит новый кадр (читаем сразу), либо писатель увидит
//! наш `read_pos` и взведёт `DATA`. Взвод до привязки пакета оставляет
//! событие взведённым (автосброс снимает только ожидание), и привязка
//! потребляет его сразу (`AlreadySignaled`, пакет всё равно в порту) --
//! `Waker` разбудят; взвод после привязки будит обычным путём. Регистрация
//! стоит ДО перепроверки, чтобы между «пусто» и сном не было окна, в котором
//! сигнал мог бы уйти другому ожиданию этой стороны: единственные
//! потребители `DATA` этого кольца -- наши регистрации, и все они к моменту
//! `Pending` либо живы, либо разобраны.
//!
//! Отключение и смерть пира сообщаются только при пустом кольце: пир пишет
//! данные до `DISCONNECT`/выхода, а мы разбираем сработавшее ДО чтения, так
//! что последнее сообщение прочитается раньше ошибки («сначала дочитать
//! кольцо»).
//!
//! # Отправка без потерянного пробуждения
//!
//! `send_async`: разобрать сработавшее -> `try_send` -> `QueueFull`: заявка
//! `space_waiter` (PAD + кадр, `arm_space_waiter_for`) -> регистрации
//! `[SPACE, DISCONNECT, процесс пира]` -> перепроверка `FreeSpace::fits` ->
//! `Pending`. Заявка с барьером и перепроверка -- Dekker-пара с
//! `take_space_waiter` читателя (`fence` после сдвига `read_pos`): либо мы
//! увидим освобождённое место, либо читатель увидит заявку и взведёт
//! `SPACE`, который наша регистрация потребит (сразу или позже). Заявка
//! снимается при завершении и при `Drop` future.
//!
//! # Владение и `Drop`
//!
//! Future держит `&mut` стороны канала: пока он жив, сторону не уничтожить,
//! так что регистрации никогда не переживают свои handle (ядро к тому же
//! держит ссылку на объект привязанного пакета). Реактор хранит только
//! `Waker` и пакет; после `Drop` future возможно не более одного уже
//! начатого пробуждения его `Waker` (контракт реактора, ложное пробуждение
//! допустимо).
//!
//! # Не «fused»
//!
//! `recv_async`/`send_async` выполняют операцию на каждом опросе: после
//! `Ready` future снова не опрашивают (повторный опрос выполнит ещё одну
//! операцию); `connect_async` после `Ready` отвечает `Err(NotReady)`.
//!
//! # Одна операция за раз
//!
//! Методы берут `&mut self`: сторона -- `!Sync` (SPSC), а future по `&mut`
//! остаётся `Send` (годится для `tokio::spawn`). Одновременные приём и
//! отправка на одной стороне требуют разделения по задачам с обёрткой
//! (`Mutex`/канал команд) -- или двух разных сторон.

use std::future::Future;
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle};
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use crate::error::{Result, ShmError};
use crate::events::SharedEvents;
use crate::reactor::{CancelOutcome, Reactor, Registration};
use crate::ring::RingBuffer;
use crate::win::EventHandle;

/// Ошибка async API на канале без событий (anonymous-сервер): ждать в
/// реакторе нечего.
pub(crate) const ANONYMOUS: ShmError =
    ShmError::InvalidConfig("anonymous channel has no events: async API needs a named channel");

/// Сторона канала глазами async API (реализуют `SharedServer` и
/// `SharedClient`).
pub(crate) trait Endpoint {
    /// `Err(NotConnected)` вне подключения.
    fn ensure_connected(&self) -> Result<()>;
    /// События канала (`None` -- anonymous).
    fn channel_events(&self) -> Option<&SharedEvents>;
    /// `DATA` входящего кольца.
    fn rx_data(events: &SharedEvents) -> &EventHandle;
    /// `SPACE` исходящего кольца.
    fn tx_space(events: &SharedEvents) -> &EventHandle;
    /// Входящее и исходящее кольца.
    fn rx(&self) -> &RingBuffer;
    fn tx(&self) -> &RingBuffer;
    /// Handle процесса пира (если наблюдается).
    fn peer_process(&self) -> Option<BorrowedHandle<'_>>;
    /// Пир штатно ушёл: его состояние handshake -- не `SERVER_READY`
    /// (клиент дополнительно сверяет `generation` сессии).
    fn peer_left(&self) -> bool;
    /// Приём с сигналом `SPACE` (`receive_from_*`).
    fn receive(&self, buf: &mut Vec<u8>) -> Result<usize>;
    /// Отправка без перезаписи с сигналом `DATA` (`try_send_to_*`).
    fn try_send(&self, data: &[u8]) -> Result<()>;
    /// Пир ушёл, кольцо дочитано: отметить отключение (`mark_disconnected`).
    fn mark_gone(&mut self);
}

/// Числовой handle для реактора.
fn raw(handle: BorrowedHandle<'_>) -> isize {
    handle.as_raw_handle().addr().cast_signed()
}

// ============================================================================
// Набор регистраций
// ============================================================================

/// Индексы набора: событие операции, `DISCONNECT`, процесс пира.
pub(crate) const OP: usize = 0;
pub(crate) const DISCONNECT: usize = 1;
pub(crate) const PEER: usize = 2;

/// Набор одноразовых регистраций одного future (один `Waker`).
#[derive(Debug, Default)]
pub(crate) struct WaitSet {
    regs: [Option<Registration>; 3],
}

impl WaitSet {
    /// Разобрать набор: сработавшие регистрации снять и отметить в ответе,
    /// несработавшим -- обновить `Waker` (без замены на эквивалентный).
    pub(crate) fn harvest(&mut self, waker: &Waker) -> [bool; 3] {
        let mut fired = [false; 3];
        for (slot, fired) in self.regs.iter_mut().zip(&mut fired) {
            // `set_waker == false` -- реактор уже снял регистрацию: сигнал
            // потреблён и доставлен нам (пробуждение было или идёт).
            if slot.as_ref().is_some_and(|reg| !reg.set_waker(waker)) {
                *slot = None;
                *fired = true;
            }
        }
        fired
    }

    /// Зарегистрировать ожидание `handle` в позиции `index`, если её нет.
    pub(crate) fn ensure(&mut self, index: usize, handle: isize, waker: &Waker) -> Result<()> {
        if let Some(slot) = self.regs.get_mut(index)
            && slot.is_none()
        {
            *slot = Some(Reactor::global()?.register(handle, waker.clone())?);
        }
        Ok(())
    }

    /// То же для `BorrowedHandle`.
    pub(crate) fn ensure_handle(
        &mut self,
        index: usize,
        handle: BorrowedHandle<'_>,
        waker: &Waker,
    ) -> Result<()> {
        self.ensure(index, raw(handle), waker)
    }

    /// Снять регистрацию `index`; если она успела потребить сигнал, который
    /// никто не разобрал, -- вернуть его автосбросному событию `restore`
    /// (`None` -- событие-уведомление или возвращать не нужно).
    pub(crate) fn cancel(&mut self, index: usize, restore: Option<&EventHandle>) {
        let Some(reg) = self.regs.get_mut(index).and_then(Option::take) else {
            return;
        };
        match reg.cancel() {
            CancelOutcome::NotFired => {}
            // Сигнал потреблён: снятый пакет (`FiredNotDelivered`) или
            // пробуждение `Waker`, которое уже никто не разберёт
            // (`AlreadyDelivered`, future завершён/брошен). Лишний сигнал
            // протокол допускает, потерянный -- нет.
            CancelOutcome::FiredNotDelivered | CancelOutcome::AlreadyDelivered => {
                if let Some(event) = restore {
                    // Ошибка взвода -- только у негодного handle; вернуть
                    // её из `Drop` некуда.
                    let _ = event.set();
                }
            }
        }
    }

    /// Снять всё: событие операции `op` и `DISCONNECT` -- с возвратом
    /// потреблённого сигнала, процесс пира (уведомление, остаётся
    /// взведённым) -- без.
    pub(crate) fn clear(&mut self, op: Option<&EventHandle>, disconnect: Option<&EventHandle>) {
        self.cancel(OP, op);
        self.cancel(DISCONNECT, disconnect);
        self.cancel(PEER, None);
    }
}

/// Причина конца сессии, замеченная по сработавшим регистрациям.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gone {
    /// `DISCONNECT` при ушедшем пире.
    Disconnected,
    /// Процесс пира завершился.
    Died,
}

impl Gone {
    const fn error(self) -> ShmError {
        match self {
            Self::Disconnected => ShmError::NotConnected,
            Self::Died => ShmError::PeerDied,
        }
    }
}

/// Разобрать `DISCONNECT`/процесс пира из сработавшего. `DISCONNECT` при
/// ушедшем пире взводится обратно (событие одно на обе стороны и
/// автосбросное -- правило `DisconnectWatch`); при живом -- устаревший
/// сигнал, поглощён.
fn observe_gone<E: Endpoint>(endpoint: &E, fired: [bool; 3], gone: &mut Option<Gone>) {
    if fired[DISCONNECT] && endpoint.peer_left() {
        if let Some(events) = endpoint.channel_events() {
            let _ = events.disconnect.set();
        }
        gone.get_or_insert(Gone::Disconnected);
    }
    if fired[PEER] {
        gone.get_or_insert(Gone::Died);
    }
}

/// Зарегистрировать `DISCONNECT` и процесс пира (если наблюдается).
fn ensure_session<E: Endpoint>(endpoint: &E, waits: &mut WaitSet, waker: &Waker) -> Result<()> {
    let Some(events) = endpoint.channel_events() else {
        return Err(ANONYMOUS);
    };
    waits.ensure_handle(DISCONNECT, events.disconnect.as_handle(), waker)?;
    if let Some(peer) = endpoint.peer_process() {
        waits.ensure_handle(PEER, peer, waker)?;
    }
    Ok(())
}

// ============================================================================
// Приём
// ============================================================================

/// Future `recv_async`.
pub(crate) struct RecvFuture<'a, E: Endpoint> {
    endpoint: &'a mut E,
    buf: &'a mut Vec<u8>,
    waits: WaitSet,
    gone: Option<Gone>,
}

impl<'a, E: Endpoint> RecvFuture<'a, E> {
    pub(crate) fn new(endpoint: &'a mut E, buf: &'a mut Vec<u8>) -> Self {
        Self {
            endpoint,
            buf,
            waits: WaitSet::default(),
            gone: None,
        }
    }

    fn step(&mut self, waker: &Waker) -> Poll<Result<usize>> {
        if let Err(err) = self.endpoint.ensure_connected() {
            return Poll::Ready(Err(err));
        }
        let Some(events) = self.endpoint.channel_events() else {
            return Poll::Ready(Err(ANONYMOUS));
        };
        let data = raw(E::rx_data(events).as_handle());
        // Сначала разбор сработавшего, потом чтение: пир пишет данные до
        // `DISCONNECT`/выхода, поэтому замеченный уход не обгонит его
        // последнее сообщение.
        let fired = self.waits.harvest(waker);
        observe_gone(&*self.endpoint, fired, &mut self.gone);
        loop {
            match self.endpoint.receive(self.buf) {
                Ok(len) => return Poll::Ready(Ok(len)),
                Err(ShmError::QueueEmpty) => {}
                Err(err) => return Poll::Ready(Err(err)),
            }
            if let Some(gone) = self.gone {
                // Кольцо дочитано: регистрации -- до закрытия handle пира.
                self.finish();
                self.endpoint.mark_gone();
                return Poll::Ready(Err(gone.error()));
            }
            // Регистрации -- ДО перепроверки (см. модуль).
            if let Err(err) = self.waits.ensure(OP, data, waker) {
                return Poll::Ready(Err(err));
            }
            if let Err(err) = ensure_session(&*self.endpoint, &mut self.waits, waker) {
                return Poll::Ready(Err(err));
            }
            if self.endpoint.rx().is_empty() {
                return Poll::Pending;
            }
            // Кадр опубликован между чтением и перепроверкой -- читаем.
        }
    }

    /// Снять регистрации с возвратом потреблённых сигналов.
    fn finish(&mut self) {
        let events = self.endpoint.channel_events();
        self.waits.clear(
            events.map(E::rx_data),
            events.map(|events| &events.disconnect),
        );
    }
}

impl<E: Endpoint> Future for RecvFuture<'_, E> {
    type Output = Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let result = this.step(cx.waker());
        if result.is_ready() {
            this.finish();
        }
        result
    }
}

impl<E: Endpoint> Drop for RecvFuture<'_, E> {
    fn drop(&mut self) {
        self.finish();
    }
}

// ============================================================================
// Отправка
// ============================================================================

/// Future `send_async`.
pub(crate) struct SendFuture<'a, E: Endpoint> {
    endpoint: &'a mut E,
    data: &'a [u8],
    waits: WaitSet,
    gone: Option<Gone>,
    /// Заявка `space_waiter` выставлена -- снять при завершении.
    armed: bool,
}

impl<'a, E: Endpoint> SendFuture<'a, E> {
    pub(crate) fn new(endpoint: &'a mut E, data: &'a [u8]) -> Self {
        Self {
            endpoint,
            data,
            waits: WaitSet::default(),
            gone: None,
            armed: false,
        }
    }

    fn step(&mut self, waker: &Waker) -> Poll<Result<()>> {
        if let Err(err) = self.endpoint.ensure_connected() {
            return Poll::Ready(Err(err));
        }
        let Some(events) = self.endpoint.channel_events() else {
            return Poll::Ready(Err(ANONYMOUS));
        };
        let space = raw(E::tx_space(events).as_handle());
        let fired = self.waits.harvest(waker);
        observe_gone(&*self.endpoint, fired, &mut self.gone);
        let len = self.data.len();
        loop {
            // Место -- раньше ошибки ухода: читатель мог освободить его
            // перед уходом (как `wait_for_space`).
            match self.endpoint.try_send(self.data) {
                Ok(()) => return Poll::Ready(Ok(())),
                Err(ShmError::QueueFull) => {}
                Err(err) => return Poll::Ready(Err(err)),
            }
            if let Some(gone) = self.gone {
                // Сторону НЕ отмечаем отключённой: входящее кольцо ещё
                // может хранить непрочитанное (его дочитает `recv_async`).
                return Poll::Ready(Err(gone.error()));
            }
            // Заявка -> регистрации -> перепроверка (Dekker с
            // `take_space_waiter`, см. модуль). Позицию записи меняет только
            // этот писатель, так что повторная заявка на опросе та же.
            self.endpoint.tx().arm_space_waiter_for(len);
            self.armed = true;
            if let Err(err) = self.waits.ensure(OP, space, waker) {
                return Poll::Ready(Err(err));
            }
            if let Err(err) = ensure_session(&*self.endpoint, &mut self.waits, waker) {
                return Poll::Ready(Err(err));
            }
            if !self.endpoint.tx().free_space().fits(len) {
                return Poll::Pending;
            }
            // Место освободилось между попыткой и перепроверкой -- снова.
        }
    }

    /// Снять заявку и регистрации с возвратом потреблённых сигналов.
    fn finish(&mut self) {
        if self.armed {
            self.endpoint.tx().disarm_space_waiter();
            self.armed = false;
        }
        let events = self.endpoint.channel_events();
        self.waits.clear(
            events.map(E::tx_space),
            events.map(|events| &events.disconnect),
        );
    }
}

impl<E: Endpoint> Future for SendFuture<'_, E> {
    type Output = Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let result = this.step(cx.waker());
        if result.is_ready() {
            this.finish();
        }
        result
    }
}

impl<E: Endpoint> Drop for SendFuture<'_, E> {
    fn drop(&mut self) {
        self.finish();
    }
}
