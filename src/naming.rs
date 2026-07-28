//! Формирование имён kernel-объектов (Section/Event) из базового имени канала.
//!
//! Имя, переданное вызывающим кодом, эмитится КАК ЕСТЬ, без библиотечного
//! префикса (`XSHM_SEG_`/`XSHM_` убраны в 0.6.0) — пространство имён полностью
//! контролирует встраивающий код. Вызывающая сторона сама гарантирует, что
//! базовое имя не конфликтует с другими объектами в том же namespace.

#[derive(Clone, Copy)]
pub enum Direction {
    ServerToClient,
    ClientToServer,
}

impl Direction {
    const fn as_str(self) -> &'static str {
        match self {
            Direction::ServerToClient => "S2C",
            Direction::ClientToServer => "C2S",
        }
    }
}

/// Явно заданный вызывающим namespace, который нельзя переопределять.
///
/// Без этой проверки имя `Global\Chan` превращалось в `Local\Global\Chan`, а
/// `to_nt_path` разворачивал его в `\Sessions\<id>\BaseNamedObjects\Global\Chan`
/// — несуществующий путь, из-за чего межсессионный IPC через `Global\`
/// (заявленный в README и в доке `to_nt_path`) не работал вообще.
fn has_explicit_namespace(base: &str) -> bool {
    base.starts_with("Global\\") || base.starts_with("Local\\") || base.starts_with('\\')
}

/// Имя секции: `Local\{base}`, либо `base` как есть, если namespace задан явно
/// (`Global\...`, `Local\...`, либо готовый NT-путь `\...`).
pub fn mapping_name(base: &str) -> String {
    if has_explicit_namespace(base) {
        base.to_owned()
    } else {
        format!("Local\\{base}")
    }
}

/// Префикс имён событий канала. Namespace выбирается по тем же правилам, что
/// и в `mapping_name`, иначе секция и события канала оказались бы в РАЗНЫХ
/// namespace'ах.
fn event_prefix(base: &str) -> String {
    if has_explicit_namespace(base) {
        format!("{base}_")
    } else {
        format!("Local\\{base}_")
    }
}

pub fn event_name(base: &str, direction: Direction, suffix: &str) -> String {
    format!("{}{}_{}", event_prefix(base), direction.as_str(), suffix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::EVENT_DATA_SUFFIX;

    #[test]
    fn plain_name_gets_local_namespace() {
        assert_eq!(mapping_name("Chan"), "Local\\Chan");
        assert_eq!(
            event_name("Chan", Direction::ServerToClient, EVENT_DATA_SUFFIX),
            "Local\\Chan_S2C_DATA"
        );
    }

    /// Регрессия (аудит 2026-07-28): `Global\` безусловно оборачивался в
    /// `Local\`, из-за чего NT-путь получался вида
    /// `\Sessions\<id>\BaseNamedObjects\Global\Chan` и создание объекта падало
    /// — межсессионный IPC был неработоспособен, вопреки документации.
    #[test]
    fn global_prefix_is_preserved() {
        assert_eq!(mapping_name("Global\\Chan"), "Global\\Chan");
        assert_eq!(
            event_name("Global\\Chan", Direction::ClientToServer, EVENT_DATA_SUFFIX),
            "Global\\Chan_C2S_DATA"
        );
    }

    #[test]
    fn explicit_local_prefix_is_not_doubled() {
        assert_eq!(mapping_name("Local\\Chan"), "Local\\Chan");
        assert_eq!(
            event_name("Local\\Chan", Direction::ServerToClient, EVENT_DATA_SUFFIX),
            "Local\\Chan_S2C_DATA"
        );
    }

    #[test]
    fn raw_nt_path_is_passed_through() {
        assert_eq!(
            mapping_name("\\BaseNamedObjects\\Chan"),
            "\\BaseNamedObjects\\Chan"
        );
    }

    /// Секция и события обязаны попадать в ОДИН namespace — иначе клиент
    /// откроет секцию, но не найдёт события (или наоборот).
    #[test]
    fn section_and_events_share_namespace() {
        for base in ["Chan", "Global\\Chan", "Local\\Chan"] {
            let section = mapping_name(base);
            let event = event_name(base, Direction::ServerToClient, EVENT_DATA_SUFFIX);
            let section_ns = section.rsplit_once('\\').map(|(ns, _)| ns.to_owned());
            let event_ns = event.rsplit_once('\\').map(|(ns, _)| ns.to_owned());
            assert_eq!(section_ns, event_ns, "namespace mismatch for base {base}");
        }
    }
}
