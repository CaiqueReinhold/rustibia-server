use crate::game::TickDelta;

#[derive(Clone, Debug)]
pub struct Voices {
    pub cooldown: TickDelta,
    pub chance: u32,
    pub sentences: Vec<String>,
}
