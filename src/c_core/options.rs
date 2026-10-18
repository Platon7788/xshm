//! Конфигурация — только значения и преобразование ABI, не проверка протокола.
use super::{Error, Result};
use std::time::Duration;

/// Явная политика ожидания. Конечное значение никогда не становится Infinite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// Однократная проверка без ожидания события.
    Immediate,
    /// Конечный срок с округлением вверх до миллисекунды.
    For(Duration),
    /// Ожидание без срока, прерываемое токеном отмены.
    Infinite,
}
impl Wait {
    pub(super) fn milliseconds(self) -> Result<u32> {
        match self {
            Self::Immediate => Ok(0),
            Self::Infinite => Ok(u32::MAX),
            Self::For(duration) => {
                let ms = duration.as_nanos().div_ceil(1_000_000);
                let ms = u32::try_from(ms).map_err(|_| Error::TimeoutTooLarge)?;
                if ms == u32::MAX {
                    return Err(Error::TimeoutTooLarge);
                }
                Ok(ms)
            }
        }
    }
}

/// Ограничения одного направления кольца.
pub type RingConfig = xipc_sys::RingConfig;
/// Ограничения обоих направлений канала.
pub type Layout = xipc_sys::Layout;

/// Политика доступа к создаваемым объектам.
#[derive(Debug, Clone, Copy, Default)]
pub enum Security {
    /// Текущий пользователь и SYSTEM.
    #[default]
    CurrentUser,
    /// Текущий пользователь и SYSTEM, с разрешением Low Integrity.
    CurrentUserAllowLow,
    /// NULL DACL: намеренно разрешить доступ всем пользователям.
    Everyone,
}

/// Конфигурация сервера. Указателей и заимствованной памяти не содержит.
#[derive(Debug, Clone, Copy)]
pub struct ChannelConfig {
    /// Параметры колец; проверяются C-ядром.
    pub layout: Layout,
    /// Политика доступа к создаваемым объектам.
    pub security: Security,
}
impl Default for ChannelConfig {
    fn default() -> Self {
        // SAFETY: функция не имеет предусловий; возвращает структуру по значению.
        let config = unsafe { xipc_sys::xipc_channel_config_default() };
        Self {
            layout: config.layout,
            security: Security::default(),
        }
    }
}
impl ChannelConfig {
    pub(super) const fn raw(self) -> xipc_sys::ChannelConfig {
        xipc_sys::ChannelConfig {
            layout: self.layout,
            security: xipc_sys::Security {
                kind: match self.security {
                    Security::CurrentUser => 0,
                    Security::CurrentUserAllowLow => 1,
                    Security::Everyone => 2,
                },
                descriptor: std::ptr::null(),
                descriptor_len: 0,
            },
            ns: std::ptr::null(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finite_timeouts_do_not_become_infinite_or_zero_by_rounding() {
        assert_eq!(
            Wait::For(Duration::from_nanos(1)).milliseconds().unwrap(),
            1
        );
        assert_eq!(Wait::For(Duration::ZERO).milliseconds().unwrap(), 0);
        assert_eq!(
            Wait::For(Duration::from_millis(u64::from(u32::MAX - 1)))
                .milliseconds()
                .unwrap(),
            u32::MAX - 1
        );
        assert!(matches!(
            Wait::For(Duration::from_millis(u64::from(u32::MAX))).milliseconds(),
            Err(Error::TimeoutTooLarge)
        ));
        assert!(matches!(
            Wait::For(Duration::MAX).milliseconds(),
            Err(Error::TimeoutTooLarge)
        ));
        assert_eq!(Wait::Infinite.milliseconds().unwrap(), u32::MAX);
    }
}
