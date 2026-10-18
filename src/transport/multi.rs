//! Однопоточный диспетчер C с защищённым межпоточным пробуждением.
use super::error::check;
use super::{DisconnectReason, Error, MultiOptions, PrivateNamespace, Result, Wait};
use std::ffi::CString;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

#[derive(Debug)]
struct WakeState(Mutex<Option<NonNull<xipc_sys::Multi>>>);
// SAFETY: разделяемый указатель используется только для разрешённого C wake.
// Mutex не допускает пересечения wake с закрытием/освобождением объекта.
unsafe impl Send for WakeState {}
// SAFETY: все обращения к указателю защищены тем же Mutex; next с wake
// может пересекаться по контракту C, start завершается до публикации handle.
unsafe impl Sync for WakeState {}

/// Пробуждает диспетчер из другого потока. После Drop сервера даёт Disconnected.
#[derive(Debug, Clone)]
pub struct MultiWake(Arc<WakeState>);
impl MultiWake {
    /// Прервать текущее либо следующее ожидание события; сигналы объединяются.
    pub fn wake(&self) -> Result<()> {
        let guard = self
            .0
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ptr = guard.ok_or(Error::Disconnected(DisconnectReason::Local))?;
        // SAFETY: guard удерживает аллокацию до возврата, C разрешает wake
        // из другого потока одновременно с next, но не с free.
        check(unsafe { xipc_sys::xipc_multi_wake(ptr.as_ptr()) })
    }
}

/// Идентификатор конкретной сессии конкретного сервера. Старый идентификатор
/// не адресует нового клиента, занявшего тот же слот.
#[derive(Debug, Clone)]
pub struct MultiPeer {
    owner: Arc<WakeState>,
    slot: u32,
    generation: u64,
}
impl MultiPeer {
    /// Номер слота для диагностики.
    pub const fn slot(&self) -> u32 {
        self.slot
    }
}
impl PartialEq for MultiPeer {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.owner, &other.owner)
            && self.slot == other.slot
            && self.generation == other.generation
    }
}
impl Eq for MultiPeer {}

/// Событие диспетчера. Данные заимствованы из буфера вызывающего, а не сервера:
/// пока они читаются, серверу можно отправить ответ.
#[derive(Debug)]
#[non_exhaustive]
pub enum MultiEvent<'buffer> {
    /// Истёк срок без события.
    Timeout,
    /// Началась новая сессия.
    Connected(MultiPeer),
    /// Получено сообщение, скопированное C-ядром в переданный буфер.
    Message {
        peer: MultiPeer,
        data: &'buffer [u8],
    },
    /// Сессия завершилась после доставки последних кадров.
    Disconnected {
        peer: MultiPeer,
        reason: DisconnectReason,
        error: Option<Error>,
    },
    /// Вызван межпоточный wake.
    Wake,
    /// Ошибка слота либо порта; C-диспетчер остаётся пригоден к работе.
    Fault { slot: Option<u32>, error: Error },
}

#[derive(Debug, Default)]
struct Slot {
    generation: u64,
    active: bool,
}

/// Сервер Multi. Создаётся, обслуживается и закрывается одним потоком.
///
/// ```compile_fail
/// fn requires_send<T: Send>() {}
/// requires_send::<xshm::MultiServer>();
/// ```
#[derive(Debug)]
pub struct MultiServer {
    ptr: NonNull<xipc_sys::Multi>,
    wake: Arc<WakeState>,
    slots: Vec<Slot>,
    _namespace: Option<PrivateNamespace>,
    _owner_thread: PhantomData<Rc<()>>,
}
impl MultiServer {
    /// Создать 1..=256 каналов. При ошибке C освобождает частично созданные
    /// ресурсы; Rust освобождает аллокацию и удерживаемое пространство имён.
    pub fn bind(base: &str, count: u32, options: MultiOptions) -> Result<Self> {
        if !(1..=256).contains(&count) {
            return Err(Error::InvalidConfig);
        }
        let base = CString::new(base)?;
        let channel = options.channel.raw()?;
        let raw = xipc_sys::MultiOptions {
            layout: channel.layout,
            security: channel.security,
            ns: channel.ns,
            recv_batch: options.receive_batch,
        };
        let mut slots_ptr = std::ptr::null_mut();
        // SAFETY: count проверен, адрес локального выходного указателя валиден.
        let ptr = NonNull::new(unsafe { xipc_sys::xipc_multi_alloc(count, &raw mut slots_ptr) })
            .ok_or(Error::Allocation)?;
        let server = Self {
            ptr,
            wake: Arc::new(WakeState(Mutex::new(Some(ptr)))),
            slots: (0..count).map(|_| Slot::default()).collect(),
            _namespace: options.channel.namespace,
            _owner_thread: PhantomData,
        };
        // SAFETY: сервер и slots — одна неподвижная C-аллокация. Namespace
        // удерживается server; строка и параметры живы до возврата start.
        let code = unsafe {
            xipc_sys::xipc_multi_start(
                server.ptr.as_ptr(),
                slots_ptr,
                count,
                base.as_ptr(),
                &raw const raw,
            )
        };
        check(code)?;
        Ok(server)
    }
    /// Получить безопасный межпоточный сигнал пробуждения.
    pub fn wake_handle(&self) -> MultiWake {
        MultiWake(self.wake.clone())
    }

    /// Следующее событие. Буфер должен вмещать c2s.max_message_size:
    /// недостаточный буфер отвергается C до извлечения сообщения.
    pub fn next_event<'buffer>(
        &mut self,
        buffer: &'buffer mut [u8],
        wait: Wait,
    ) -> Result<MultiEvent<'buffer>> {
        let cap = u32::try_from(buffer.len()).map_err(|_| Error::LengthTooLarge)?;
        let ms = wait.milliseconds()?;
        let mut event = xipc_sys::MultiEvent::default();
        // SAFETY: owner-thread и &mut self исключают конкурирующие операции,
        // кроме разрешённого wake. C копирует не более cap байт в buffer.
        check(unsafe {
            xipc_sys::xipc_multi_next(
                self.ptr.as_ptr(),
                buffer.as_mut_ptr().cast(),
                cap,
                ms,
                &raw mut event,
            )
        })?;
        match event.kind {
            0 => Ok(MultiEvent::Timeout),
            4 => Ok(MultiEvent::Wake),
            5 => Ok(MultiEvent::Fault {
                slot: if event.slot == u32::MAX {
                    None
                } else {
                    if event.slot as usize >= self.slots.len() {
                        return Err(Error::InvalidCoreResult);
                    }
                    Some(event.slot)
                },
                error: check(event.error).err().ok_or(Error::InvalidCoreResult)?,
            }),
            1..=3 => {
                let state = self
                    .slots
                    .get_mut(event.slot as usize)
                    .ok_or(Error::InvalidCoreResult)?;
                if event.kind == 1 {
                    if state.active {
                        return Err(Error::InvalidCoreResult);
                    }
                    state.generation = state
                        .generation
                        .checked_add(1)
                        .ok_or(Error::InvalidCoreResult)?;
                    state.active = true;
                } else if !state.active {
                    return Err(Error::InvalidCoreResult);
                }
                let peer = MultiPeer {
                    owner: self.wake.clone(),
                    slot: event.slot,
                    generation: state.generation,
                };
                match event.kind {
                    1 => Ok(MultiEvent::Connected(peer)),
                    2 => {
                        let data = buffer
                            .get(..event.len as usize)
                            .ok_or(Error::InvalidCoreResult)?;
                        Ok(MultiEvent::Message { peer, data })
                    }
                    _ => {
                        state.active = false;
                        Ok(MultiEvent::Disconnected {
                            peer,
                            reason: event.reason.into(),
                            error: check(event.error).err(),
                        })
                    }
                }
            }
            _ => Err(Error::InvalidCoreResult),
        }
    }
    fn validate_peer(&self, peer: &MultiPeer) -> Result<u32> {
        let valid = Arc::ptr_eq(&self.wake, &peer.owner)
            && self
                .slots
                .get(peer.slot as usize)
                .is_some_and(|s| s.active && s.generation == peer.generation);
        if valid {
            Ok(peer.slot)
        } else {
            Err(Error::Disconnected(DisconnectReason::Unknown))
        }
    }
    const fn status(code: i32) -> Result<()> {
        if code == 14 {
            Err(Error::Disconnected(DisconnectReason::Unknown))
        } else {
            check(code)
        }
    }
    /// Отправить без вытеснения старых сообщений. Full не теряет данные.
    pub fn try_send(&mut self, peer: &MultiPeer, data: &[u8]) -> Result<()> {
        let slot = self.validate_peer(peer)?;
        let len = u32::try_from(data.len()).map_err(|_| Error::LengthTooLarge)?;
        // SAFETY: peer проверен, уникальный owner-thread, срез живёт весь вызов.
        Self::status(unsafe {
            xipc_sys::xipc_multi_try_send(self.ptr.as_ptr(), slot, data.as_ptr().cast(), len)
        })
    }
    /// Явно разрешить вытеснение старых сообщений. C Multi не возвращает счётчик.
    pub fn send_overwriting(&mut self, peer: &MultiPeer, data: &[u8]) -> Result<()> {
        let slot = self.validate_peer(peer)?;
        let len = u32::try_from(data.len()).map_err(|_| Error::LengthTooLarge)?;
        // SAFETY: те же гарантии владения и времени жизни, что у try_send.
        Self::status(unsafe {
            xipc_sys::xipc_multi_send(self.ptr.as_ptr(), slot, data.as_ptr().cast(), len)
        })
    }
    /// Отключить конкретную сессию; событие Disconnected поступит через next_event.
    pub fn disconnect(&mut self, peer: &MultiPeer) -> Result<()> {
        let slot = self.validate_peer(peer)?;
        // SAFETY: owner-thread, уникальный сервер и проверенная сессия.
        Self::status(unsafe { xipc_sys::xipc_multi_disconnect_client(self.ptr.as_ptr(), slot) })
    }
}
impl Drop for MultiServer {
    fn drop(&mut self) {
        let mut guard = self
            .wake
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = None;
        // SAFETY: owner-thread, нет незавершённых вызовов next (&mut self).
        // Mutex исключает wake до полного free; namespace пока удерживается.
        unsafe { xipc_sys::xipc_multi_free(self.ptr.as_ptr()) };
    }
}
