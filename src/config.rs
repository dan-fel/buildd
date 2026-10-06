//! Where buildd keeps its state and how much of the machine it uses.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The daemon's limits, from `config.toml` in its home.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    /// Builds that run at once. Each repository gets at most this many slot
    /// directories.
    pub slots: usize,
    /// Compiler jobs shared by all running builds.
    pub jobs: usize,
    /// Of those, what one build's tests take while they run: their threads
    /// and the processes they start use the machine without job tokens.
    pub test_jobs: usize,
    /// Disk one slot's target may keep between builds, in bytes. Build disk
    /// is at most this times `slots` per repository, plus what a running
    /// build adds before it is pruned.
    pub slot_limit: u64,
    /// Disk the volume holding the home keeps free, in bytes: below it,
    /// idle slots give up what builds used longest ago.
    pub min_free: u64,
    /// Memory running builds may use together, in bytes, by their recorded
    /// peaks: a build that would not fit waits.
    pub memory: u64,
    /// Other machines' daemons, for builds that must run on another OS.
    pub remotes: Vec<Remote>,
}

/// Another machine whose buildd daemon builds for `os`, reached over SSH.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Remote {
    /// What people see: `pc`.
    pub name: String,
    /// The SSH destination: `user@host`.
    pub ssh: String,
    /// The OS its builds run on, as Rust names it: `linux`, `macos`.
    pub os: String,
    /// The shell command that runs `buildd serve` there, with whatever
    /// environment it needs: `bash -lc 'buildd serve'`.
    pub command: String,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    slots: Option<usize>,
    jobs: Option<usize>,
    test_jobs: Option<usize>,
    slot_limit_gib: Option<f64>,
    min_free_gib: Option<f64>,
    memory_gib: Option<f64>,
    #[serde(default)]
    remote: Vec<Remote>,
}

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

impl Config {
    /// Reads `config.toml` in `home`. Absent settings take their defaults:
    /// two slots, one job per available CPU, half of them for a build's
    /// tests, 20 GiB per slot, 15 GiB of free disk, and the machine's memory
    /// less 6 GiB for builds.
    ///
    /// # Errors
    /// When the file cannot be read or parsed, or a limit is not positive.
    pub fn load(home: &Path) -> Result<Self, String> {
        let path = home.join("config.toml");
        let file = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str::<File>(&text)
                .map_err(|error| format!("{}: {error}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => File::default(),
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        let slot_limit_gib = file.slot_limit_gib.unwrap_or(20.0);
        if !(slot_limit_gib.is_finite() && slot_limit_gib > 0.0) {
            return Err(format!(
                "{}: slot_limit_gib must be positive",
                path.display()
            ));
        }
        #[expect(clippy::cast_precision_loss, reason = "memory in GiB")]
        let physical_gib = crate::memory::physical() as f64 / GIB;
        let memory_gib = file.memory_gib.unwrap_or((physical_gib - 6.0).max(1.0));
        if !(memory_gib.is_finite() && memory_gib > 0.0) {
            return Err(format!("{}: memory_gib must be positive", path.display()));
        }
        let min_free_gib = file.min_free_gib.unwrap_or(15.0);
        if !(min_free_gib.is_finite() && min_free_gib >= 0.0) {
            return Err(format!(
                "{}: min_free_gib must not be negative",
                path.display()
            ));
        }
        let jobs = file.jobs.unwrap_or_else(|| {
            std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
        });
        let config = Self {
            slots: file.slots.unwrap_or(2),
            jobs,
            test_jobs: file.test_jobs.unwrap_or((jobs / 2).max(1)),
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a positive, finite number of GiB rounds to bytes"
            )]
            slot_limit: (slot_limit_gib * GIB).round() as u64,
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a non-negative, finite number of GiB rounds to bytes"
            )]
            min_free: (min_free_gib * GIB).round() as u64,
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a positive, finite number of GiB rounds to bytes"
            )]
            memory: (memory_gib * GIB).round() as u64,
            remotes: file.remote,
        };
        if config.slots == 0 || config.jobs == 0 {
            return Err(format!(
                "{}: slots and jobs must be at least 1",
                path.display()
            ));
        }
        if let Some(remote) = config
            .remotes
            .iter()
            .find(|remote| remote.os == std::env::consts::OS)
        {
            return Err(format!(
                "{}: remote {} builds for {}, this machine's own OS",
                path.display(),
                remote.name,
                remote.os
            ));
        }
        if !(1..=config.jobs).contains(&config.test_jobs) {
            return Err(format!(
                "{}: test_jobs must be between 1 and jobs",
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
    fn settings_default_and_reject_nonpositive_and_unknown_keys() {
        let home = TempDir::new();
        let defaults = Config::load(&home.0).unwrap();
        assert_eq!(defaults.slots, 2);
        assert!(defaults.jobs >= 1);
        assert_eq!(defaults.slot_limit, 20 << 30);
        let write = |text: &str| std::fs::write(home.0.join("config.toml"), text).unwrap();
        assert_eq!(defaults.test_jobs, (defaults.jobs / 2).max(1));
        assert_eq!(defaults.min_free, 15 << 30);
        assert!(defaults.memory >= 1 << 30);
        write("slots = 3\njobs = 6\nslot_limit_gib = 0.5\nmin_free_gib = 0\nmemory_gib = 8\n");
        let expected = Config {
            slots: 3,
            jobs: 6,
            test_jobs: 3,
            slot_limit: 1 << 29,
            min_free: 0,
            memory: 8 << 30,
            remotes: Vec::new(),
        };
        assert_eq!(Config::load(&home.0).unwrap(), expected);
        let other = if std::env::consts::OS == "linux" {
            "macos"
        } else {
            "linux"
        };
        write(&format!(
            "[[remote]]\nname = \"pc\"\nssh = \"me@pc\"\nos = \"{other}\"\ncommand = \"buildd serve\"\n"
        ));
        assert_eq!(Config::load(&home.0).unwrap().remotes[0].name, "pc");
        write(&format!(
            "[[remote]]\nname = \"self\"\nssh = \"me@here\"\nos = \"{}\"\ncommand = \"buildd serve\"\n",
            std::env::consts::OS
        ));
        assert!(Config::load(&home.0).is_err(), "a remote for this OS");
        write("slot_limit_gib = 12\n");
        assert_eq!(Config::load(&home.0).unwrap().slot_limit, 12 << 30);
        write("jobs = 6\ntest_jobs = 6\n");
        assert_eq!(Config::load(&home.0).unwrap().test_jobs, 6);
        for invalid in [
            "slots = 0\n",
            "slot_limit_gib = 0\n",
            "slot = 1\n",
            "jobs = 4\ntest_jobs = 5\n",
            "test_jobs = 0\n",
            "min_free_gib = -1\n",
            "memory_gib = 0\n",
        ] {
            write(invalid);
            assert!(Config::load(&home.0).is_err(), "{invalid}");
        }
    }
}
