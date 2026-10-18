//! Политика безопасности объектов (0.10): дескриптор задаёт создатель
//! (`ObjectSecurity`), по умолчанию -- владелец (пользователь процесса) +
//! SYSTEM вместо NULL DACL.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use xshm::{
    AutoHandler, AutoOptions, AutoServer, Beacon, ChannelConfig, ClientRegistration,
    DispatchHandler, DispatchOptions, DispatchServer, MultiClient, MultiClientHandler,
    MultiClientOptions, MultiHandler, MultiOptions, MultiServer, ObjectSecurity, SharedClient,
    SharedServer, ShmError,
};

// Своё объявление `NtQuerySecurityObject`: тесту нужно прочитать DACL
// созданного объекта, библиотека такой функции наружу не даёт.
#[cfg_attr(
    target_arch = "x86",
    link(name = "ntdll", kind = "raw-dylib", import_name_type = "undecorated")
)]
#[cfg_attr(not(target_arch = "x86"), link(name = "ntdll", kind = "raw-dylib"))]
unsafe extern "system" {
    fn NtQuerySecurityObject(
        handle: *mut core::ffi::c_void,
        security_information: u32,
        security_descriptor: *mut core::ffi::c_void,
        length: u32,
        length_needed: *mut u32,
    ) -> i32;
}

/// `DACL_SECURITY_INFORMATION`.
const DACL_SECURITY_INFORMATION: u32 = 4;
/// `OWNER_SECURITY_INFORMATION` -- владелец.
const OWNER_SECURITY_INFORMATION: u32 = 1;
/// `LABEL_SECURITY_INFORMATION` -- метка целостности (SACL).
const LABEL_SECURITY_INFORMATION: u32 = 0x10;
/// `SE_DACL_PRESENT` в `Control` дескриптора.
const SE_DACL_PRESENT: u16 = 0x0004;

fn unique_name(tag: &str) -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    format!(
        "XSHM_SECURITY_{tag}_{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Self-relative дескриптор объекта по handle (части `information`).
fn query_security(handle: isize, information: u32) -> Vec<u8> {
    let raw = core::ptr::without_provenance_mut(handle.cast_unsigned());
    let mut buffer = vec![0u32; 256];
    let mut needed = 0u32;
    let length = u32::try_from(buffer.len() * 4).unwrap_or(u32::MAX);
    // SAFETY: дескриптор объекта открыт сервером и жив весь вызов; буфер --
    // локальный `Vec` длины `length` байт (выровнен на 4), `needed` -- на стеке.
    let status = unsafe {
        NtQuerySecurityObject(
            raw,
            information,
            buffer.as_mut_ptr().cast(),
            length,
            &raw mut needed,
        )
    };
    assert_eq!(status, 0, "NtQuerySecurityObject: {status:#x}");
    buffer.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// DACL объекта по дескриптору: `None` -- NULL DACL (доступ всем),
/// `Some(n)` -- DACL из `n` записей.
fn dacl_ace_count(handle: isize) -> Option<u16> {
    let bytes = query_security(handle, DACL_SECURITY_INFORMATION);
    // Self-relative SECURITY_DESCRIPTOR: Control @2, OffsetDacl @16.
    let control = u16::from_le_bytes([bytes[2], bytes[3]]);
    assert_ne!(control & SE_DACL_PRESENT, 0, "DACL обязан присутствовать");
    let offset = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]) as usize;
    if offset == 0 {
        return None;
    }
    // ACL: AclRevision, Sbz1, AclSize(u16), AceCount(u16) @4.
    Some(u16::from_le_bytes([bytes[offset + 4], bytes[offset + 5]]))
}

#[test]
fn default_channel_works_for_same_user() {
    let name = unique_name("DEFAULT");
    let mut server = SharedServer::start(&name).unwrap();
    // Политика по умолчанию -- не NULL DACL: владелец + SYSTEM.
    let aces = dacl_ace_count(server.section_handle());
    assert_eq!(aces, Some(2), "ожидался DACL владелец + SYSTEM");

    let client = thread::spawn(move || SharedClient::connect(&name, Duration::from_secs(5)));
    server
        .wait_for_client(Some(Duration::from_secs(5)))
        .unwrap();
    let client = client.join().unwrap().unwrap();

    server.send_to_client(b"ping").unwrap();
    let mut out = Vec::new();
    assert_eq!(client.receive_from_server(&mut out).unwrap(), 4);
    assert_eq!(out, b"ping");
}

/// Task 13: безымянные события anonymous-канала -- с тем же дескриптором,
/// что секция: политика по умолчанию (владелец + SYSTEM). `GENERIC_ALL` в
/// DACL ядро отображает в права типа (`SECTION_ALL_ACCESS` у секции,
/// `EVENT_ALL_ACCESS` у события), поэтому байты DACL сверяются с объектами
/// того же типа именованного канала той же политики: секция -- с секцией,
/// каждое из 7 событий -- с событием. Владелец (ревью T13, M7) и метка
/// целостности от типа не зависят -- у всех 8 объектов те же, что у
/// секции (чужой владелец получил бы `WRITE_DAC`).
#[test]
fn anonymous_events_share_section_descriptor() {
    let server = SharedServer::start_anonymous().unwrap();
    let named = SharedServer::start(&unique_name("ANON_REF")).unwrap();
    let dacl = |handle| query_security(handle, DACL_SECURITY_INFORMATION);
    assert_eq!(dacl_ace_count(server.section_handle()), Some(2));
    assert_eq!(
        dacl(server.section_handle()),
        dacl(named.section_handle()),
        "DACL секции"
    );
    let event = dacl(named.get_event_handles().s2c_data);
    let owner = |handle| query_security(handle, OWNER_SECURITY_INFORMATION);
    let label = |handle| query_security(handle, LABEL_SECURITY_INFORMATION);
    let section_owner = owner(server.section_handle());
    let section_label = label(server.section_handle());
    assert_eq!(
        section_owner,
        owner(named.section_handle()),
        "владелец секции"
    );
    assert_eq!(section_label, label(named.section_handle()), "метка секции");
    let h = server.get_event_handles();
    for (what, handle) in [
        ("s2c_data", h.s2c_data),
        ("c2s_data", h.c2s_data),
        ("s2c_space", h.s2c_space),
        ("c2s_space", h.c2s_space),
        ("connect_ack", h.connect_ack),
        ("connect_req", h.connect_req),
        ("disconnect", h.disconnect),
    ] {
        assert_eq!(dacl(handle), event, "DACL события {what}");
        assert_eq!(owner(handle), section_owner, "владелец события {what}");
        assert_eq!(label(handle), section_label, "метка события {what}");
    }
}

#[test]
fn everyone_policy_matches_null_dacl() {
    let name = unique_name("EVERYONE");
    let config = ChannelConfig::default().security(ObjectSecurity::Everyone);
    let mut server = SharedServer::start_with(&name, &config).unwrap();
    assert_eq!(dacl_ace_count(server.section_handle()), None, "секция");
    let events = server.get_event_handles();
    assert_eq!(dacl_ace_count(events.s2c_data), None, "событие канала");

    let client = thread::spawn(move || SharedClient::connect(&name, Duration::from_secs(5)));
    server
        .wait_for_client(Some(Duration::from_secs(5)))
        .unwrap();
    client.join().unwrap().unwrap();
}

/// Готовый дескриптор с пустым DACL (никому ничего): создатель получает
/// дескрипторы, открыть объект по имени нельзя даже тому же пользователю --
/// отказ виден как `AccessDenied`, а не как `WindowsError`.
#[test]
fn descriptor_without_grants_denies_open() {
    let name = unique_name("EMPTY_DACL");
    let config = ChannelConfig::default().security(deny_all());
    let server = SharedServer::start_with(&name, &config).unwrap();
    assert_eq!(dacl_ace_count(server.section_handle()), Some(0));
    assert_eq!(connect_error(&name), Some(ShmError::AccessDenied));
}

/// Self-relative дескриптор с пустым DACL: Revision 1, Control =
/// `SE_SELF_RELATIVE | SE_DACL_PRESENT`, Owner/Group/Sacl = 0, OffsetDacl =
/// 20; ACL -- ревизия 2, размер 8, ни одной записи.
fn deny_all() -> ObjectSecurity {
    let mut sd = vec![1u8, 0, 0x04, 0x80];
    sd.extend_from_slice(&[0; 12]);
    sd.extend_from_slice(&20u32.to_le_bytes());
    sd.extend_from_slice(&[2, 0, 8, 0, 0, 0, 0, 0]);
    ObjectSecurity::Descriptor(sd)
}

/// Ошибка подключения клиента к каналу `name` (`None` -- подключился).
fn connect_error(name: &str) -> Option<ShmError> {
    SharedClient::connect(name, Duration::from_millis(200)).err()
}

struct Quiet;

impl AutoHandler for Quiet {}

impl MultiHandler for Quiet {
    fn on_client_connect(&self, _client_id: u32) {}
    fn on_client_disconnect(&self, _client_id: u32) {}
    fn on_message(&self, _client_id: u32, _data: &[u8]) {}
}

impl DispatchHandler for Quiet {
    fn on_client_connect(&self, _client_id: u32, _info: &ClientRegistration) {}
    fn on_client_disconnect(&self, _client_id: u32) {}
    fn on_message(&self, _client_id: u32, _data: &[u8]) {}
}

/// Политика из опций доходит до объектов каждого режима: Auto (канал),
/// Multi (каждый слот), Dispatch (лобби).
#[test]
fn options_security_reaches_every_mode() {
    let auto = unique_name("AUTO");
    let _auto = AutoServer::start(
        &auto,
        Arc::new(Quiet),
        AutoOptions {
            security: deny_all(),
            ..AutoOptions::default()
        },
    )
    .unwrap();
    assert_eq!(connect_error(&auto), Some(ShmError::AccessDenied));

    let multi = unique_name("MULTI");
    let multi_server = MultiServer::start(
        &multi,
        Arc::new(Quiet),
        MultiOptions {
            max_clients: 2,
            security: deny_all(),
            ..MultiOptions::default()
        },
    )
    .unwrap();
    for slot in 0..2 {
        assert_eq!(
            connect_error(&format!("{multi}_{slot}")),
            Some(ShmError::AccessDenied),
            "слот {slot}"
        );
    }
    multi_server.stop();

    let lobby = unique_name("DISPATCH");
    let dispatch = DispatchServer::start(
        &lobby,
        Arc::new(Quiet),
        DispatchOptions {
            security: deny_all(),
            ..DispatchOptions::default()
        },
    )
    .unwrap();
    assert_eq!(connect_error(&lobby), Some(ShmError::AccessDenied));
    dispatch.stop();

    // Политика по умолчанию -- тот же пользователь подключается.
    let open = unique_name("AUTO_DEFAULT");
    let _open = AutoServer::start(&open, Arc::new(Quiet), AutoOptions::default()).unwrap();
    assert_ne!(connect_error(&open), Some(ShmError::AccessDenied));
}

/// Маяк: политика создателя (`open_with`) действует на событие маяка;
/// существующий маяк открывается по своему DACL.
#[test]
fn beacon_policy_belongs_to_creator() {
    let name = unique_name("BEACON");
    let owner = Beacon::open_with(&name, &deny_all()).unwrap();
    assert_eq!(Beacon::open(&name).err(), Some(ShmError::AccessDenied));
    owner.raise().unwrap();

    let shared = unique_name("BEACON_EVERYONE");
    let server = Beacon::open_with(&shared, &ObjectSecurity::Everyone).unwrap();
    let client = Beacon::open(&shared).unwrap();
    server.raise().unwrap();
    assert!(client.is_raised().unwrap());
}

/// Метка Low integrity ставится без особых привилегий: канал создаётся и
/// работает для того же пользователя.
#[test]
fn low_integrity_policy_creates_working_channel() {
    let name = unique_name("LOW");
    let config = ChannelConfig::default().security(ObjectSecurity::CurrentUserAllowLowIntegrity);
    let mut server = SharedServer::start_with(&name, &config).unwrap();

    // Метка у созданного объекта: SACL @12, одна SYSTEM_MANDATORY_LABEL_ACE
    // (тип 0x11, маска NO_WRITE_UP = 1) для S-1-16-4096.
    let sd = query_security(server.section_handle(), LABEL_SECURITY_INFORMATION);
    let sacl = u32::from_le_bytes([sd[12], sd[13], sd[14], sd[15]]) as usize;
    assert_ne!(sacl, 0, "метка целостности обязана быть");
    assert_eq!(
        u16::from_le_bytes([sd[sacl + 4], sd[sacl + 5]]),
        1,
        "одна ACE"
    );
    let ace = &sd[sacl + 8..];
    assert_eq!(ace[0], 0x11, "SYSTEM_MANDATORY_LABEL_ACE_TYPE");
    assert_eq!(
        u32::from_le_bytes([ace[4], ace[5], ace[6], ace[7]]),
        1,
        "NO_WRITE_UP"
    );
    assert_eq!(
        &ace[8..20],
        &[1, 1, 0, 0, 0, 0, 0, 16, 0x00, 0x10, 0, 0],
        "S-1-16-4096 (Low)"
    );

    let client = thread::spawn(move || SharedClient::connect(&name, Duration::from_secs(5)));
    server
        .wait_for_client(Some(Duration::from_secs(5)))
        .unwrap();
    client.join().unwrap().unwrap();
}

/// Ревью T9 (важное 1): `MultiClient` против сервера, чей DACL его не
/// включает, сообщает `AccessDenied`, а не `NotConnected` («сервера нет»).
#[test]
fn multi_client_reports_access_denied() {
    #[derive(Default)]
    struct Errors(Mutex<Vec<ShmError>>);
    impl MultiClientHandler for Errors {
        fn on_connect(&self, _slot_id: u32) {}
        fn on_disconnect(&self) {}
        fn on_message(&self, _data: &[u8]) {}
        fn on_error(&self, err: ShmError) {
            self.0.lock().unwrap().push(err);
        }
    }

    let name = unique_name("MULTI_DENIED");
    let server = MultiServer::start(
        &name,
        Arc::new(Quiet),
        MultiOptions {
            max_clients: 3,
            security: deny_all(),
            ..MultiOptions::default()
        },
    )
    .unwrap();
    let errors = Arc::new(Errors::default());
    let client = MultiClient::connect(
        &name,
        errors.clone(),
        MultiClientOptions {
            retry_delay: Duration::from_millis(20),
            ..MultiClientOptions::default()
        },
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while errors.0.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "нет on_error");
        thread::sleep(Duration::from_millis(5));
    }
    // 0.10 (Task 11): о той же ошибке вне подключения -- один раз за
    // эпизод; повторы (`retry_delay` = 20 мс) идут молча.
    thread::sleep(Duration::from_millis(200));
    client.stop();
    assert_eq!(errors.0.lock().unwrap().len(), 1);
    let seen = errors.0.lock().unwrap().clone();
    assert!(
        seen.iter().all(|e| *e == ShmError::AccessDenied),
        "ожидался только AccessDenied, получено {seen:?}"
    );
    server.stop();

    // Сервера нет -- по-прежнему NotConnected (отказ в доступе не путается
    // с отсутствием слотов).
    let absent = Arc::new(Errors::default());
    let client = MultiClient::connect(
        &unique_name("MULTI_ABSENT"),
        absent.clone(),
        MultiClientOptions::default(),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while absent.0.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "нет on_error");
        thread::sleep(Duration::from_millis(5));
    }
    client.stop();
    assert_eq!(absent.0.lock().unwrap()[0], ShmError::NotConnected);
}

#[test]
fn invalid_descriptor_is_rejected() {
    let config = ChannelConfig::default().security(ObjectSecurity::Descriptor(vec![0xFF; 7]));
    assert!(matches!(
        SharedServer::start_with(&unique_name("BAD_SD"), &config),
        Err(ShmError::InvalidConfig(_))
    ));
}

/// Ручной прогон двумя пользователями одной сессии:
/// 1. пользователь A: `cargo test --test security -- --ignored --exact hold_channel_for_foreign_user`
///    (держит канал `Local\XSHM_SECURITY_FOREIGN` 120 с, политика по умолчанию);
/// 2. пользователь B (`runas /user:B cmd`, тот же каталог):
///    `cargo test --test security -- --ignored --exact foreign_user_is_denied`.
///
/// Ожидание: B получает `AccessDenied` -- раньше (NULL DACL) подключался.
#[test]
#[ignore = "нужен второй пользователь"]
fn foreign_user_is_denied() {
    assert_eq!(
        SharedClient::connect("Local\\XSHM_SECURITY_FOREIGN", Duration::from_secs(1)).err(),
        Some(ShmError::AccessDenied)
    );
}

/// Сторона пользователя A для `foreign_user_is_denied`.
#[test]
#[ignore = "нужен второй пользователь"]
fn hold_channel_for_foreign_user() {
    let _server = SharedServer::start("Local\\XSHM_SECURITY_FOREIGN").unwrap();
    thread::sleep(Duration::from_secs(120));
}

/// Task 11 (R19): маяк присутствия создаётся с политикой канала (Auto,
/// база Multi, лобби Dispatch). Клиент, которому DACL сервера отказал и в
/// канале, и в маяке, переходит на резервный повтор по таймеру (решение
/// R-M) и сообщает `AccessDenied` один раз, а не на каждой попытке.
#[test]
fn presence_objects_use_channel_policy() {
    #[derive(Default)]
    struct Errors(Mutex<Vec<ShmError>>);
    impl AutoHandler for Errors {
        fn on_error(&self, err: ShmError) {
            self.0.lock().unwrap().push(err);
        }
    }

    let name = unique_name("PRESENCE_DENIED");
    let server = AutoServer::start(
        &name,
        Arc::new(Quiet),
        AutoOptions {
            security: deny_all(),
            ..AutoOptions::default()
        },
    )
    .unwrap();
    assert_eq!(Beacon::open(&name).err(), Some(ShmError::AccessDenied));

    let errors = Arc::new(Errors::default());
    let client = xshm::AutoClient::connect(
        &name,
        errors.clone(),
        AutoOptions {
            reconnect_delay: Duration::from_millis(20),
            ..AutoOptions::default()
        },
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while errors.0.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "нет on_error");
        thread::sleep(Duration::from_millis(5));
    }
    // Около 50 резервных попыток за секунду -- и ни одной новой ошибки.
    thread::sleep(Duration::from_secs(1));
    client.stop();
    assert_eq!(
        errors.0.lock().unwrap().clone(),
        vec![ShmError::AccessDenied]
    );
    server.stop();

    let multi = unique_name("PRESENCE_MULTI");
    let multi_server = MultiServer::start(
        &multi,
        Arc::new(Quiet),
        MultiOptions {
            max_clients: 1,
            security: deny_all(),
            ..MultiOptions::default()
        },
    )
    .unwrap();
    // Маяк базы Multi -- `<база>_multi_beacon` (M2 ревью Task 11).
    assert_eq!(
        Beacon::open(&format!("{multi}_multi")).err(),
        Some(ShmError::AccessDenied)
    );
    multi_server.stop();

    let lobby = unique_name("PRESENCE_LOBBY");
    let dispatch = DispatchServer::start(
        &lobby,
        Arc::new(Quiet),
        DispatchOptions {
            security: deny_all(),
            ..DispatchOptions::default()
        },
    )
    .unwrap();
    assert_eq!(Beacon::open(&lobby).err(), Some(ShmError::AccessDenied));
    dispatch.stop();
}
