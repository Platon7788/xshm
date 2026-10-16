//! Формирование имён kernel-объектов (Section/Event) из базового имени канала.
//!
//! Имя, переданное вызывающим кодом, эмитится КАК ЕСТЬ, без библиотечного
//! префикса (`XSHM_SEG_`/`XSHM_` убраны в 0.6.0) — пространство имён полностью
//! контролирует встраивающий код. Вызывающая сторона сама гарантирует, что
//! базовое имя не конфликтует с другими объектами в том же namespace.
//!
//! 0.10: объект может жить в частном пространстве имён ([`ObjectPath`] с
//! корнем) -- тогда имя относительное, без `Local\`/`Global\`.

use crate::namespace::PrivateNamespace;

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

/// Имя объекта ядра вместе с каталогом, в котором его искать (0.10).
///
/// `root = None` -- Win32-имя (`Local\`/`Global\`/NT-путь, см.
/// `ntapi::to_nt_path`); `Some(ns)` -- имя относительно частного
/// пространства `ns` (`OBJECT_ATTRIBUTES.RootDirectory`), проверяется при
/// разрешении: пустое или с `\` -- `InvalidConfig`.
pub(crate) struct ObjectPath<'a> {
    root: Option<&'a PrivateNamespace>,
    name: String,
}

impl<'a> ObjectPath<'a> {
    /// Объект `{base}{suffix}` канала/лобби/маяка `base`: в пространстве
    /// `ns` -- относительное имя как есть, иначе -- в namespace по правилам
    /// `scoped_name` (явный `Global\`/`Local\`/NT-путь как есть, иначе
    /// неявный `Local\`).
    pub(crate) fn scoped(ns: Option<&'a PrivateNamespace>, base: &str, suffix: &str) -> Self {
        let name = match ns {
            Some(_) => format!("{base}{suffix}"),
            None => scoped_name(base, suffix),
        };
        Self { root: ns, name }
    }

    /// Секция канала `base`.
    pub(crate) fn mapping(ns: Option<&'a PrivateNamespace>, base: &str) -> Self {
        Self::scoped(ns, base, "")
    }

    /// Событие канала `base`.
    pub(crate) fn event(
        ns: Option<&'a PrivateNamespace>,
        base: &str,
        direction: Direction,
        suffix: &str,
    ) -> Self {
        Self::scoped(ns, base, &event_suffix(direction, suffix))
    }

    /// Каталог (`None` -- общий `\BaseNamedObjects`).
    pub(crate) const fn root(&self) -> Option<&'a PrivateNamespace> {
        self.root
    }

    /// Имя (Win32 или относительное -- см. [`ObjectPath::root`]).
    pub(crate) fn name(&self) -> &str {
        &self.name
    }
}

impl From<&str> for ObjectPath<'_> {
    fn from(name: &str) -> Self {
        Self {
            root: None,
            name: name.to_owned(),
        }
    }
}

impl From<&String> for ObjectPath<'_> {
    fn from(name: &String) -> Self {
        Self::from(name.as_str())
    }
}

impl From<String> for ObjectPath<'_> {
    fn from(name: String) -> Self {
        Self { root: None, name }
    }
}

/// Явно заданный вызывающим namespace, который нельзя переопределять.
///
/// Без этой проверки имя `Global\Chan` превращалось в `Local\Global\Chan`, а
/// `to_nt_path` разворачивал его в `\Sessions\<id>\BaseNamedObjects\Global\Chan`
/// — несуществующий путь, из-за чего межсессионный IPC через `Global\`
/// (заявленный в README и в доке `to_nt_path`) не работал вообще.
pub(crate) fn has_explicit_namespace(base: &str) -> bool {
    base.starts_with("Global\\") || base.starts_with("Local\\") || base.starts_with('\\')
}

/// `{base}{suffix}` в namespace `base`: как есть при явном namespace
/// (`Global\...`, `Local\...`, NT-путь `\...`), иначе -- `Local\{base}{suffix}`.
/// Все объекты канала (секция, события, маяк, мьютекс лобби) попадают в ОДИН
/// namespace.
fn scoped_name(base: &str, suffix: &str) -> String {
    if has_explicit_namespace(base) {
        format!("{base}{suffix}")
    } else {
        format!("Local\\{base}{suffix}")
    }
}

/// Имя секции: `Local\{base}`, либо `base` как есть, если namespace задан явно
/// (`Global\...`, `Local\...`, либо готовый NT-путь `\...`).
#[cfg(test)]
fn mapping_name(base: &str) -> String {
    scoped_name(base, "")
}

/// Суффикс события: `_{S2C|C2S}_{suffix}`.
fn event_suffix(direction: Direction, suffix: &str) -> String {
    format!("_{}_{}", direction.as_str(), suffix)
}

/// Имя события канала. Namespace выбирается по тем же правилам, что и в
/// `mapping_name`, иначе секция и события канала оказались бы в РАЗНЫХ
/// namespace'ах.
#[cfg(test)]
fn event_name(base: &str, direction: Direction, suffix: &str) -> String {
    scoped_name(base, &event_suffix(direction, suffix))
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
