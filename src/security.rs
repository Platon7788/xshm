//! Политика безопасности именованных объектов (0.10).
//!
//! До 0.10 каждый именованный объект xshm (секция канала, 7 событий, маяк,
//! мьютекс лобби) создавался с NULL DACL: открыть, читать и писать его мог
//! любой процесс машины, включая процессы других пользователей. Теперь
//! дескриптор выбирает создатель ([`ObjectSecurity`]), по умолчанию --
//! [`ObjectSecurity::CurrentUser`]: доступ только пользователю процесса и
//! SYSTEM. Открывающая сторона дескриптор не передаёт (как и раньше):
//! решает DACL объекта, отказ -- [`ShmError::AccessDenied`].
//!
//! Дескриптор строится заново на каждое создание ([`SecurityAttributes`]):
//! буферы SID/ACL принадлежат ему и живут до конца вызова `NtCreate*`,
//! никакого глобального изменяемого состояния.

use core::ptr::null_mut;

use crate::error::{Result, ShmError};
use crate::ntapi::{
    ACCESS_MASK, ACL, ACL_REVISION, GENERIC_ALL, HANDLE, NT_CURRENT_PROCESS, NTSTATUS, NtClose,
    NtOpenProcessToken, NtQueryInformationToken, NullDaclSecurityDescriptor, PSID, PVOID,
    RtlAddAccessAllowedAce, RtlAddMandatoryAce, RtlCreateAcl, RtlCreateSecurityDescriptor,
    RtlLengthSid, RtlSetDaclSecurityDescriptor, RtlSetSaclSecurityDescriptor,
    RtlValidRelativeSecurityDescriptor, SECURITY_DESCRIPTOR, SECURITY_DESCRIPTOR_REVISION,
    STATUS_BUFFER_TOO_SMALL, STATUS_SUCCESS, SYSTEM_MANDATORY_LABEL_ACE_TYPE,
    SYSTEM_MANDATORY_LABEL_NO_WRITE_UP, TOKEN_QUERY, TOKEN_USER, TokenUser, ULONG,
};
use crate::win::status_to_error;

/// Кто получает доступ к именованным объектам, которые создаёт сторона
/// (сервер канала, слоты Multi, лобби и каналы Dispatch, маяк).
///
/// Политику задаёт **создатель** объекта; открывающий получает доступ по
/// DACL созданного объекта. По умолчанию -- [`ObjectSecurity::CurrentUser`].
///
/// Миграция с 0.9 (NULL DACL у всех объектов): служба SYSTEM (сессия 0) и
/// процесс пользователя на `Global\`-канале с политикой по умолчанию друг
/// друга больше не видят, если объекты создаёт служба (DACL -- только
/// SYSTEM): пользователь получит `AccessDenied`. Такому серверу нужна
/// [`ObjectSecurity::Everyone`] (поведение 0.9) или свой
/// [`ObjectSecurity::Descriptor`] (например, SYSTEM + группа `Users`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ObjectSecurity {
    /// Владелец (пользователь процесса) + SYSTEM — по умолчанию.
    ///
    /// DACL из двух ACE `ACCESS_ALLOWED` с маской `GENERIC_ALL`: SID
    /// пользователя токена процесса (`TokenUser`) и `S-1-5-18`
    /// (LocalSystem); процесс, запущенный от SYSTEM, получает одну запись.
    /// Владелец и группа объекта -- по умолчанию токена. SID пользователя
    /// берётся из токена **процесса**, а не из токена имперсонации потока.
    #[default]
    CurrentUser,
    /// То же + метка Low integrity: доступ из песочниц (браузерные процессы,
    /// AppContainer Low).
    ///
    /// SACL с одной меткой `SYSTEM_MANDATORY_LABEL` `S-1-16-4096` (Low) и
    /// политикой `NO_WRITE_UP` (SDDL `S:(ML;;NW;;;LW)`): процесс того же
    /// пользователя с уровнем Low получает запись, без метки объект --
    /// Medium, и Low-процессу запись запрещена. AppContainer дополнительно
    /// требует ACE для SID пакета или `ALL APPLICATION PACKAGES` -- это
    /// [`ObjectSecurity::Descriptor`].
    CurrentUserAllowLowIntegrity,
    /// NULL DACL — любой процесс (поведение ≤ 0.9; нужно, например, службе
    /// SYSTEM ↔ пользователь без общей группы).
    Everyone,
    /// Готовый self-relative SECURITY_DESCRIPTOR (байты); проверяется
    /// `RtlValidRelativeSecurityDescriptor` (с длиной буфера -- смещения
    /// внутри дескриптора не выходят за байты). Некорректный --
    /// `InvalidConfig` до создания объекта.
    Descriptor(Vec<u8>),
}

impl ObjectSecurity {
    /// Построить дескриптор для одного или нескольких вызовов `NtCreate*`.
    ///
    /// # Errors
    ///
    /// `InvalidConfig` -- `Descriptor` некорректен; ошибка ОС при чтении
    /// токена процесса или построении ACL.
    pub(crate) fn attributes(&self) -> Result<SecurityAttributes> {
        match self {
            Self::CurrentUser => SecurityAttributes::current_user(false),
            Self::CurrentUserAllowLowIntegrity => SecurityAttributes::current_user(true),
            Self::Everyone => Ok(SecurityAttributes(Inner::NullDacl(
                NullDaclSecurityDescriptor::new(),
            ))),
            Self::Descriptor(bytes) => SecurityAttributes::self_relative(bytes),
        }
    }
}

/// SID LocalSystem `S-1-5-18`: ревизия 1, одно подполномочие, NT Authority
/// (5, big-endian в 6 байтах), RID 18 -- слова `u32` в порядке памяти.
const SYSTEM_SID: [u32; 3] = [
    u32::from_le_bytes([1, 1, 0, 0]),
    u32::from_le_bytes([0, 0, 0, 5]),
    18,
];

/// SID метки целостности Low `S-1-16-4096` (Mandatory Label Authority 16).
pub(crate) const LOW_LABEL_SID: [u32; 3] = [
    u32::from_le_bytes([1, 1, 0, 0]),
    u32::from_le_bytes([0, 0, 0, 16]),
    0x1000,
];

/// Наибольший размер SID (`SECURITY_MAX_SID_SIZE`: 15 подполномочий).
const MAX_SID_SIZE: usize = 68;

/// Готовый дескриптор безопасности для `OBJECT_ATTRIBUTES` -- владеет всеми
/// буферами, на которые указывает (ACL, байты self-relative дескриптора).
pub(crate) struct SecurityAttributes(Inner);

enum Inner {
    /// Абсолютный дескриптор: `sd` указывает в кучу `dacl`/`sacl` (адреса
    /// куч не меняются при перемещении `Vec`).
    Absolute {
        sd: SECURITY_DESCRIPTOR,
        /// Держат кучу, на которую указывает `sd` (читаются только тестами).
        _dacl: Vec<u32>,
        _sacl: Option<Vec<u32>>,
    },
    /// NULL DACL.
    NullDacl(NullDaclSecurityDescriptor),
    /// Проверенный self-relative дескриптор (выровнен на 4).
    SelfRelative(Vec<u32>),
}

/// Дескриптор токена процесса, закрывается в `Drop`.
struct Token(HANDLE);

impl Drop for Token {
    fn drop(&mut self) {
        // SAFETY: дескриптор получен успешным `NtOpenProcessToken`, владеет им
        // только `Token`, закрывается ровно один раз.
        let _ = unsafe { NtClose(self.0) };
    }
}

/// `STATUS_SUCCESS` -- `Ok`, иначе ошибка ОС с контекстом.
const fn check(status: NTSTATUS, context: &'static str) -> Result<()> {
    if status == STATUS_SUCCESS {
        Ok(())
    } else {
        Err(status_to_error(status, context))
    }
}

/// Длина буфера в байтах как `ULONG`.
fn byte_len(words: &[u32]) -> Result<ULONG> {
    ULONG::try_from(size_of_val(words))
        .map_err(|_| ShmError::InvalidConfig("security buffer exceeds 4 GiB"))
}

/// SID пользователя токена процесса (`TokenUser`), слова `u32` ровно его
/// длины.
pub(crate) fn current_user_sid() -> Result<Vec<u32>> {
    let mut raw: HANDLE = null_mut();
    // SAFETY: псевдо-handle текущего процесса не требует закрытия;
    // out-параметр -- локальная переменная.
    let status = unsafe { NtOpenProcessToken(NT_CURRENT_PROCESS, TOKEN_QUERY, &raw mut raw) };
    check(status, "NtOpenProcessToken")?;
    let token = Token(raw);

    // Буфер из `usize`: выравнивание `TOKEN_USER` (указатель) соблюдено.
    let word = size_of::<usize>();
    let mut buffer: Vec<usize> = vec![0; (size_of::<TOKEN_USER>() + MAX_SID_SIZE).div_ceil(word)];
    let mut status = STATUS_BUFFER_TOO_SMALL;
    for _ in 0..2 {
        let length = ULONG::try_from(buffer.len() * word)
            .map_err(|_| ShmError::InvalidConfig("token buffer exceeds 4 GiB"))?;
        let mut needed: ULONG = 0;
        // SAFETY: дескриптор токена жив (`token`); буфер -- локальный `Vec`
        // ровно `length` байт, выровненный на указатель; `needed` -- на стеке.
        status = unsafe {
            NtQueryInformationToken(
                token.0,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                length,
                &raw mut needed,
            )
        };
        if status != STATUS_BUFFER_TOO_SMALL {
            break;
        }
        buffer.resize((needed as usize).div_ceil(word), 0);
    }
    check(status, "NtQueryInformationToken(TokenUser)")?;

    let user = buffer.as_ptr().cast::<TOKEN_USER>();
    // SAFETY: вызов успешен -- ядро записало в начало буфера `TOKEN_USER`;
    // буфер выровнен на указатель и не меньше `size_of::<TOKEN_USER>()`.
    let sid: PSID = unsafe { user.read() }.User.Sid;
    // Ядро кладёт SID в тот же буфер: проверяем, что он целиком внутри, до
    // любого чтения по указателю.
    let start = buffer.as_ptr().addr();
    let end = start + buffer.len() * word;
    if sid.is_null() || sid.addr() < start || sid.addr() + 8 > end {
        return Err(ShmError::Corrupted);
    }
    // SAFETY: `sid` указывает внутрь живого буфера (проверено выше), где
    // ядро разместило корректный SID; `RtlLengthSid` читает его заголовок.
    let length = unsafe { RtlLengthSid(sid) } as usize;
    if !(8..=MAX_SID_SIZE).contains(&length)
        || !length.is_multiple_of(4)
        || sid.addr() + length > end
    {
        return Err(ShmError::Corrupted);
    }
    let mut copy = vec![0u32; length / 4];
    // SAFETY: источник -- `length` байт внутри живого `buffer` (проверено),
    // приёмник -- свой `Vec` ровно `length` байт; области не пересекаются.
    unsafe {
        core::ptr::copy_nonoverlapping(sid.cast::<u8>(), copy.as_mut_ptr().cast::<u8>(), length);
    }
    Ok(copy)
}

/// ACL с одной записью на каждый SID: `ACCESS_ALLOWED` (маска `GENERIC_ALL`)
/// или, при `label`, обязательная метка `NO_WRITE_UP`.
fn build_acl(sids: &[&[u32]], label: bool) -> Result<Vec<u32>> {
    // Заголовок ACL 8 байт; каждая ACE -- заголовок 4 + маска 4 + SID.
    let words = 2 + sids.iter().map(|sid| 2 + sid.len()).sum::<usize>();
    let mut acl = vec![0u32; words];
    let length = byte_len(&acl)?;
    let acl_ptr = acl.as_mut_ptr().cast::<ACL>();
    // SAFETY: `acl_ptr` -- собственный буфер `length` байт, выровненный на 4
    // (`Vec<u32>`), живой весь вызов.
    let status = unsafe { RtlCreateAcl(acl_ptr, length, ACL_REVISION) };
    check(status, "RtlCreateAcl")?;
    for sid in sids {
        // Rtl-функции принимают `PSID` без `const`, но SID только читают
        // (копируют в ACE).
        let sid_ptr: PSID = sid.as_ptr().cast_mut().cast();
        let status = if label {
            // SAFETY: ACL инициализирован выше и рассчитан на все записи; SID
            // -- корректный SID в живом срезе.
            unsafe {
                RtlAddMandatoryAce(
                    acl_ptr,
                    ACL_REVISION,
                    0,
                    sid_ptr,
                    SYSTEM_MANDATORY_LABEL_ACE_TYPE,
                    SYSTEM_MANDATORY_LABEL_NO_WRITE_UP,
                )
            }
        } else {
            let mask: ACCESS_MASK = GENERIC_ALL;
            // SAFETY: то же -- ACL инициализирован, места хватает, SID жив.
            unsafe { RtlAddAccessAllowedAce(acl_ptr, ACL_REVISION, mask, sid_ptr) }
        };
        check(status, "RtlAddAce")?;
    }
    Ok(acl)
}

impl SecurityAttributes {
    /// Владелец + SYSTEM (`label` -- плюс метка Low в SACL).
    fn current_user(label: bool) -> Result<Self> {
        let user = current_user_sid()?;
        // Процесс от SYSTEM: одна запись вместо двух одинаковых.
        let dacl = if user.as_slice() == SYSTEM_SID.as_slice() {
            build_acl(&[&SYSTEM_SID], false)?
        } else {
            build_acl(&[&user, &SYSTEM_SID], false)?
        };
        let sacl = if label {
            Some(build_acl(&[&LOW_LABEL_SID], true)?)
        } else {
            None
        };

        let mut sd = SECURITY_DESCRIPTOR::new();
        // SAFETY: `sd` -- локальная структура нужного размера; функция только
        // заполняет её поля.
        let status =
            unsafe { RtlCreateSecurityDescriptor(&raw mut sd, SECURITY_DESCRIPTOR_REVISION) };
        check(status, "RtlCreateSecurityDescriptor")?;
        // SAFETY: `sd` инициализирован; DACL -- куча `dacl`, которая переезжает
        // в возвращаемую структуру вместе с `sd` (адрес кучи не меняется).
        let status = unsafe {
            RtlSetDaclSecurityDescriptor(&raw mut sd, 1, dacl.as_ptr().cast_mut().cast(), 0)
        };
        check(status, "RtlSetDaclSecurityDescriptor")?;
        if let Some(sacl) = &sacl {
            // SAFETY: то же для SACL -- куча `sacl` живёт вместе с `sd`.
            let status = unsafe {
                RtlSetSaclSecurityDescriptor(&raw mut sd, 1, sacl.as_ptr().cast_mut().cast(), 0)
            };
            check(status, "RtlSetSaclSecurityDescriptor")?;
        }
        Ok(Self(Inner::Absolute {
            sd,
            _dacl: dacl,
            _sacl: sacl,
        }))
    }

    /// Готовый self-relative дескриптор: копия (выровненная на 4) и
    /// проверка `RtlValidRelativeSecurityDescriptor` с длиной буфера.
    fn self_relative(bytes: &[u8]) -> Result<Self> {
        const INVALID: ShmError = ShmError::InvalidConfig(
            "ObjectSecurity::Descriptor is not a valid self-relative SECURITY_DESCRIPTOR",
        );
        if bytes.is_empty() {
            return Err(INVALID);
        }
        let length = ULONG::try_from(bytes.len()).map_err(|_| INVALID)?;
        let mut aligned = vec![0u32; bytes.len().div_ceil(4)];
        for (word, chunk) in aligned.iter_mut().zip(bytes.chunks(4)) {
            let mut le = [0u8; 4];
            le[..chunk.len()].copy_from_slice(chunk);
            *word = u32::from_le_bytes(le);
        }
        // SAFETY: буфер -- собственный `Vec` не короче `length` байт; функция
        // читает только внутри `length` (смещения дескриптора проверяются
        // против неё) и ничего не пишет.
        let valid =
            unsafe { RtlValidRelativeSecurityDescriptor(aligned.as_mut_ptr().cast(), length, 0) };
        if valid == 0 {
            return Err(INVALID);
        }
        Ok(Self(Inner::SelfRelative(aligned)))
    }

    /// Указатель для `OBJECT_ATTRIBUTES.SecurityDescriptor`; действителен,
    /// пока жив `self`. NT-вызовы создания дескриптор только читают.
    pub(crate) const fn as_ptr(&self) -> PVOID {
        match &self.0 {
            Inner::Absolute { sd, .. } => sd.as_ptr(),
            Inner::NullDacl(null) => null.as_ptr(),
            Inner::SelfRelative(bytes) => bytes.as_ptr().cast_mut().cast(),
        }
    }

    /// Байты DACL абсолютного дескриптора (`None` -- NULL DACL или
    /// self-relative).
    #[cfg(test)]
    fn dacl(&self) -> Option<Vec<u8>> {
        match &self.0 {
            Inner::Absolute { _dacl: dacl, .. } => {
                Some(dacl.iter().flat_map(|w| w.to_le_bytes()).collect())
            }
            _ => None,
        }
    }

    /// Байты SACL абсолютного дескриптора.
    #[cfg(test)]
    fn sacl(&self) -> Option<Vec<u8>> {
        match &self.0 {
            Inner::Absolute {
                _sacl: Some(sacl), ..
            } => Some(sacl.iter().flat_map(|w| w.to_le_bytes()).collect()),
            _ => None,
        }
    }
}

/// Байты SID (слова `u32` в порядке памяти).
#[cfg(test)]
fn sid_bytes(sid: &[u32]) -> Vec<u8> {
    sid.iter().flat_map(|w| w.to_le_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ACCESS_ALLOWED_ACE_TYPE`.
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0x00;

    /// Записи ACL: (тип, маска, SID).
    fn parse_acl(acl: &[u8]) -> Vec<(u8, u32, Vec<u8>)> {
        let count = usize::from(u16::from_le_bytes([acl[4], acl[5]]));
        let mut offset = 8;
        let mut aces = Vec::new();
        for _ in 0..count {
            let kind = acl[offset];
            let size = usize::from(u16::from_le_bytes([acl[offset + 2], acl[offset + 3]]));
            let mask = u32::from_le_bytes(acl[offset + 4..offset + 8].try_into().unwrap());
            aces.push((kind, mask, acl[offset + 8..offset + size].to_vec()));
            offset += size;
        }
        aces
    }

    #[test]
    fn current_user_descriptor_grants_owner_and_system() {
        let user = current_user_sid().unwrap();
        assert_ne!(
            sid_bytes(&user),
            sid_bytes(&SYSTEM_SID),
            "тест -- не от SYSTEM"
        );
        let attributes = ObjectSecurity::CurrentUser.attributes().unwrap();
        let aces = parse_acl(&attributes.dacl().unwrap());
        assert_eq!(
            aces,
            vec![
                (ACCESS_ALLOWED_ACE_TYPE, GENERIC_ALL, sid_bytes(&user)),
                (ACCESS_ALLOWED_ACE_TYPE, GENERIC_ALL, sid_bytes(&SYSTEM_SID)),
            ]
        );
        assert!(attributes.sacl().is_none(), "без метки целостности");
        assert_eq!(ObjectSecurity::default(), ObjectSecurity::CurrentUser);
    }

    #[test]
    fn low_integrity_policy_adds_low_mandatory_label() {
        let attributes = ObjectSecurity::CurrentUserAllowLowIntegrity
            .attributes()
            .unwrap();
        assert_eq!(parse_acl(&attributes.dacl().unwrap()).len(), 2);
        assert_eq!(
            parse_acl(&attributes.sacl().unwrap()),
            vec![(
                SYSTEM_MANDATORY_LABEL_ACE_TYPE,
                SYSTEM_MANDATORY_LABEL_NO_WRITE_UP,
                sid_bytes(&LOW_LABEL_SID)
            )]
        );
    }

    #[test]
    fn everyone_policy_is_null_dacl() {
        let attributes = ObjectSecurity::Everyone.attributes().unwrap();
        assert!(attributes.dacl().is_none());
        assert!(attributes.sacl().is_none());
        assert!(!attributes.as_ptr().is_null());
    }

    #[test]
    fn descriptor_policy_is_validated() {
        // Self-relative, NULL DACL: SE_SELF_RELATIVE | SE_DACL_PRESENT, без смещений.
        let mut null_dacl = vec![1u8, 0, 0x04, 0x80];
        null_dacl.extend_from_slice(&[0; 16]);
        assert!(
            ObjectSecurity::Descriptor(null_dacl.clone())
                .attributes()
                .is_ok()
        );

        // Не self-relative (абсолютный флаг) -- отказ.
        let mut absolute = null_dacl.clone();
        absolute[3] = 0;
        // Смещение DACL за пределы буфера -- отказ без чтения чужой памяти.
        let mut out_of_bounds = null_dacl.clone();
        out_of_bounds[16] = 200;
        for bad in [Vec::new(), vec![0xFF; 7], absolute, out_of_bounds] {
            assert!(matches!(
                ObjectSecurity::Descriptor(bad).attributes(),
                Err(ShmError::InvalidConfig(_))
            ));
        }
    }

    /// SID SYSTEM и метки Low собраны верно (длина по `RtlLengthSid`).
    #[test]
    fn well_known_sids_have_expected_length() {
        for sid in [SYSTEM_SID, LOW_LABEL_SID] {
            let mut copy = sid;
            // SAFETY: `copy` -- корректный SID с одним подполномочием на стеке.
            let length = unsafe { RtlLengthSid(copy.as_mut_ptr().cast()) };
            assert_eq!(length, 12);
        }
    }
}
