//! Ожидания принадлежащих Rust HANDLE через C-ядро, без нового NT-слоя.
use super::error::check;
use super::{Beacon, CancellationToken, Error, Result, Wait};
use std::ffi::c_void;
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle};
use std::ptr::NonNull;
use std::sync::Arc;

/// Дождаться любого объекта. При нескольких сигналах выигрывает меньший
/// индекс. Не более 64 объектов; пустой список возвращает Timeout.
pub fn wait_any(objects: &[&dyn AsHandle], wait: Wait) -> Result<usize> {
    let timeout = wait.milliseconds()?;
    if objects.len() > 64 {
        return Err(Error::InvalidArgument);
    }
    let mut handles = [std::ptr::null_mut(); 64];
    for (handle, object) in handles.iter_mut().zip(objects) {
        *handle = object.as_handle().as_raw_handle();
    }
    // SAFETY: каждый объект заимствован на весь вызов, AsHandle обещает живой
    // HANDLE. C не сохраняет массив и читает только проверенное число элементов.
    let code = unsafe { xipc_sys::xipc_wait_any(handles.as_ptr(), objects.len() as u32, timeout) };
    match code {
        -1 => Err(Error::Timeout),
        -2 => Err(Error::KernelCall),
        code => usize::try_from(code)
            .ok()
            .filter(|&index| index < objects.len())
            .ok_or(Error::InvalidCoreResult),
    }
}

impl AsHandle for Beacon {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        // SAFETY: открытый Beacon держит HANDLE через Arc и не допускает close
        // до Drop последнего владельца; заимствование ограничено &self.
        unsafe { BorrowedHandle::borrow_raw(self.handle()) }
    }
}
impl AsHandle for CancellationToken {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        // SAFETY: токен держит открытый Beacon через Arc до конца &self.
        unsafe { BorrowedHandle::borrow_raw(Self::handle(Some(self))) }
    }
}

#[derive(Debug)]
struct OwnedProcess(NonNull<c_void>);
// SAFETY: NT process HANDLE переносим; единственная изменяющая операция —
// Close после последнего Arc, когда ни одно заимствованное ожидание не живёт.
unsafe impl Send for OwnedProcess {}
// SAFETY: конкурентные ожидания на process HANDLE разрешены NT; закрытие
// невозможно до окончания заимствований владельцев Arc.
unsafe impl Sync for OwnedProcess {}
impl Drop for OwnedProcess {
    fn drop(&mut self) {
        // SAFETY: последний владелец HANDLE, полученного C open; ожиданий нет.
        unsafe { xipc_sys::xipc_process_exit_close(self.0.as_ptr()) };
    }
}
/// Сигнал выхода конкретного объекта процесса. Удерживаемый HANDLE не меняет
/// идентичность при последующем повторном использовании его PID системой.
#[derive(Debug, Clone)]
pub struct ProcessExit(Arc<OwnedProcess>);
impl ProcessExit {
    /// Открыть процесс для ожидания завершения, с проверкой доступа в C.
    pub fn open(pid: u32) -> Result<Self> {
        let mut handle = std::ptr::null_mut();
        // SAFETY: выходной указатель доступен для записи на всё время вызова.
        check(unsafe { xipc_sys::xipc_process_exit_open(pid, &raw mut handle) })?;
        let handle = NonNull::new(handle).ok_or(Error::InvalidCoreResult)?;
        Ok(Self(Arc::new(OwnedProcess(handle))))
    }
    /// Дождаться выхода процесса, не удерживая никаких пользовательских locks.
    pub fn wait(&self, wait: Wait) -> Result<()> {
        wait_any(&[self], wait).map(|_| ())
    }
}
impl AsHandle for ProcessExit {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        // SAFETY: Arc удерживает process HANDLE весь срок заимствования &self.
        unsafe { BorrowedHandle::borrow_raw(self.0.0.as_ptr()) }
    }
}
