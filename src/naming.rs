#[derive(Clone, Copy)]
pub enum Direction {
    ServerToClient,
    ClientToServer,
}

impl Direction {
    fn as_str(&self) -> &'static str {
        match self {
            Direction::ServerToClient => "S2C",
            Direction::ClientToServer => "C2S",
        }
    }
}

// Примечание: имена объектов эмитятся как есть (без префикса `XSHM_SEG_` / `XSHM_`) --
// пространство имён полностью контролирует встраивающий код. Вызывающая сторона
// должна сама гарантировать, что base не конфликтует с другими объектами Local\ в сессии.
fn event_prefix(base: &str) -> String {
    format!("Local\\{base}_")
}

pub fn mapping_name(base: &str) -> String {
    format!("Local\\{base}")
}

pub fn event_name(base: &str, direction: Direction, suffix: &str) -> String {
    format!("{}{}_{}", event_prefix(base), direction.as_str(), suffix)
}
