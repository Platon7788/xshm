use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::constants::{
    HANDSHAKE_CLIENT_HELLO, HANDSHAKE_IDLE, HANDSHAKE_SERVER_READY, RESERVED_CLIENT_PID_INDEX,
    RESERVED_SERVER_PID_INDEX, SHARED_MAGIC, SHARED_VERSION,
};
use crate::error::{Result, ShmError};
use crate::events::SharedEvents;
use crate::naming::mapping_name;
use crate::ring::{DisconnectWatch, FreeSpace, RingBuffer, WriteOutcome};
use crate::server::poll_with_peer;
use crate::shared::SharedView;
use crate::win::{Mapping, ProcessWatch, wait_any};

/// Откат клиентской стороны handshake в IDLE (заявка не принята).
///
/// Вызывается на всех неуспешных путях `SharedClient::connect`: без него в
/// shared memory остался бы «висящий» `CLIENT_HELLO`, который сервер принял бы
/// за живую заявку на подключение. `client_state` здесь уже НЕ `CLIENT_HELLO`
/// (его снял CAS в `withdraw_hello`) -- трогаем только PID и заголовки.
fn rollback_handshake(view: &SharedView) {
    // Заявленный PID отзываем вместе с HELLO: иначе клиент старой версии,
    // пришедший следом, унаследовал бы наш PID (см. RESERVED_CLIENT_PID_INDEX).
    view.control_block().reserved[RESERVED_CLIENT_PID_INDEX].store(0, Ordering::Release);
    let (header_a, header_b) = view.headers();
    header_a
        .handshake_state
        .store(HANDSHAKE_IDLE, Ordering::Release);
    header_b
        .handshake_state
        .store(HANDSHAKE_IDLE, Ordering::Release);
}

/// Отозвать заявку `CLIENT_HELLO` (таймаут, остановка, ошибка ожидания).
///
/// Ревизия 2 (25.09.2026): точка фиксации рукопожатия -- `client_state`.
/// Сервер фиксирует его CAS `CLIENT_HELLO -> SERVER_READY`
/// (`SharedServer::complete_handshake`), клиент отзывает CAS `CLIENT_HELLO ->
/// IDLE`; выигрывает ровно один. Раньше обе стороны писали store: клиент,
/// откатившийся в момент ответа сервера, оставлял сервер «подключённым» к
/// никому, а взведённый `S2C_CONNECT` -- следующему клиенту.
///
/// `true` -- отзывать поздно: `client_state` уже не `CLIENT_HELLO`, а
/// `generation` сменился -- сервер принял нас (и, возможно, уже отключил:
/// тогда `DISCONNECT` взведён, worker дочитает кольцо). `false` -- заявка
/// отозвана (или её снял уходящий сервер без рукопожатия), подключения нет.
fn withdraw_hello(view: &SharedView, generation_before: u32) -> bool {
    let control = view.control_block();
    let withdrawn = control
        .client_state
        .compare_exchange(
            HANDSHAKE_CLIENT_HELLO,
            HANDSHAKE_IDLE,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok();
    if !withdrawn && control.generation.load(Ordering::Acquire) != generation_before {
        return true;
    }
    rollback_handshake(view);
    false
}

/// Прерывание ожидания `S2C_CONNECT` (ревизия 2): событие своего процесса
/// (`wake` worker-а) и проверка «пора остановиться». Сработал `wake`, а
/// `should_stop()` вернул `true` -- заявка отзывается, `connect` возвращает
/// `Err(NotReady)`; иначе (например, `wake` от `send`) ожидание продолжается.
pub(crate) struct Interrupt<'a> {
    pub(crate) wake: isize,
    pub(crate) should_stop: &'a mut dyn FnMut() -> bool,
}

/// Чем кончилось ожидание `S2C_CONNECT`.
enum AckWait {
    /// Сервер принял именно нашу заявку (`generation` сменился).
    Accepted,
    /// Таймаут, остановка или ошибка ожидания -- заявку отзывать.
    Abandoned(ShmError),
}

/// Ожидание `S2C_CONNECT` (и `wake` вызывающего, если задан) до `timeout`.
/// Сигнал без смены `generation` -- устаревший (сервер старой версии ответил
/// откатившемуся клиенту) -- поглощается, ожидание продолжается.
fn wait_ack(
    view: &SharedView,
    events: &SharedEvents,
    generation_before: u32,
    timeout: Duration,
    mut interrupt: Option<&mut Interrupt<'_>>,
) -> AckWait {
    let accepted = || view.control_block().generation.load(Ordering::Acquire) != generation_before;
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return AckWait::Abandoned(ShmError::Timeout);
        }
        let fired = match interrupt.as_deref_mut() {
            Some(int) => wait_any(&[events.connect_ack.raw_handle(), int.wake], Some(left)),
            None => events
                .connect_ack
                .wait(Some(left))
                .map(|signaled| signaled.then_some(0)),
        };
        match fired {
            Ok(Some(0)) => {
                if accepted() {
                    return AckWait::Accepted;
                }
            }
            Ok(Some(_)) => {
                if interrupt
                    .as_deref_mut()
                    .is_some_and(|int| (int.should_stop)())
                {
                    return AckWait::Abandoned(ShmError::NotReady);
                }
            }
            Ok(None) => return AckWait::Abandoned(ShmError::Timeout),
            Err(err) => return AckWait::Abandoned(err),
        }
    }
}

#[derive(Debug)]
pub struct SharedClient {
    _mapping: Mapping,
    view: SharedView,
    events: SharedEvents,
    ring_tx: RingBuffer,
    ring_rx: RingBuffer,
    connected: bool,
    /// Удерживаемый handle процесса сервера (сервер 0.8+ опубликовал PID и
    /// процесс удалось открыть) -- для детекции его смерти.
    peer: Option<ProcessWatch>,
    /// `generation` сессии: сервер публикует новый при каждом рукопожатии,
    /// так что расхождение значит «канал/слот уже у другой сессии».
    generation: u32,
}

// SAFETY: см. `SharedServer` -- владеющий `Mapping` плюс атомарные операции в
// shared memory, привязки к потоку нет.
unsafe impl Send for SharedClient {}

impl SharedClient {
    pub fn connect(name: &str, timeout: Duration) -> Result<Self> {
        Self::connect_impl(name, timeout, true, None)
    }

    /// Подключение, ожидание которого прерывает `interrupt` (worker-ы
    /// Auto/Dispatch/Multi: `stop`/Drop не ждут `connect_timeout`).
    pub(crate) fn connect_interruptible(
        name: &str,
        timeout: Duration,
        interrupt: Interrupt<'_>,
    ) -> Result<Self> {
        Self::connect_impl(name, timeout, true, Some(interrupt))
    }

    /// `announce_pid = false` -- поведение клиента 0.7/0.8.0 без обмена PID
    /// (нужно тестам совместимости версий).
    ///
    /// Порядок (ревизия 2, INTEROP §d.2 шаг 3a, R15): запомнить `generation`
    /// -> сбросить `S2C_CONNECT` -> PID в `reserved[3]` -> `client_state =
    /// CLIENT_HELLO` -> заголовки -> `C2S_CONNECT_REQ` -> ждать
    /// `S2C_CONNECT`. Рукопожатие состоялось, если `generation` сменился --
    /// даже когда сервер уже успел отключить нас (`server_state == IDLE`):
    /// тогда `DISCONNECT` взведён, и worker дочитает кольцо (прощальное
    /// сообщение), а не потеряет его в откате.
    pub(crate) fn connect_impl(
        name: &str,
        timeout: Duration,
        announce_pid: bool,
        mut interrupt: Option<Interrupt<'_>>,
    ) -> Result<Self> {
        let map_name = mapping_name(name);
        let mapping = Mapping::open(&map_name)?;
        // SAFETY: `Mapping::open` уже проверил, что отображение не меньше
        // `shared_mapping_size()`, и держит его живым, пока жив `mapping`.
        let view = unsafe { SharedView::new(mapping.as_ptr()) };

        // Проверка magic и version для валидации shared memory
        let control = view.control_block();
        if control.magic != SHARED_MAGIC {
            return Err(ShmError::Corrupted);
        }
        if control.version != SHARED_VERSION {
            return Err(ShmError::HandshakeFailed);
        }

        let events = SharedEvents::open(name)?;

        // Ревизия 2: `generation` ДО заявки -- по его смене узнаём, что
        // сервер принял именно нас (новый generation он публикует в
        // `complete_handshake` раньше, чем фиксирует рукопожатие CAS-ом).
        let generation_before = control.generation.load(Ordering::Acquire);
        // Ревизия 2: `S2C_CONNECT`, оставшийся от чужого рукопожатия (сервер
        // старой версии ответил клиенту, который уже откатился), иначе
        // «подтвердил» бы нашу заявку раньше сервера. Сбрасываем ДО
        // `CLIENT_HELLO`: ответ на нашу заявку может прийти только после.
        let _ = events.connect_ack.reset();

        // Протокол 0.8+: PID клиента -- ДО CLIENT_HELLO, чтобы Release-публикация
        // client_state сделала его видимым серверу вместе с заявкой.
        if announce_pid {
            control.reserved[RESERVED_CLIENT_PID_INDEX]
                .store(std::process::id(), Ordering::Release);
        }

        control
            .client_state
            .store(HANDSHAKE_CLIENT_HELLO, Ordering::Release);

        let (header_a, header_b) = view.headers();
        header_a
            .handshake_state
            .store(HANDSHAKE_CLIENT_HELLO, Ordering::Release);
        header_b
            .handshake_state
            .store(HANDSHAKE_CLIENT_HELLO, Ordering::Release);

        let waited = match events.connect_req.set() {
            Ok(()) => wait_ack(
                &view,
                &events,
                generation_before,
                timeout,
                interrupt.as_mut(),
            ),
            Err(err) => AckWait::Abandoned(err),
        };
        // Отзыв проиграл CAS сервера -- рукопожатие состоялось: продолжаем
        // как подключённые (сервер ждёт нас; если уже отключил -- worker
        // увидит `DISCONNECT` и дочитает кольцо).
        if let AckWait::Abandoned(err) = waited
            && !withdraw_hello(&view, generation_before)
        {
            return Err(err);
        }

        // PID сервера (0 -- сервер старой версии: наблюдения нет, как в 0.7).
        let peer = ProcessWatch::open_peer(
            control.reserved[RESERVED_SERVER_PID_INDEX].load(Ordering::Acquire),
        );

        let generation = control.generation.load(Ordering::Acquire);
        header_a.connection_gen.store(generation, Ordering::Release);
        header_b.connection_gen.store(generation, Ordering::Release);
        header_a
            .handshake_state
            .store(HANDSHAKE_SERVER_READY, Ordering::Release);
        header_b
            .handshake_state
            .store(HANDSHAKE_SERVER_READY, Ordering::Release);

        // SAFETY: указатели из `SharedView` -- внутрь живого маппинга, выровнены
        // по layout'у и не пересекаются; `mapping` переезжает в возвращаемую
        // структуру, поэтому переживает кольца.
        let ring_tx = unsafe { RingBuffer::new(view.ring_header_b(), view.ring_buffer_b()) };
        // SAFETY: см. выше (встречное кольцо того же маппинга).
        let ring_rx = unsafe { RingBuffer::new(view.ring_header_a(), view.ring_buffer_a()) };

        let client = Self {
            _mapping: mapping,
            view,
            events,
            ring_tx,
            ring_rx,
            connected: true,
            peer,
            generation,
        };

        Ok(client)
    }

    /// Сессия всё ещё наша: `generation` не сменился (ревизия 2). Сменился
    /// -- сервер уже провёл рукопожатие с другим клиентом этого канала/слота
    /// (а наш `DISCONNECT` мог быть сброшен его `complete_handshake`).
    pub(crate) fn is_session_current(&self) -> bool {
        self.view.control_block().generation.load(Ordering::Acquire) == self.generation
    }

    #[must_use]
    pub const fn is_connected(&self) -> bool {
        self.connected
    }

    pub(crate) const fn events(&self) -> &SharedEvents {
        &self.events
    }

    pub(crate) fn mark_disconnected(&mut self) {
        self.connected = false;
        self.peer = None;
    }

    const fn ensure_connected(&self) -> Result<()> {
        if !self.connected {
            Err(ShmError::NotConnected)
        } else {
            Ok(())
        }
    }

    pub fn send_to_server(&self, payload: &[u8]) -> Result<WriteOutcome> {
        self.ensure_connected()?;
        let result = self.ring_tx.write_message(payload)?;
        if result.was_empty {
            let _ = self.events.c2s.data.set();
        }
        Ok(result)
    }

    /// Отправка серверу **без перезаписи** непрочитанных данных.
    ///
    /// Сообщение либо целиком ложится в свободное место кольца
    /// Client -> Server, либо `Err(ShmError::QueueFull)` и кольцо не
    /// меняется. Семантика -- `SharedServer::try_send_to_client`.
    pub fn try_send_to_server(&self, payload: &[u8]) -> Result<WriteOutcome> {
        self.ensure_connected()?;
        let result = self.ring_tx.try_write_message(payload)?;
        if result.was_empty {
            let _ = self.events.c2s.data.set();
        }
        Ok(result)
    }

    /// Свободное место в кольце Client -> Server (нижняя граница из потока
    /// писателя, см. `SharedServer::free_space`). Вне подключения --
    /// `FreeSpace::ZERO`.
    #[must_use]
    pub fn free_space(&self) -> FreeSpace {
        if self.connected {
            self.ring_tx.free_space()
        } else {
            FreeSpace::ZERO
        }
    }

    /// Ждать места в кольце Client -> Server под payload длины `payload_len`
    /// (семантика -- `SharedServer::wait_for_space`).
    ///
    /// Набор ожидания -- `[C2S_SPACE, S2C_DISCONNECT, процесс сервера]`,
    /// одно ожидание на весь `timeout`; `DISCONNECT` при `server_state !=
    /// SERVER_READY` -- `Err(NotConnected)`.
    pub fn wait_for_space(&self, payload_len: usize, timeout: Option<Duration>) -> Result<bool> {
        self.ensure_connected()?;
        let control = self.view.control_block();
        let peer_left = || control.server_state.load(Ordering::Acquire) != HANDSHAKE_SERVER_READY;
        let disconnect = DisconnectWatch {
            event: &self.events.disconnect,
            peer_left: &peer_left,
        };
        self.ring_tx.wait_for_space(
            payload_len,
            Some(&self.events.c2s.space),
            self.peer.as_ref(),
            Some(&disconnect),
            timeout,
        )
    }

    /// Для auto-режима: заявка/снятие ожидания места в исходящем кольце.
    pub(crate) fn arm_space_waiter(&self, frame: u32) {
        self.ring_tx.arm_space_waiter(frame);
    }

    pub(crate) fn disarm_space_waiter(&self) {
        self.ring_tx.disarm_space_waiter();
    }

    pub fn receive_from_server(&self, buffer: &mut Vec<u8>) -> Result<usize> {
        self.ensure_connected()?;
        let len = self.ring_rx.read_message(buffer)?;
        // SPACE -- когда кольцо опустело ИЛИ писатель-сервер ждёт места и
        // его теперь хватает (см. `SharedServer::receive_from_client`).
        if self.ring_rx.take_space_waiter() || self.ring_rx.message_count() == 0 {
            let _ = self.events.s2c.space.set();
        }
        Ok(len)
    }

    pub fn poll_server(&self, timeout: Option<Duration>) -> Result<bool> {
        self.ensure_connected()?;
        if !self.ring_rx.is_empty() {
            return Ok(true);
        }
        poll_with_peer(
            &self.events.s2c.data,
            self.peer.as_ref(),
            &self.ring_rx,
            timeout,
        )
    }

    /// PID процесса сервера, если сервер 0.8+ опубликовал его.
    #[must_use]
    pub fn peer_pid(&self) -> Option<u32> {
        self.peer.as_ref().map(ProcessWatch::pid)
    }

    /// Жив ли процесс сервера (семантика -- `SharedServer::is_peer_alive`).
    #[must_use]
    pub fn is_peer_alive(&self) -> Option<bool> {
        if !self.connected {
            return None;
        }
        self.peer.as_ref().map(|peer| !peer.has_exited())
    }

    pub(crate) fn peer_wait_handle(&self) -> Option<isize> {
        self.peer.as_ref().map(ProcessWatch::raw_handle)
    }
}

impl Drop for SharedClient {
    fn drop(&mut self) {
        if self.connected {
            // R14: сначала своё состояние -> IDLE, потом `DISCONNECT`.
            // Ревизия 2: CAS `SERVER_READY -> IDLE`, а не store -- не затирает
            // `CLIENT_HELLO` следующего клиента. `reserved[3]` не трогаем:
            // наш PID сервер уже забрал (`swap` в рукопожатии), а следующий
            // клиент мог записать туда свой.
            let _ = self.view.control_block().client_state.compare_exchange(
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
            let _ = self.events.disconnect.set();
            self.connected = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::SharedServer;

    /// Отзыв заявки против фиксации сервера: CAS решает однозначно.
    #[test]
    fn withdraw_hello_loses_only_to_a_committed_handshake() {
        let name = format!("XSHM_WITHDRAW_{}", std::process::id());
        let server = SharedServer::start(&name).unwrap();
        let view = server.view();
        let control = view.control_block();

        // Никто не ответил -- отзыв удался.
        let before = control.generation.load(Ordering::Acquire);
        control
            .client_state
            .store(HANDSHAKE_CLIENT_HELLO, Ordering::Release);
        assert!(!withdraw_hello(view, before));
        assert_eq!(control.client_state.load(Ordering::Acquire), HANDSHAKE_IDLE);

        // Сервер успел: generation опубликован, CAS HELLO -> READY выигран.
        control
            .client_state
            .store(HANDSHAKE_CLIENT_HELLO, Ordering::Release);
        control.generation.fetch_add(1, Ordering::AcqRel);
        control
            .client_state
            .compare_exchange(
                HANDSHAKE_CLIENT_HELLO,
                HANDSHAKE_SERVER_READY,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .unwrap();
        assert!(withdraw_hello(view, before), "рукопожатие состоялось");
        assert_eq!(
            control.client_state.load(Ordering::Acquire),
            HANDSHAKE_SERVER_READY
        );

        // Уходящий сервер снял заявку без рукопожатия (generation тот же).
        let before = control.generation.load(Ordering::Acquire);
        control
            .client_state
            .store(HANDSHAKE_IDLE, Ordering::Release);
        assert!(!withdraw_hello(view, before));
    }
}
