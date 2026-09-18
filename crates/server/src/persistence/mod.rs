use std::path::{Path, PathBuf};
use std::{fs, io};

pub mod areas;
pub mod creatures;
pub mod items;
pub mod login;
pub mod map;
pub mod online;
pub mod player;
pub mod spawns;
pub mod spells;
pub mod support;
pub mod target_mode;

#[cfg(test)]
pub mod test_fixtures;

fn yaml_files_in(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut paths = fs::read_dir(dir)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<io::Result<Vec<_>>>()?;
    paths.retain(|path| {
        path.is_file()
            && matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("yaml" | "yml")
            )
    });
    paths.sort();
    Ok(paths)
}
