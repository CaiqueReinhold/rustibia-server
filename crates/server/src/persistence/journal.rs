//! What the site has not yet acknowledged: each logged-out player's final state since the last
//! world save, and the world save being delivered.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use rustibia_contract::{CharacterSave, WorldSave};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tracing::warn;

const WORLD_FILE: &str = "world.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournaledPlayer {
    pub tick: u64,
    pub save: CharacterSave,
}

pub struct Journal {
    dir: PathBuf,
}

impl Journal {
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "tmp") {
                fs::remove_file(&path)?;
            }
        }
        Ok(Self { dir })
    }

    pub fn write_player(&self, player: &JournaledPlayer) -> io::Result<()> {
        self.write(&self.dir.join(format!("{}.json", player.save.id)), player)
    }

    /// Deletes `id`'s entry only if it was journaled at `tick` or earlier.
    pub fn remove_player_up_to(&self, id: i32, tick: u64) -> io::Result<()> {
        let path = self.dir.join(format!("{id}.json"));
        match read::<JournaledPlayer>(&path) {
            Ok(player) if player.tick <= tick => fs::remove_file(&path),
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub fn players(&self) -> io::Result<Vec<JournaledPlayer>> {
        let mut players = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            let is_player = path.extension().is_some_and(|ext| ext == "json")
                && path.file_name().is_some_and(|name| name != WORLD_FILE);
            if !is_player {
                continue;
            }
            match read(&path) {
                Ok(player) => players.push(player),
                Err(e) => {
                    warn!(path = %path.display(), "skipping an unreadable journal entry: {e}")
                }
            }
        }
        Ok(players)
    }

    pub fn write_world(&self, save: &WorldSave) -> io::Result<()> {
        self.write(&self.dir.join(WORLD_FILE), save)
    }

    pub fn world(&self) -> io::Result<Option<WorldSave>> {
        match read(&self.dir.join(WORLD_FILE)) {
            Ok(save) => Ok(Some(save)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn remove_world(&self) -> io::Result<()> {
        match fs::remove_file(self.dir.join(WORLD_FILE)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }

    fn write(&self, path: &Path, value: &impl Serialize) -> io::Result<()> {
        let temp_path = path.with_extension("json.tmp");
        let mut file = File::create(&temp_path)?;
        serde_json::to_writer(&mut file, value).map_err(io::Error::other)?;
        file.sync_all()?;
        fs::rename(&temp_path, path)?;
        File::open(&self.dir)?.sync_all()
    }
}

fn read<T: DeserializeOwned>(path: &Path) -> io::Result<T> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;

    use rustibia_contract::{Coords, Outfit, PoolValue};

    use super::*;

    pub(crate) fn a_save(id: i32) -> CharacterSave {
        CharacterSave {
            id,
            position: Coords { x: 1, y: 2, z: 7 },
            origin: Coords { x: 1, y: 2, z: 7 },
            facing: 0,
            life: PoolValue {
                current: 1,
                maximum: 1,
            },
            mana: PoolValue {
                current: 1,
                maximum: 1,
            },
            capacity: 1,
            speed: 1,
            outfit: Outfit {
                id: 1,
                head: 0,
                body: 0,
                legs: 0,
                feet: 0,
            },
            skills: Vec::new(),
            inventory: HashMap::new(),
        }
    }

    fn player(id: i32, tick: u64) -> JournaledPlayer {
        JournaledPlayer {
            tick,
            save: a_save(id),
        }
    }

    fn ticks(journal: &Journal) -> Vec<(i32, u64)> {
        let mut ticks: Vec<_> = journal
            .players()
            .unwrap()
            .iter()
            .map(|p| (p.save.id, p.tick))
            .collect();
        ticks.sort();
        ticks
    }

    #[test]
    fn a_later_logout_replaces_the_earlier_one() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path()).unwrap();

        journal.write_player(&player(7, 1)).unwrap();
        journal.write_player(&player(7, 5)).unwrap();
        journal.write_player(&player(8, 2)).unwrap();

        assert_eq!(ticks(&journal), vec![(7, 5), (8, 2)]);
    }

    #[test]
    fn removing_up_to_a_tick_keeps_a_later_logout() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path()).unwrap();
        journal.write_player(&player(7, 9)).unwrap();

        journal.remove_player_up_to(7, 8).unwrap();
        assert_eq!(ticks(&journal), vec![(7, 9)]);

        journal.remove_player_up_to(7, 9).unwrap();
        assert!(ticks(&journal).is_empty());
    }

    #[test]
    fn the_world_save_is_kept_apart_from_the_players() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path()).unwrap();
        let save = WorldSave {
            tick: 3,
            characters: vec![a_save(7)],
            chunks: Vec::new(),
        };

        journal.write_world(&save).unwrap();

        assert_eq!(journal.world().unwrap(), Some(save));
        assert!(journal.players().unwrap().is_empty());
        journal.remove_world().unwrap();
        assert_eq!(journal.world().unwrap(), None);
    }

    #[test]
    fn opening_discards_a_write_that_never_finished() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("7.json.tmp"), b"{\"tick\": 7, \"sa").unwrap();

        let journal = Journal::open(dir.path()).unwrap();

        assert!(journal.players().unwrap().is_empty());
        assert!(!dir.path().join("7.json.tmp").exists());
    }
}
