//! Частное пространство имён (0.10).
//!
//! Именованные объекты xshm по умолчанию живут в общем каталоге
//! `\BaseNamedObjects` (или его сессионной копии): имя канала видит и может
//! занять заранее любой процесс машины. [`PrivateNamespace`] -- каталог
//! объектов ядра, который находится не по имени, а по **дескриптору
//! границы**: имя границы + SID пользователя процесса (+ метка целостности
//! Low для [`ObjectSecurity::CurrentUserAllowLowIntegrity`]). Открыть или
//! создать такое пространство может только процесс, в токене которого есть
//! все SID границы, -- то есть процесс того же пользователя; процесс другого
//! пользователя не может ни найти канал, ни создать пространство с чужим
//! SID в границе, ни занять имя заранее.
//!
//! Объекты внутри пространства создаются и открываются по **относительному**
//! имени: `OBJECT_ATTRIBUTES { RootDirectory = handle пространства,
//! ObjectName = имя }`. Префиксы `Local\`/`Global\` и NT-пути внутри
//! пространства бессмысленны и отвергаются (`InvalidConfig`), как и любой
//! `\` в имени.
//!
//! Время жизни: пространство существует, пока открыт хоть один его handle
//! (в любом процессе), и исчезает с последним. `Drop` только закрывает
//! handle -- `NtDeletePrivateNamespace` не вызывается: удалённое создателем
//! пространство осталось бы у пиров «висящим» каталогом (их handle продолжает
//! указывать на старый каталог), а перезапущенный сервер создал бы новое с
//! той же границей -- переподключающийся клиент искал бы его не там.

use core::ptr::null_mut;
use std::sync::Arc;

use crate::error::{Result, ShmError};
use crate::ntapi::{
    DIRECTORY_ALL_ACCESS, HANDLE, NtClose, NtCreatePrivateNamespace, NtName,
    NtOpenPrivateNamespace, OBJECT_ATTRIBUTES, POBJECT_BOUNDARY_DESCRIPTOR, PSID,
    RtlAddIntegrityLabelToBoundaryDescriptor, RtlAddSIDToBoundaryDescriptor,
    RtlCreateBoundaryDescriptor, RtlDeleteBoundaryDescriptor, STATUS_OBJECT_NAME_COLLISION,
    STATUS_OBJECT_NAME_NOT_FOUND, STATUS_SUCCESS,
};
use crate::security::{LOW_LABEL_SID, ObjectSecurity, current_user_sid};
use crate::win::status_to_error;

/// Сколько раз повторить пару «создать -> открыть», если пространство
/// исчезло между ними (последний держатель закрыл handle).
const OPEN_OR_CREATE_ATTEMPTS: usize = 8;

/// `NTSTATUS` `STATUS_OBJECT_PATH_NOT_FOUND`: так `NtOpenPrivateNamespace`
/// сообщает, что пространства с такой границей нет.
const STATUS_OBJECT_PATH_NOT_FOUND: i32 = 0xC000_003A_u32.cast_signed();

/// Частное пространство имён (см. модуль): handle каталога объектов,
/// найденного по дескриптору границы.
///
/// Делится через `Arc`: канал, лобби Dispatch, слоты Multi и маяк держат
/// свою ссылку, пока живут их объекты, -- иначе пространство исчезло бы
/// вместе с последним handle, и пир не нашёл бы в нём объекты.
///
/// Обе стороны канала вызывают [`PrivateNamespace::open_or_create`] с одним
/// и тем же именем границы **и** одной политикой: граница
/// [`ObjectSecurity::CurrentUserAllowLowIntegrity`] содержит ещё и метку Low,
/// это другой дескриптор границы -- другое пространство.
///
/// # Время жизни
///
/// `Drop` закрывает только свой handle; `NtDeletePrivateNamespace` не
/// вызывается (и не объявлен в `ntapi/funcs.rs`): удаление создателем
/// оставило бы пиров с «висящим» каталогом, а перезапущенный сервер создал
/// бы другое пространство с той же границей. Пространство исчезает с
/// последним handle в любом процессе. Закреплено тестом
/// `creator_drop_keeps_namespace_for_peers` (`tests/namespace.rs`).
///
/// Async-подключение в пространстве --
/// [`SharedClient::connect_async_in`](crate::SharedClient::connect_async_in)
/// (`Arc` по значению: future `'static` и держит пространство до
/// готовности).
///
/// ```no_run
/// use std::sync::Arc;
/// use std::time::Duration;
/// use xshm::{ChannelConfig, ObjectSecurity, PrivateNamespace, SharedClient, SharedServer};
///
/// let ns = PrivateNamespace::open_or_create("MyApp", "myapp", &ObjectSecurity::default())?;
/// let config = ChannelConfig::default().namespace(Arc::clone(&ns));
/// let server = SharedServer::start_with("Chan", &config)?; // без `Local\`
///
/// // Другой процесс того же пользователя:
/// let ns = PrivateNamespace::open_or_create("MyApp", "myapp", &ObjectSecurity::default())?;
/// let client = SharedClient::connect_in(&ns, "Chan", Duration::from_secs(2))?;
/// # Ok::<(), xshm::ShmError>(())
/// ```
pub struct PrivateNamespace {
    handle: HANDLE,
    boundary: String,
    alias: String,
    low_integrity: bool,
    created: bool,
}

// SAFETY: внутри -- NT-дескриптор каталога (машинное слово) и неизменяемые
// строки; ядро разрешает пользоваться дескриптором из любого потока.
unsafe impl Send for PrivateNamespace {}
// SAFETY: см. выше -- изменяемого состояния на стороне Rust нет.
unsafe impl Sync for PrivateNamespace {}

/// Дескриптор границы, освобождается в `Drop`.
struct Boundary(POBJECT_BOUNDARY_DESCRIPTOR);

impl Drop for Boundary {
    fn drop(&mut self) {
        // SAFETY: дескриптор получен `RtlCreateBoundaryDescriptor` (и, быть
        // может, перевыделен `RtlAdd*`, которые обновили `self.0`), владеет
        // им только `Boundary`; освобождается ровно один раз.
        unsafe { RtlDeleteBoundaryDescriptor(self.0) };
    }
}

impl Boundary {
    /// Граница `name` с SID пользователя процесса и, если `low_integrity`,
    /// меткой целостности Low.
    fn new(name: &str, low_integrity: bool) -> Result<Self> {
        let mut nt_name = NtName::verbatim(name)?;
        // SAFETY: `nt_name` -- корректная UNICODE_STRING, живая весь вызов;
        // функция копирует имя в новый дескриптор.
        let raw = unsafe { RtlCreateBoundaryDescriptor(nt_name.as_ptr(), 0) };
        if raw.is_null() {
            return Err(ShmError::WindowsError {
                code: 0xC000_0017, // STATUS_NO_MEMORY
                context: "RtlCreateBoundaryDescriptor",
            });
        }
        let mut boundary = Self(raw);

        let sid = current_user_sid()?;
        // Rtl-функция принимает `PSID` без `const`, но SID только копирует.
        let sid_ptr: PSID = sid.as_ptr().cast_mut().cast();
        // SAFETY: `boundary.0` -- живой дескриптор границы; функция может
        // перевыделить его и записывает новый адрес по указателю; SID --
        // корректный SID в живом `Vec`.
        let status = unsafe { RtlAddSIDToBoundaryDescriptor(&raw mut boundary.0, sid_ptr) };
        check(status, "RtlAddSIDToBoundaryDescriptor")?;

        if low_integrity {
            let label_ptr: PSID = LOW_LABEL_SID.as_ptr().cast_mut().cast();
            // SAFETY: то же; метка -- корректный SID `S-1-16-4096` в
            // неизменяемой константе (функция её только копирует).
            let status =
                unsafe { RtlAddIntegrityLabelToBoundaryDescriptor(&raw mut boundary.0, label_ptr) };
            check(status, "RtlAddIntegrityLabelToBoundaryDescriptor")?;
        }
        Ok(boundary)
    }
}

/// `STATUS_SUCCESS` -- `Ok`, иначе ошибка ОС с контекстом.
const fn check(status: i32, context: &'static str) -> Result<()> {
    if status == STATUS_SUCCESS {
        Ok(())
    } else {
        Err(status_to_error(status, context))
    }
}

/// Имя внутри пространства (и имя границы/псевдоним): непустое и без `\`.
pub(crate) const fn validate_relative(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(ShmError::InvalidConfig(
            "name inside a private namespace must not be empty",
        ));
    }
    let bytes = name.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            return Err(ShmError::InvalidConfig(
                "name inside a private namespace must be relative: no `Local\\`/`Global\\`/NT path or `\\`",
            ));
        }
        i += 1;
    }
    Ok(())
}

impl PrivateNamespace {
    /// Создать или открыть (OBJ_OPENIF) пространство с границей `boundary`,
    /// в которую входит SID текущего пользователя (и, если
    /// `allow_low_integrity`, метка Low).
    ///
    /// Метка Low добавляется, когда `security` --
    /// [`ObjectSecurity::CurrentUserAllowLowIntegrity`]. Тот же `security`
    /// задаёт дескриптор безопасности самого каталога, если его создаёт этот
    /// вызов (существующий открывается по своему DACL).
    ///
    /// `alias` -- псевдоним, как `lpAliasPrefix` у Win32
    /// `CreatePrivateNamespaceW`: ядро его не использует (пространство
    /// определяется только дескриптором границы), xshm хранит его для
    /// диагностики ([`PrivateNamespace::alias`], `Debug`). Пиры с одной
    /// границей и разными псевдонимами попадают в ОДНО пространство.
    ///
    /// Объекты внутри создаются с относительными именами: базовое имя
    /// канала/лобби/маяка -- без `Local\`/`Global\` (иначе `InvalidConfig`).
    ///
    /// # Errors
    ///
    /// `InvalidConfig` -- пустые `boundary`/`alias` или `\` в них,
    /// некорректный `ObjectSecurity::Descriptor`; `AccessDenied` --
    /// пространство создано с DACL, не включающим этот процесс; ошибка ОС.
    pub fn open_or_create(
        boundary: &str,
        alias: &str,
        security: &ObjectSecurity,
    ) -> Result<Arc<Self>> {
        validate_relative(boundary)?;
        validate_relative(alias)?;
        let low_integrity = matches!(security, ObjectSecurity::CurrentUserAllowLowIntegrity);
        let descriptor = Boundary::new(boundary, low_integrity)?;
        let attributes = security.attributes()?;

        for _ in 0..OPEN_OR_CREATE_ATTEMPTS {
            // ObjectName обязан быть NULL: ядро отвергает имя
            // (`STATUS_OBJECT_NAME_INVALID`), пространство ищется по границе.
            let mut create_attributes = OBJECT_ATTRIBUTES::new(null_mut(), 0, attributes.as_ptr());
            let mut handle: HANDLE = null_mut();
            // SAFETY: `handle` -- out-параметр на стеке; атрибуты и дескриптор
            // безопасности (`attributes`) живут до конца вызова; дескриптор
            // границы жив (`descriptor`) и только читается.
            let status = unsafe {
                NtCreatePrivateNamespace(
                    &raw mut handle,
                    DIRECTORY_ALL_ACCESS,
                    &raw mut create_attributes,
                    descriptor.0,
                )
            };
            if status == STATUS_SUCCESS {
                return Ok(Arc::new(Self::new(
                    handle,
                    boundary,
                    alias,
                    low_integrity,
                    true,
                )));
            }
            if status != STATUS_OBJECT_NAME_COLLISION {
                return Err(status_to_error(status, "NtCreatePrivateNamespace"));
            }

            let mut open_attributes = OBJECT_ATTRIBUTES::new(null_mut(), 0, null_mut());
            // SAFETY: то же, без дескриптора безопасности.
            let status = unsafe {
                NtOpenPrivateNamespace(
                    &raw mut handle,
                    DIRECTORY_ALL_ACCESS,
                    &raw mut open_attributes,
                    descriptor.0,
                )
            };
            if status == STATUS_SUCCESS {
                return Ok(Arc::new(Self::new(
                    handle,
                    boundary,
                    alias,
                    low_integrity,
                    false,
                )));
            }
            // Пространство исчезло между созданием и открытием (закрыт
            // последний чужой handle) -- создаём заново.
            if status != STATUS_OBJECT_PATH_NOT_FOUND && status != STATUS_OBJECT_NAME_NOT_FOUND {
                return Err(status_to_error(status, "NtOpenPrivateNamespace"));
            }
        }
        Err(status_to_error(
            STATUS_OBJECT_NAME_COLLISION,
            "NtCreatePrivateNamespace: namespace keeps vanishing",
        ))
    }

    fn new(
        handle: HANDLE,
        boundary: &str,
        alias: &str,
        low_integrity: bool,
        created: bool,
    ) -> Self {
        Self {
            handle,
            boundary: boundary.to_owned(),
            alias: alias.to_owned(),
            low_integrity,
            created,
        }
    }

    /// Имя границы.
    #[must_use]
    pub fn boundary(&self) -> &str {
        &self.boundary
    }

    /// Псевдоним (только диагностика, см. [`PrivateNamespace::open_or_create`]).
    #[must_use]
    pub fn alias(&self) -> &str {
        &self.alias
    }

    /// Пространство создал этот вызов `open_or_create` (а не открыл
    /// существующее).
    #[must_use]
    pub const fn created(&self) -> bool {
        self.created
    }

    /// Handle каталога для `OBJECT_ATTRIBUTES.RootDirectory`; действителен,
    /// пока жив `self`.
    pub(crate) const fn root(&self) -> HANDLE {
        self.handle
    }
}

impl Drop for PrivateNamespace {
    fn drop(&mut self) {
        // Только закрытие handle: без `NtDeletePrivateNamespace` (см. раздел
        // «Время жизни» у типа) -- пространство живёт, пока handle держит
        // любой пир; тест `creator_drop_keeps_namespace_for_peers`.
        // SAFETY: handle получен успешным `NtCreate/OpenPrivateNamespace`,
        // владеет им только `self`, закрывается ровно один раз.
        let _ = unsafe { NtClose(self.handle) };
    }
}

impl std::fmt::Debug for PrivateNamespace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrivateNamespace")
            .field("boundary", &self.boundary)
            .field("alias", &self.alias)
            .field("low_integrity", &self.low_integrity)
            .field("created", &self.created)
            .finish_non_exhaustive()
    }
}

/// Равенство -- «то же пространство»: одна граница (имя и метка Low).
/// Псевдоним ядро не учитывает, и здесь он не участвует.
impl PartialEq for PrivateNamespace {
    fn eq(&self, other: &Self) -> bool {
        self.boundary == other.boundary && self.low_integrity == other.low_integrity
    }
}

impl Eq for PrivateNamespace {}

impl core::hash::Hash for PrivateNamespace {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.boundary.hash(state);
        self.low_integrity.hash(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique(tag: &str) -> String {
        format!(
            "xshm-ns-{tag}-{}-{:?}",
            std::process::id(),
            std::time::Instant::now()
        )
        .replace(['.', ' ', ':'], "")
    }

    #[test]
    fn second_open_finds_the_same_namespace() {
        let boundary = unique("same");
        let first =
            PrivateNamespace::open_or_create(&boundary, "a", &ObjectSecurity::default()).unwrap();
        assert!(first.created());
        // Другой псевдоним -- то же пространство (ядро смотрит на границу).
        let second =
            PrivateNamespace::open_or_create(&boundary, "b", &ObjectSecurity::default()).unwrap();
        assert!(!second.created());
        assert_eq!(*first, *second);
    }

    #[test]
    fn namespace_vanishes_with_the_last_handle() {
        let boundary = unique("gone");
        let first =
            PrivateNamespace::open_or_create(&boundary, "a", &ObjectSecurity::default()).unwrap();
        drop(first);
        let again =
            PrivateNamespace::open_or_create(&boundary, "a", &ObjectSecurity::default()).unwrap();
        assert!(
            again.created(),
            "последний handle закрыт -- пространство новое"
        );
    }

    /// Граница с меткой Low -- другой дескриптор, другое пространство.
    #[test]
    fn low_integrity_boundary_is_a_different_namespace() {
        let boundary = unique("low");
        let medium =
            PrivateNamespace::open_or_create(&boundary, "a", &ObjectSecurity::default()).unwrap();
        let low = PrivateNamespace::open_or_create(
            &boundary,
            "a",
            &ObjectSecurity::CurrentUserAllowLowIntegrity,
        )
        .unwrap();
        assert!(medium.created());
        assert!(low.created());
        assert_ne!(*medium, *low);
    }

    #[test]
    fn invalid_boundary_or_alias_is_rejected() {
        for (boundary, alias) in [("", "a"), ("b", ""), ("Global\\b", "a"), ("b", "Local\\a")] {
            assert!(matches!(
                PrivateNamespace::open_or_create(boundary, alias, &ObjectSecurity::default()),
                Err(ShmError::InvalidConfig(_))
            ));
        }
    }
}
