//! Each character's latest save not yet acknowledged by the site, one file per character.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use rustibia_contract::CharacterSave;
use tracing::warn;

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

    pub fn write(&self, save: &CharacterSave) -> io::Result<()> {
        let final_path = self.path(save.id);
        let temp_path = final_path.with_extension("json.tmp");
        let mut file = File::create(&temp_path)?;
        serde_json::to_writer(&mut file, save).map_err(io::Error::other)?;
        file.sync_all()?;
        fs::rename(&temp_path, &final_path)?;
        File::open(&self.dir)?.sync_all()
    }

    /// Deletes `id`'s save only if it is `version` or older.
    pub fn remove_if_version(&self, id: i32, version: i64) -> io::Result<()> {
        let path = self.path(id);
        match read(&path) {
            Ok(save) if save.save_version <= version => fs::remove_file(&path),
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub fn pending(&self) -> io::Result<Vec<CharacterSave>> {
        let mut pending = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            match read(&path) {
                Ok(save) => pending.push(save),
                Err(e) => warn!(path = %path.display(), "skipping an unreadable journal entry: {e}"),
            }
        }
        Ok(pending)
    }

    fn path(&self, id: i32) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }
}

fn read(path: &Path) -> io::Result<CharacterSave> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use rustibia_contract::{Coords, Outfit, PoolValue};

    use super::*;

    fn a_save(id: i32, save_version: i64) -> CharacterSave {
        CharacterSave {
            id,
            save_version,
            position: Coords { x: 1, y: 2, z: 7 },
            origin: Coords { x: 1, y: 2, z: 7 },
            facing: 0,
            life: PoolValue { current: 1, maximum: 1 },
            mana: PoolValue { current: 1, maximum: 1 },
            capacity: 1,
            speed: 1,
            outfit: Outfit { id: 1, head: 0, body: 0, legs: 0, feet: 0 },
            skills: Vec::new(),
            inventory: HashMap::new(),
        }
    }

    fn versions(journal: &Journal) -> Vec<(i32, i64)> {
        let mut pending: Vec<(i32, i64)> = journal
            .pending()
            .unwrap()
            .into_iter()
            .map(|save| (save.id, save.save_version))
            .collect();
        pending.sort();
        pending
    }

    #[test]
    fn a_write_replaces_the_older_save() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path()).unwrap();

        journal.write(&a_save(7, 1)).unwrap();
        journal.write(&a_save(7, 2)).unwrap();
        journal.write(&a_save(8, 1)).unwrap();

        assert_eq!(versions(&journal), vec![(7, 2), (8, 1)]);
    }

    #[test]
    fn removing_an_acknowledged_version_keeps_a_newer_one() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path()).unwrap();
        journal.write(&a_save(7, 3)).unwrap();

        journal.remove_if_version(7, 2).unwrap();
        assert_eq!(versions(&journal), vec![(7, 3)]);

        journal.remove_if_version(7, 3).unwrap();
        assert!(versions(&journal).is_empty());
    }

    #[test]
    fn removing_a_character_with_nothing_journaled_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path()).unwrap();

        journal.remove_if_version(7, 1).unwrap();
    }

    #[test]
    fn opening_discards_a_write_that_never_finished() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("7.json.tmp"), b"{\"id\": 7, \"save_ver").unwrap();

        let journal = Journal::open(dir.path()).unwrap();

        assert!(journal.pending().unwrap().is_empty());
        assert!(!dir.path().join("7.json.tmp").exists());
    }

    #[test]
    fn opening_creates_a_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("journal");

        let journal = Journal::open(&nested).unwrap();
        journal.write(&a_save(7, 1)).unwrap();

        assert_eq!(versions(&journal), vec![(7, 1)]);
    }
}
