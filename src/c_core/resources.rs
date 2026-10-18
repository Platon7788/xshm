//! Совместное владение непрозрачными объектами C без копирования их раскладки.
use super::error::check;
use super::{CancellationToken, Error, Result, Security, Wait};
use std::ffi::{CString, c_void};
use std::ptr::NonNull;
use std::sync::Arc;

#[derive(Debug)]
struct NamespaceInner(NonNull<xipc_sys::Namespace>);
// SAFETY: после open пространство имён неизменно; NT HANDLE переносим между
// потоками. Последний Arc закрывается лишь после всех заимствующих объектов.
unsafe impl Send for NamespaceInner {}
// SAFETY: из разделяемой ссылки доступно только чтение стабильного C-объекта.
unsafe impl Sync for NamespaceInner {}
impl Drop for NamespaceInner {
    fn drop(&mut self) {
        // SAFETY: последний владелец C-аллокации, заимствующих объектов уже нет.
        unsafe { xipc_sys::xipc_namespace_free(self.0.as_ptr()) };
    }
}

/// Пространство имён ядра. Клоны и каналы сохраняют его дескриптор живым.
#[derive(Debug, Clone)]
pub struct PrivateNamespace(Arc<NamespaceInner>);
impl PrivateNamespace {
    /// Открыть либо создать границу текущего пользователя. Политика доступа
    /// объектов внутри задаётся отдельно в ChannelConfig/Beacon::open_with.
    pub fn open(boundary: &str, security: Security) -> Result<Self> {
        let boundary = CString::new(boundary)?;
        let raw_security = security.raw()?;
        // SAFETY: C-аллокатор создаёт нулевой объект и не имеет предусловий.
        let ptr =
            NonNull::new(unsafe { xipc_sys::xipc_namespace_alloc() }).ok_or(Error::Allocation)?;
        let inner = NamespaceInner(ptr);
        // SAFETY: уникальный нулевой объект; строки/политика живут до возврата.
        // Alias диагностический и не участвует в идентичности NT-границы.
        let code = unsafe {
            xipc_sys::xipc_namespace_open(
                ptr.as_ptr(),
                boundary.as_ptr(),
                c"xshm".as_ptr(),
                &raw const raw_security,
            )
        };
        check(code)?;
        Ok(Self(Arc::new(inner)))
    }
    pub(super) fn raw(&self) -> *const xipc_sys::Namespace {
        self.0.0.as_ptr()
    }
}

#[derive(Debug)]
struct BeaconInner {
    ptr: NonNull<xipc_sys::Beacon>,
    // C не дублирует namespace: сохраняем его до завершения C free.
    namespace: Option<PrivateNamespace>,
}
// SAFETY: после open C разрешает concurrent raise/lower/clear/wait.
// Drop закрывает объект только после последнего Arc и завершения вызовов.
unsafe impl Send for BeaconInner {}
// SAFETY: C использует NT-объекты и атомарные shared-поля, Rust не создаёт
// ссылок на их память; open/close не пересекаются с вызовами через Arc.
unsafe impl Sync for BeaconInner {}
impl BeaconInner {
    fn new(namespace: Option<PrivateNamespace>) -> Result<Self> {
        // SAFETY: C-аллокатор без предусловий; результат принадлежит этому owner.
        let ptr =
            NonNull::new(unsafe { xipc_sys::xipc_beacon_alloc() }).ok_or(Error::Allocation)?;
        Ok(Self { ptr, namespace })
    }
}
impl Drop for BeaconInner {
    fn drop(&mut self) {
        // SAFETY: последний владелец, все вызовы/ожидания завершились.
        // Поле namespace будет освобождено только после этого вызова.
        unsafe { xipc_sys::xipc_beacon_free(self.ptr.as_ptr()) };
    }
}

/// Разделяемый маяк. Закрывается после последнего клона и всех ожиданий.
/// Методы raise/lower предназначены владельцу публикации; наблюдатель
/// использует wait/is_raised/clear_if_stale и не гасит живой сервер.
#[derive(Debug, Clone)]
pub struct Beacon(Arc<BeaconInner>);
impl Beacon {
    /// Открыть именованный маяк с политикой текущего пользователя.
    pub fn open(name: &str) -> Result<Self> {
        Self::open_with(name, Security::default(), None)
    }
    /// Открыть маяк с заданной политикой; пространство имён удерживается объектом.
    pub fn open_with(
        name: &str,
        security: Security,
        namespace: Option<PrivateNamespace>,
    ) -> Result<Self> {
        let name = CString::new(name)?;
        let raw_security = security.raw()?;
        let inner = BeaconInner::new(namespace)?;
        let ns = inner
            .namespace
            .as_ref()
            .map_or(std::ptr::null(), PrivateNamespace::raw);
        // SAFETY: уникальная C-аллокация; строки и политика живут до возврата,
        // namespace удерживается inner до закрытия маяка.
        let code = unsafe {
            xipc_sys::xipc_beacon_open(
                inner.ptr.as_ptr(),
                name.as_ptr(),
                &raw const raw_security,
                ns,
            )
        };
        check(code)?;
        Ok(Self(Arc::new(inner)))
    }
    /// Создать безымянное manual-reset событие через C-ядро.
    pub fn unnamed() -> Result<Self> {
        let inner = BeaconInner::new(None)?;
        // SAFETY: уникальный нулевой C-объект; ошибка очищает его.
        check(unsafe { xipc_sys::xipc_beacon_unnamed(inner.ptr.as_ptr()) })?;
        Ok(Self(Arc::new(inner)))
    }
    /// Опубликовать присутствие. Разрешено владельцу соответствующего сервера.
    pub fn raise(&self) -> Result<()> {
        // SAFETY: Arc удерживает объект; C разрешает эту конкурентную операцию.
        check(unsafe { xipc_sys::xipc_beacon_raise(self.0.ptr.as_ptr()) })
    }
    /// Снять собственную публикацию; наблюдателю нужен clear_if_stale.
    pub fn lower(&self) -> Result<()> {
        // SAFETY: Arc удерживает объект; C разрешает эту конкурентную операцию.
        check(unsafe { xipc_sys::xipc_beacon_lower(self.0.ptr.as_ptr()) })
    }
    /// Убрать устаревшую публикацию по правилам C-ядра. В режиме stateless
    /// ядро может только сбросить событие; это ограничение C-протокола.
    pub fn clear_if_stale(&self) -> Result<bool> {
        let mut cleared = false;
        // SAFETY: Arc держит объект; cleared локален и доступен для записи.
        check(unsafe {
            xipc_sys::xipc_beacon_clear_if_stale(self.0.ptr.as_ptr(), &raw mut cleared)
        })?;
        Ok(cleared)
    }
    /// Проверить состояние, сохраняя ошибку NT-ожидания вместо ложного false.
    pub fn is_raised(&self) -> Result<bool> {
        // SAFETY: Arc удерживает событие; timeout=0 не блокирует и не сбрасывает его.
        match unsafe { xipc_sys::xipc_beacon_wait(self.0.ptr.as_ptr(), 0) } {
            0 => Ok(true),
            4 => Ok(false),
            code => {
                check(code)?;
                Err(Error::InvalidCoreResult)
            }
        }
    }
    /// Дождаться маяка или отмены. Поднятый маяк выигрывает одновременный сигнал.
    pub fn wait(&self, wait: Wait, cancel: Option<&CancellationToken>) -> Result<()> {
        let ms = wait.milliseconds()?;
        let handles = [self.handle(), CancellationToken::handle(cancel)];
        let count = if cancel.is_some() { 2 } else { 1 };
        // SAFETY: оба Arc заимствованы до возврата, массив содержит count
        // валидных HANDLE; C не сохраняет указатель массива.
        match unsafe { xipc_sys::xipc_wait_any(handles.as_ptr(), count, ms) } {
            0 => Ok(()),
            1 if cancel.is_some() => Err(Error::Cancelled),
            -1 => Err(Error::Timeout),
            -2 => Err(Error::KernelCall),
            _ => Err(Error::InvalidCoreResult),
        }
    }
    pub(super) fn handle(&self) -> *mut c_void {
        // SAFETY: Arc удерживает открытый маяк; возвращаемый HANDLE заимствован.
        unsafe { xipc_sys::xipc_beacon_handle(self.0.ptr.as_ptr()) }
    }
}

/// Проверить существование лобби, не подключаясь к нему. Как и в C API,
/// отсутствие доступа или ошибка открытия дают false; это подсказка для
/// обнаружения, а не проверка аутентичности или гарантированное подключение.
pub fn lobby_exists(name: &str, namespace: Option<&PrivateNamespace>) -> Result<bool> {
    let name = CString::new(name)?;
    let ns = namespace.map_or(std::ptr::null(), PrivateNamespace::raw);
    // SAFETY: строка и заимствованный namespace живы на всё время C-вызова.
    Ok(unsafe { xipc_sys::xipc_lobby_exists(name.as_ptr(), ns) })
}
