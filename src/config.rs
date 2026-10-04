//! Where buildd keeps its state and how much of the machine it uses.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The daemon's limits, from `config.toml` in its home.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Config {
    /// Builds that run at once. Each repository gets at most this many slot
    /// directories, so this also bounds build disk.
    pub slots: usize,
    /// Compiler jobs shared by all running builds.
    pub jobs: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    slots: Option<usize>,
    jobs: Option<usize>,
}

impl Config {
    /// Reads `config.toml` in `home`. Absent settings take their defaults:
    /// two slots, and one job per available CPU.
    ///
    /// # Errors
    /// When the file cannot be read or parsed, or a limit is zero.
    pub fn load(home: &Path) -> Result<Self, String> {
        let path = home.join("config.toml");
        let file = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str::<File>(&text)
                .map_err(|error| format!("{}: {error}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => File {
                slots: None,
                jobs: None,
            },
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        let config = Self {
            slots: file.slots.unwrap_or(2),
            jobs: file.jobs.unwrap_or_else(|| {
                std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
            }),
        };
        if config.slots == 0 || config.jobs == 0 {
            return Err(format!(
                "{}: slots and jobs must be at least 1",
                path.display()
            ));
        }
        Ok(config)
    }
}

/// buildd's home: `$BUILDD_HOME`, or `buildd` in the user's cache directory.
///
/// # Errors
/// When `$BUILDD_HOME` is relative, or the user has no home directory.
pub fn home() -> Result<PathBuf, String> {
    if let Some(home) = std::env::var_os("BUILDD_HOME") {
        let home = PathBuf::from(home);
        if !home.is_absolute() {
            return Err(format!("BUILDD_HOME must be absolute: {}", home.display()));
        }
        return Ok(home);
    }
    directories::BaseDirs::new()
        .map(|directories| directories.cache_dir().join("buildd"))
        .ok_or_else(|| "the user has no home directory".to_owned())
}

/// The daemon's socket in `home`.
#[must_use]
pub fn socket(home: &Path) -> PathBuf {
    home.join("sock")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::tests::TempDir;

    #[test]
    fn settings_default_and_reject_zero_and_unknown_keys() {
        let home = TempDir::new();
        let defaults = Config::load(&home.0).unwrap();
        assert_eq!(defaults.slots, 2);
        assert!(defaults.jobs >= 1);
        std::fs::write(home.0.join("config.toml"), "slots = 3\njobs = 6\n").unwrap();
        assert_eq!(Config::load(&home.0).unwrap(), Config { slots: 3, jobs: 6 });
        std::fs::write(home.0.join("config.toml"), "slots = 0\n").unwrap();
        assert!(Config::load(&home.0).is_err());
        std::fs::write(home.0.join("config.toml"), "slot = 1\n").unwrap();
        assert!(Config::load(&home.0).is_err());
    }
}
