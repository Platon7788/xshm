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

fn event_prefix(base: &str) -> String {
    format!("Local\\XSHM_{base}_")
}

pub fn mapping_name(base: &str) -> String {
    format!("Local\\XSHM_SEG_{base}")
}

pub fn event_name(base: &str, direction: Direction, suffix: &str) -> String {
    format!("{}{}_{}", event_prefix(base), direction.as_str(), suffix)
}
