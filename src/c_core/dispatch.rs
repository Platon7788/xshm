//! Выделенные каналы через C-лобби; Rust управляет только владением и callbacks.
use super::error::check;
use super::{DisconnectReason, DispatchOptions, Error, PrivateNamespace, Result, Wait};
use std::collections::BTreeMap;
use std::ffi::{CString, c_void};
use std::fmt;
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

/// Данные регистрации: PID заявлен пиром, а не является аутентификацией.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    /// PID из сообщения регистрации.
    pub pid: u32,
    /// Прикладная ревизия клиента.
    pub revision: u16,
    /// UTF-8 имя, сохраняющее в том числе NUL внутри wire-регистрации.
    pub name: String,
}
impl Registration {
    fn copy_from(raw: &xipc_sys::Registration) -> Result<Self> {
        if raw.name_len > 64 {
            return Err(Error::InvalidCoreResult);
        }
        let name = std::str::from_utf8(&raw.name[..usize::from(raw.name_len)])
            .map_err(|_| Error::InvalidCoreResult)?
            .to_owned();
        Ok(Self {
            pid: raw.pid,
            revision: raw.revision,
            name,
        })
    }
}

/// Диагностика успешной клиентской регистрации.
#[derive(Debug, Clone)]
pub struct DispatchConnectionInfo {
    /// Прикладной идентификатор, назначенный сервером.
    pub client_id: u32,
    /// Имя выделенного канала; после его завершения требуется новая регистрация.
    pub channel_name: String,
}

#[derive(Debug)]
struct WakeState(Mutex<Option<NonNull<xipc_sys::Dispatch>>>);
// SAFETY: shared-указатель используется только для C wake под Mutex;
// закрытие владельцем проходит под тем же Mutex после всех вызовов wake.
unsafe impl Send for WakeState {}
// SAFETY: доступ к указателю сериализован; C разрешает wake параллельно next.
unsafe impl Sync for WakeState {}
/// Межпоточный сигнал; безопасен после закрытия сервера и возвращает Disconnected.
#[derive(Debug, Clone)]
pub struct DispatchWake(Arc<WakeState>);
impl DispatchWake {
    /// Прервать текущее или следующее ожидание; сигналы могут объединяться.
    pub fn wake(&self) -> Result<()> {
        let guard = self
            .0
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ptr = guard.ok_or(Error::Disconnected(DisconnectReason::Local))?;
        // SAFETY: guard запрещает free до возврата, wake — разрешённая C операция.
        check(unsafe { xipc_sys::xipc_dispatch_wake(ptr.as_ptr()) })
    }
}

/// Конкретная сессия конкретного сервера, включая защиту от повторного client_id.
#[derive(Debug, Clone)]
pub struct DispatchPeer {
    owner: Arc<WakeState>,
    id: u32,
    generation: u64,
}
impl DispatchPeer {
    /// Идентификатор C для диагностики и сопоставления клиентской регистрации.
    pub const fn client_id(&self) -> u32 {
        self.id
    }
}
impl PartialEq for DispatchPeer {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.owner, &other.owner)
            && self.id == other.id
            && self.generation == other.generation
    }
}
impl Eq for DispatchPeer {}

/// Событие с принадлежащей Rust регистрацией и данными в буфере вызывающего.
#[derive(Debug)]
#[non_exhaustive]
pub enum DispatchEvent<'buffer> {
    /// Истёк срок без события.
    Timeout,
    /// Выделенный канал подключён; до этого сообщения не доставляются.
    Connected {
        peer: DispatchPeer,
        registration: Arc<Registration>,
    },
    /// Сообщение можно использовать для ответа без дополнительной копии.
    Message {
        peer: DispatchPeer,
        data: &'buffer [u8],
        registration: Arc<Registration>,
    },
    /// Последние кадры доставлены, сессия завершена.
    Disconnected {
        peer: DispatchPeer,
        reason: DisconnectReason,
        error: Option<Error>,
        registration: Arc<Registration>,
    },
    /// Получен межпоточный сигнал.
    Wake,
    /// Ошибка лобби/порта (None) либо конкретного client_id; цикл C продолжает работу.
    Fault {
        client_id: Option<u32>,
        error: Error,
    },
}

type Admit = dyn FnMut(&Registration) -> bool;
#[derive(Debug, Clone, Copy)]
enum CallbackFailure {
    Panic,
    Core,
}
impl CallbackFailure {
    const fn error(self) -> Error {
        match self {
            Self::Panic => Error::AdmissionPanicked,
            Self::Core => Error::InvalidCoreResult,
        }
    }
}
struct Admission {
    callback: Box<Admit>,
    server: NonNull<xipc_sys::Dispatch>,
    failure: Option<CallbackFailure>,
}
impl fmt::Debug for Admission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Admission")
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}
unsafe extern "C" fn admit(ctx: *mut c_void, reg: *const xipc_sys::Registration) -> bool {
    // SAFETY: C хранит указатель на неподвижный Box<Admission> и вызывает его
    // только синхронно внутри next владельца. Rust не обращается к Box в это время.
    let admission = unsafe { &mut *ctx.cast::<Admission>() };
    if admission.failure.is_some() {
        return false;
    }
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<bool> {
        // SAFETY: C передаёт непустой указатель на декодированную регистрацию,
        // живую весь callback; копирование не сохраняет заимствование C-памяти.
        let registration = Registration::copy_from(unsafe { &*reg })?;
        Ok((admission.callback)(&registration))
    }));
    match result {
        Ok(Ok(accepted)) => return accepted,
        Ok(Err(_)) => admission.failure = Some(CallbackFailure::Core),
        Err(payload) => {
            // Произвольный panic payload может паниковать в Drop: не допускаем
            // второго unwind на C-границе. Сервер после ошибки прекращает callbacks.
            std::mem::forget(payload);
            admission.failure = Some(CallbackFailure::Panic);
        }
    }
    // SAFETY: server живёт весь next. Контракт C разрешает из admit только
    // wake: это публикация события, без повторного входа в цикл/изменения сессии.
    // Поэтому Infinite next тоже вернётся и отдаст Rust сохранённую ошибку.
    unsafe { xipc_sys::xipc_dispatch_wake(admission.server.as_ptr()) };
    false
}

/// Однопоточный C-диспетчер. Для фоновой работы создавайте его в потоке-владельце.
///
/// ```compile_fail
/// fn requires_send<T: Send>() {}
/// requires_send::<xshm::c_core::DispatchServer>();
/// ```
#[derive(Debug)]
pub struct DispatchServer {
    ptr: NonNull<xipc_sys::Dispatch>,
    wake: Arc<WakeState>,
    active: BTreeMap<u32, ActiveSession>,
    generation: u64,
    admission: Option<Box<Admission>>,
    _namespace: Option<PrivateNamespace>,
    _owner_thread: PhantomData<Rc<()>>,
}
#[derive(Debug)]
struct ActiveSession {
    generation: u64,
    registration: Arc<Registration>,
}
impl DispatchServer {
    /// Создать сервер, допускающий всех клиентов в пределах max_clients.
    pub fn bind(lobby: &str, options: DispatchOptions) -> Result<Self> {
        Self::bind_inner(lobby, options, None)
    }
    /// Admission вызывается потоком-владельцем до создания канала. При panic=unwind
    /// паника становится AdmissionPanicked; обработчик больше не вызывается.
    /// При panic=abort процесс завершается по политике сборки, перехват невозможен.
    pub fn bind_with_admission<F>(
        lobby: &str,
        options: DispatchOptions,
        callback: F,
    ) -> Result<Self>
    where
        F: FnMut(&Registration) -> bool + 'static,
    {
        Self::bind_inner(lobby, options, Some(Box::new(callback)))
    }
    fn bind_inner(
        lobby: &str,
        options: DispatchOptions,
        callback: Option<Box<Admit>>,
    ) -> Result<Self> {
        let count = options.max_clients;
        if !(1..=256).contains(&count) {
            return Err(Error::InvalidConfig);
        }
        let lobby = CString::new(lobby)?;
        let channel = options.channel.raw()?;
        let mut raw = xipc_sys::DispatchOptions {
            max_clients: count,
            lobby_timeout_ms: Wait::For(options.lobby_timeout).milliseconds()?,
            channel_connect_timeout_ms: Wait::For(options.channel_connect_timeout)
                .milliseconds()?,
            lobby_layout: options.lobby_layout,
            channel_layout: channel.layout,
            security: channel.security,
            ns: channel.ns,
            admit: None,
            admit_ctx: std::ptr::null_mut(),
        };
        let mut slots = std::ptr::null_mut();
        // SAFETY: count проверен, выходной указатель доступен для записи.
        let ptr =
            NonNull::new(unsafe { xipc_sys::xipc_dispatch_server_alloc(count, &raw mut slots) })
                .ok_or(Error::Allocation)?;
        let mut server = Self {
            ptr,
            wake: Arc::new(WakeState(Mutex::new(Some(ptr)))),
            active: BTreeMap::new(),
            generation: 0,
            admission: callback.map(|callback| {
                Box::new(Admission {
                    callback,
                    server: ptr,
                    failure: None,
                })
            }),
            _namespace: options.channel.namespace,
            _owner_thread: PhantomData,
        };
        if let Some(admission) = &mut server.admission {
            raw.admit = Some(admit);
            raw.admit_ctx = std::ptr::from_mut(admission.as_mut()).cast();
        }
        // SAFETY: единая неподвижная аллокация server+slots; namespace и Box
        // admission удерживаются server до завершения C free. Start не вызывает admit.
        check(unsafe {
            xipc_sys::xipc_dispatch_server_start(
                ptr.as_ptr(),
                slots,
                count,
                lobby.as_ptr(),
                &raw const raw,
            )
        })?;
        Ok(server)
    }
    /// Получить межпоточный сигнал пробуждения.
    pub fn wake_handle(&self) -> DispatchWake {
        DispatchWake(self.wake.clone())
    }
    fn callback_result(&self) -> Result<()> {
        match self.admission.as_ref().and_then(|a| a.failure) {
            Some(failure) => Err(failure.error()),
            None => Ok(()),
        }
    }
    /// Буфер должен вмещать channel.layout.c2s.max_message_size. Регистрация
    /// копируется до следующего C next; event не заимствует память C-сервера.
    pub fn next_event<'buffer>(
        &mut self,
        buffer: &'buffer mut [u8],
        wait: Wait,
    ) -> Result<DispatchEvent<'buffer>> {
        self.callback_result()?;
        let cap = u32::try_from(buffer.len()).map_err(|_| Error::LengthTooLarge)?;
        let ms = wait.milliseconds()?;
        let mut raw = xipc_sys::DispatchEvent {
            kind: 0,
            client_id: 0,
            len: 0,
            reason: 0,
            error: 0,
            reg: std::ptr::null(),
        };
        // SAFETY: уникальный owner-thread, неподвижный callback/namespace;
        // buffer содержит cap доступных для записи байт и не сохраняется C.
        let code = unsafe {
            xipc_sys::xipc_dispatch_server_next(
                self.ptr.as_ptr(),
                buffer.as_mut_ptr().cast(),
                cap,
                ms,
                &raw mut raw,
            )
        };
        self.callback_result()?;
        check(code)?;
        match raw.kind {
            0 => Ok(DispatchEvent::Timeout),
            4 => Ok(DispatchEvent::Wake),
            5 => Ok(DispatchEvent::Fault {
                client_id: (raw.client_id != 0).then_some(raw.client_id),
                error: check(raw.error).err().ok_or(Error::InvalidCoreResult)?,
            }),
            1..=3 => {
                if raw.client_id == 0 || raw.reg.is_null() {
                    return Err(Error::InvalidCoreResult);
                }
                if raw.kind == 1 {
                    if self.active.contains_key(&raw.client_id) {
                        return Err(Error::InvalidCoreResult);
                    }
                    self.generation = self
                        .generation
                        .checked_add(1)
                        .ok_or(Error::InvalidCoreResult)?;
                    // SAFETY: C гарантирует reg до следующего next/stop;
                    // копируем один раз на сессию, не выделяем String на каждый кадр.
                    let registration = Arc::new(Registration::copy_from(unsafe { &*raw.reg })?);
                    self.active.insert(
                        raw.client_id,
                        ActiveSession {
                            generation: self.generation,
                            registration,
                        },
                    );
                }
                let active = self
                    .active
                    .get(&raw.client_id)
                    .ok_or(Error::InvalidCoreResult)?;
                let registration = active.registration.clone();
                let peer = DispatchPeer {
                    owner: self.wake.clone(),
                    id: raw.client_id,
                    generation: active.generation,
                };
                match raw.kind {
                    1 => Ok(DispatchEvent::Connected { peer, registration }),
                    2 => Ok(DispatchEvent::Message {
                        peer,
                        registration,
                        data: buffer
                            .get(..raw.len as usize)
                            .ok_or(Error::InvalidCoreResult)?,
                    }),
                    _ => {
                        self.active.remove(&raw.client_id);
                        Ok(DispatchEvent::Disconnected {
                            peer,
                            registration,
                            reason: raw.reason.into(),
                            error: check(raw.error).err(),
                        })
                    }
                }
            }
            _ => Err(Error::InvalidCoreResult),
        }
    }
    fn validate_peer(&self, peer: &DispatchPeer) -> Result<u32> {
        if Arc::ptr_eq(&self.wake, &peer.owner)
            && self
                .active
                .get(&peer.id)
                .is_some_and(|active| active.generation == peer.generation)
        {
            Ok(peer.id)
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
    /// Отправить конкретной сессии без вытеснения предыдущих сообщений.
    pub fn try_send(&mut self, peer: &DispatchPeer, data: &[u8]) -> Result<()> {
        let id = self.validate_peer(peer)?;
        let len = u32::try_from(data.len()).map_err(|_| Error::LengthTooLarge)?;
        // SAFETY: уникальный owner-thread, проверенная сессия и живой срез.
        Self::status(unsafe {
            xipc_sys::xipc_dispatch_try_send(self.ptr.as_ptr(), id, data.as_ptr().cast(), len)
        })
    }
    /// Явно разрешить вытеснение старых сообщений; C не возвращает счётчик.
    pub fn send_overwriting(&mut self, peer: &DispatchPeer, data: &[u8]) -> Result<()> {
        let id = self.validate_peer(peer)?;
        let len = u32::try_from(data.len()).map_err(|_| Error::LengthTooLarge)?;
        // SAFETY: те же гарантии сессии и времени жизни, что у try_send.
        Self::status(unsafe {
            xipc_sys::xipc_dispatch_send(self.ptr.as_ptr(), id, data.as_ptr().cast(), len)
        })
    }
    /// Закрыть канал; Disconnected поступит через next_event.
    pub fn disconnect(&mut self, peer: &DispatchPeer) -> Result<()> {
        let id = self.validate_peer(peer)?;
        // SAFETY: уникальный owner-thread и проверенный идентификатор сессии.
        Self::status(unsafe { xipc_sys::xipc_dispatch_disconnect_client(self.ptr.as_ptr(), id) })
    }
}
impl Drop for DispatchServer {
    fn drop(&mut self) {
        let mut guard = self
            .wake
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = None;
        // SAFETY: владелец закрывает C до namespace/callback. Mutex исключает
        // незавершённые wake; &mut self исключает пересечение с next и callback.
        unsafe { xipc_sys::xipc_dispatch_server_free(self.ptr.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registration_uses_explicit_length_and_preserves_embedded_nul() {
        let mut raw = xipc_sys::Registration {
            pid: 42,
            revision: 7,
            name_len: 3,
            name: [0; 65],
        };
        raw.name[..3].copy_from_slice(b"a\0b");
        assert_eq!(Registration::copy_from(&raw).unwrap().name, "a\0b");
        raw.name_len = 65;
        assert!(matches!(
            Registration::copy_from(&raw),
            Err(Error::InvalidCoreResult)
        ));
        raw.name_len = 1;
        raw.name[0] = 0xff;
        assert!(matches!(
            Registration::copy_from(&raw),
            Err(Error::InvalidCoreResult)
        ));
    }
}
