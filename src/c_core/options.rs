//! Конфигурация — только значения и преобразование ABI, не проверка протокола.
use super::{Error, PrivateNamespace, Result};
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

impl Security {
    pub(super) const fn raw(self) -> xipc_sys::Security {
        xipc_sys::Security {
            kind: match self {
                Self::CurrentUser => 0,
                Self::CurrentUserAllowLow => 1,
                Self::Everyone => 2,
            },
            descriptor: std::ptr::null(),
            descriptor_len: 0,
        }
    }
}

/// Конфигурация сервера; удерживает пространство имён через Arc.
#[derive(Debug, Clone)]
pub struct ChannelConfig {
    /// Параметры колец; проверяются C-ядром.
    pub layout: Layout,
    /// Политика доступа к создаваемым объектам.
    pub security: Security,
    /// Пространство имён удерживается каналом до закрытия всех его объектов.
    pub namespace: Option<PrivateNamespace>,
}
impl Default for ChannelConfig {
    fn default() -> Self {
        // SAFETY: функция не имеет предусловий; возвращает структуру по значению.
        let config = unsafe { xipc_sys::xipc_channel_config_default() };
        Self {
            layout: config.layout,
            security: Security::default(),
            namespace: None,
        }
    }
}
impl ChannelConfig {
    pub(super) fn raw(&self) -> xipc_sys::ChannelConfig {
        xipc_sys::ChannelConfig {
            layout: self.layout,
            security: self.security.raw(),
            ns: self
                .namespace
                .as_ref()
                .map_or(std::ptr::null(), PrivateNamespace::raw),
        }
    }
}

/// Общий бюджет подключения Multi. Одна попытка может ждать рукопожатия;
/// поэтому она намеренно не называется Immediate.
#[derive(Debug, Clone, Copy)]
pub enum ConnectBudget {
    /// Один обход слотов с отдельным сроком рукопожатия.
    Attempt,
    /// Общий срок поиска и рукопожатия. Нулевой срок сразу даёт Timeout.
    For(Duration),
    /// Повторять до подключения или отмены.
    Infinite,
}
impl ConnectBudget {
    pub(super) fn milliseconds(self) -> Result<u32> {
        match self {
            Self::Attempt => Ok(0),
            Self::Infinite => Ok(u32::MAX),
            Self::For(duration) if duration.is_zero() => Err(Error::Timeout),
            Self::For(duration) => Wait::For(duration).milliseconds(),
        }
    }
}

/// Параметры сервера с независимыми слотами.
#[derive(Debug, Clone)]
pub struct MultiOptions {
    /// Раскладка, безопасность и удерживаемое пространство имён каждого слота.
    pub channel: ChannelConfig,
    /// Число сообщений слота за один круг; 0 означает 1 по контракту C.
    pub receive_batch: u32,
}
impl Default for MultiOptions {
    fn default() -> Self {
        // SAFETY: функция без предусловий, возвращает значения по умолчанию.
        let raw = unsafe { xipc_sys::xipc_multi_options_default() };
        Self {
            channel: ChannelConfig {
                layout: raw.layout,
                ..ChannelConfig::default()
            },
            receive_batch: raw.recv_batch,
        }
    }
}

/// Настройка поиска свободного слота; все ожидания выполняются C-ядром.
#[derive(Debug, Clone)]
pub struct MultiClientOptions {
    /// Срок одного рукопожатия; C ограничивает его максимумом 8000 мс.
    pub handshake_timeout: Duration,
    /// Задержка при недоступных уведомлениях или неудачном рукопожатии.
    pub retry_delay: Duration,
    /// Страховочное ожидание обнаружения; положительный срок либо None
    /// для ожидания только событий, без периодического пробуждения.
    pub discovery_fallback: Option<Duration>,
    /// Удерживается соединением до его закрытия.
    pub namespace: Option<PrivateNamespace>,
    /// Политика впервые создаваемых объектов обнаружения.
    pub security: Security,
}
impl Default for MultiClientOptions {
    fn default() -> Self {
        // SAFETY: функция без предусловий, возвращает значения по умолчанию.
        let raw = unsafe { xipc_sys::xipc_multi_client_options_default() };
        Self {
            handshake_timeout: Duration::from_millis(u64::from(raw.slot_timeout_ms)),
            retry_delay: Duration::from_millis(u64::from(raw.retry_delay_ms)),
            discovery_fallback: Some(Duration::from_millis(u64::from(raw.discovery_fallback_ms))),
            namespace: None,
            security: Security::default(),
        }
    }
}

/// Параметры лобби и выделенных каналов Dispatch.
#[derive(Debug, Clone)]
pub struct DispatchOptions {
    /// Максимум одновременно допущенных клиентов, включая ожидающих канал.
    /// Сервер выделяет столько же слотов; диапазон 1..=256.
    pub max_clients: u32,
    /// Срок получения запроса после рукопожатия лобби.
    pub lobby_timeout: Duration,
    /// Срок подключения клиента к выданному каналу.
    pub channel_connect_timeout: Duration,
    /// Раскладка канала регистрации.
    pub lobby_layout: Layout,
    /// Параметры выделенных каналов, безопасность всех объектов и namespace.
    pub channel: ChannelConfig,
}
impl Default for DispatchOptions {
    fn default() -> Self {
        // SAFETY: функция без предусловий, возвращает параметры по значению.
        let raw = unsafe { xipc_sys::xipc_dispatch_options_default() };
        Self {
            max_clients: raw.max_clients,
            lobby_timeout: Duration::from_millis(u64::from(raw.lobby_timeout_ms)),
            channel_connect_timeout: Duration::from_millis(u64::from(
                raw.channel_connect_timeout_ms,
            )),
            lobby_layout: raw.lobby_layout,
            channel: ChannelConfig {
                layout: raw.channel_layout,
                ..ChannelConfig::default()
            },
        }
    }
}

/// Отдельные сроки этапов регистрации Dispatch; каждый этап прерывается отменой.
#[derive(Debug, Clone)]
pub struct DispatchClientOptions {
    /// Ожидание мьютекса и рукопожатие лобби, каждое с этим сроком.
    pub lobby_timeout: Wait,
    /// Ожидание ответа на регистрацию.
    pub response_timeout: Wait,
    /// Рукопожатие выделенного канала.
    pub channel_timeout: Wait,
    /// Политика мьютекса, если клиент создаёт его первым.
    pub security: Security,
    /// Пространство имён сохраняется до закрытия клиентского канала.
    pub namespace: Option<PrivateNamespace>,
}
impl Default for DispatchClientOptions {
    fn default() -> Self {
        // SAFETY: функция без предусловий, возвращает параметры по значению.
        let raw = unsafe { xipc_sys::xipc_dispatch_client_options_default() };
        Self {
            lobby_timeout: Wait::For(Duration::from_millis(u64::from(raw.lobby_timeout_ms))),
            response_timeout: Wait::For(Duration::from_millis(u64::from(raw.response_timeout_ms))),
            channel_timeout: Wait::For(Duration::from_millis(u64::from(raw.channel_timeout_ms))),
            security: Security::default(),
            namespace: None,
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
