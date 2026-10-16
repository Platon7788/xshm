//! NT API модуль - прямые вызовы ntdll.dll
//!
//! Минимальная реализация без внешних зависимостей.
//! Импорт ntdll.dll через `#[link(name = "ntdll", kind = "raw-dylib")]`.
//!
//! # Особенности
//!
//! - Никакого TLS
//! - Никакого GetProcAddress в runtime
//! - Линкер резолвит адреса при загрузке модуля
//! - Минимальный overhead

pub mod funcs;
pub mod helpers;
pub mod types;

// Реэкспорт для удобства
pub use funcs::*;
pub use helpers::*;
pub use types::*;

#[cfg(test)]
mod tests {
    use super::*;
    use core::ptr;

    /// Новые импорты ntdll резолвятся (raw-dylib) и работают на текущей
    /// разрядности: IOCP, пакет ожидания завершения и время создания процесса.
    #[test]
    fn new_imports_resolve() {
        let mut iocp: HANDLE = ptr::null_mut();
        // SAFETY: выходной указатель -- локальная переменная; атрибуты не нужны
        // (безымянный объект), Count = 1.
        let status = unsafe {
            NtCreateIoCompletion(&raw mut iocp, IO_COMPLETION_ALL_ACCESS, ptr::null_mut(), 1)
        };
        assert_eq!(status, STATUS_SUCCESS);
        assert!(!iocp.is_null());

        let mut packet: HANDLE = ptr::null_mut();
        // SAFETY: выходной указатель -- локальная переменная, атрибутов нет.
        let status =
            unsafe { NtCreateWaitCompletionPacket(&raw mut packet, GENERIC_ALL, ptr::null_mut()) };
        assert_eq!(status, STATUS_SUCCESS);
        assert!(!packet.is_null());

        // SAFETY: handle получен выше и закрывается ровно один раз.
        assert_eq!(unsafe { NtClose(packet) }, STATUS_SUCCESS);
        // SAFETY: то же.
        assert_eq!(unsafe { NtClose(iocp) }, STATUS_SUCCESS);

        let mut times = KERNEL_USER_TIMES::default();
        let mut returned: ULONG = 0;
        // SAFETY: буфер -- локальная структура ровно того размера, что передан;
        // псевдо-handle текущего процесса не требует закрытия.
        let status = unsafe {
            NtQueryInformationProcess(
                NT_CURRENT_PROCESS,
                ProcessTimes,
                ptr::from_mut(&mut times).cast(),
                size_of::<KERNEL_USER_TIMES>() as ULONG,
                &raw mut returned,
            )
        };
        assert_eq!(status, STATUS_SUCCESS);
        assert_eq!(returned as usize, size_of::<KERNEL_USER_TIMES>());
        // SAFETY: LARGE_INTEGER -- объединение одинаковых по размеру i64 и пары u32/i32.
        let create_time = unsafe { times.CreateTime.QuadPart };
        assert_ne!(create_time, 0);
    }

    /// Раскладка новых NT-структур совпадает с phnt на обеих разрядностях.
    #[test]
    fn new_structs_match_phnt_layout() {
        let ptr = size_of::<usize>();
        // BaseAddress, AllocationAttributes (+ выравнивание на x64), MaximumSize.
        assert_eq!(
            size_of::<SECTION_BASIC_INFORMATION>(),
            if ptr == 8 { 24 } else { 16 }
        );
        assert_eq!(
            core::mem::offset_of!(SECTION_BASIC_INFORMATION, MaximumSize),
            if ptr == 8 { 16 } else { 8 }
        );
        assert_eq!(size_of::<KERNEL_USER_TIMES>(), 32);
        assert_eq!(size_of::<IO_STATUS_BLOCK>(), 2 * ptr);
        assert_eq!(size_of::<FILE_IO_COMPLETION_INFORMATION>(), 4 * ptr);
        assert_eq!(size_of::<SID_AND_ATTRIBUTES>(), 2 * ptr);
        assert_eq!(size_of::<TOKEN_USER>(), 2 * ptr);
        assert_eq!(size_of::<ACL>(), 8);
        assert_eq!(SectionBasicInformation, 0);
        assert_eq!(
            [SEC_RESERVE, SEC_IMAGE, SEC_FILE],
            [0x0400_0000, 0x0100_0000, 0x0080_0000]
        );
    }
}
