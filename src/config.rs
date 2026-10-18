//! Where buildd keeps its state and how much of the machine it uses.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The daemon's limits, from `config.toml` in its home.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    /// Builds that run at once. New slot directories are created up to this
    /// count per repository; caches from a larger earlier limit are retained.
    pub slots: usize,
    /// Compiler jobs shared by all running builds.
    pub jobs: usize,
    /// Of those, what one build's tests take while they run: their threads
    /// and the processes they start use the machine without job tokens.
    pub test_jobs: usize,
    /// Disk the targets of all slots, of every repository, may keep together
    /// between builds, in bytes, plus what running builds add before their
    /// slots are measured.
    pub disk: u64,
    /// Disk the volume holding the home keeps free, in bytes. Breaching
    /// it stops running builds before their idle artifacts are reclaimed.
    pub min_free: u64,
    /// Free space reserved for each running build's additional output,
    /// beyond the volume's free-disk floor.
    pub build_headroom: u64,
    /// Memory running builds may use together, in bytes, by their recorded
    /// peaks: a build that would not fit waits.
    pub memory: u64,
    /// Reservation for an unmeasured compilation. None learns exclusively,
    /// reserving the whole budget. This is admission policy, not a measured
    /// peak or a process memory limit.
    pub unmeasured_memory: Option<u64>,
    /// Other machines eligible for automatic or explicit build placement.
    pub remotes: Vec<Remote>,
    /// Branches whose new commits are built before anyone asks.
    pub prewarm: Vec<Prewarm>,
}

/// A branch whose new commits buildd builds as optional work, so the
/// worktrees branching from it find warm slots.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prewarm {
    /// Any worktree of the repository.
    pub repository: PathBuf,
    /// The branch to follow: `main`.
    pub branch: String,
    /// The OS to build on, as `--os` names it; this machine when absent.
    #[serde(default)]
    pub os: Option<String>,
    /// The builds, each a buildd command and Cargo's arguments:
    /// `["test", "--workspace", "--no-run"]`.
    pub builds: Vec<Vec<String>>,
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
    disk_gib: Option<f64>,
    min_free_gib: Option<f64>,
    build_headroom_gib: Option<f64>,
    memory_gib: Option<f64>,
    unmeasured_memory_gib: Option<f64>,
    #[serde(default)]
    remote: Vec<Remote>,
    #[serde(default)]
    prewarm: Vec<Prewarm>,
}

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
/// 2^64 bytes is exactly representable as f64, unlike u64::MAX.
const MAX_GIB: f64 = 18_446_744_073_709_551_616.0 / GIB;

impl Config {
    /// Reads `config.toml` in `home`. Absent settings take their defaults:
    /// two slots, one job per available CPU, half of them for a build's
    /// tests, 20 GiB of build disk per slot, 15 GiB of free disk, 10 GiB of
    /// output headroom per running build, and the
    /// machine's memory less 6 GiB for builds.
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
        let slots = file.slots.unwrap_or(2);
        #[expect(clippy::cast_precision_loss, reason = "a slot count in GiB")]
        let disk_gib = file.disk_gib.unwrap_or(20.0 * slots as f64);
        if !(disk_gib.is_finite() && (0.5 / GIB..MAX_GIB).contains(&disk_gib)) {
            return Err(format!(
                "{}: disk_gib must be finite, positive and fit in bytes",
                path.display()
            ));
        }
        #[expect(clippy::cast_precision_loss, reason = "memory in GiB")]
        let physical_gib = crate::memory::physical() as f64 / GIB;
        let memory_gib = file.memory_gib.unwrap_or((physical_gib - 6.0).max(1.0));
        if !(memory_gib.is_finite() && (0.5 / GIB..MAX_GIB).contains(&memory_gib)) {
            return Err(format!(
                "{}: memory_gib must be finite, positive and fit in bytes",
                path.display()
            ));
        }
        let min_free_gib = file.min_free_gib.unwrap_or(15.0);
        if file
            .unmeasured_memory_gib
            .is_some_and(|gib| !gib.is_finite() || gib < 0.5 / GIB || gib > memory_gib)
        {
            return Err(format!(
                "{}: unmeasured_memory_gib must be finite, positive and no greater than memory_gib",
                path.display()
            ));
        }
        if !(min_free_gib.is_finite() && (0.0..MAX_GIB).contains(&min_free_gib)) {
            return Err(format!(
                "{}: min_free_gib must be finite, non-negative and fit in bytes",
                path.display()
            ));
        }
        let build_headroom_gib = file.build_headroom_gib.unwrap_or(10.0);
        if !(build_headroom_gib.is_finite() && (0.0..MAX_GIB).contains(&build_headroom_gib)) {
            return Err(format!(
                "{}: build_headroom_gib must be finite, non-negative and fit in bytes",
                path.display()
            ));
        }
        let jobs = file.jobs.unwrap_or_else(|| {
            std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
        });
        let config = Self {
            slots,
            jobs,
            test_jobs: file.test_jobs.unwrap_or((jobs / 2).max(1)),
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a positive, finite number of GiB rounds to bytes"
            )]
            disk: (disk_gib * GIB).round() as u64,
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a non-negative, finite number of GiB rounds to bytes"
            )]
            min_free: (min_free_gib * GIB).round() as u64,
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a non-negative, finite number of GiB rounds to bytes"
            )]
            build_headroom: (build_headroom_gib * GIB).round() as u64,
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a positive, finite number of GiB rounds to bytes"
            )]
            memory: (memory_gib * GIB).round() as u64,
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a positive, finite reservation no larger than the memory budget"
            )]
            unmeasured_memory: file
                .unmeasured_memory_gib
                .map(|gib| (gib * GIB).round() as u64),
            remotes: file.remote,
            prewarm: file.prewarm,
        };
        if config.slots == 0 || config.jobs == 0 {
            return Err(format!(
                "{}: slots and jobs must be at least 1",
                path.display()
            ));
        }
        let mut names = std::collections::HashSet::new();
        for remote in &config.remotes {
            if remote.name.is_empty() || remote.name == "local" || !names.insert(&remote.name) {
                return Err(format!(
                    "{}: remote names must be unique, nonempty and different from local",
                    path.display()
                ));
            }
            if remote.ssh.is_empty()
                || remote.ssh.starts_with('-')
                || remote.command.is_empty()
                || remote.os.is_empty()
            {
                return Err(format!(
                    "{}: remote {} needs an SSH destination, OS and command",
                    path.display(),
                    remote.name
                ));
            }
        }
        if !(1..=config.jobs).contains(&config.test_jobs) {
            return Err(format!(
                "{}: test_jobs must be between 1 and jobs",
                path.display()
            ));
        }
        for prewarm in &config.prewarm {
            for build in &prewarm.builds {
                prewarm_operation(build)
                    .map_err(|reason| format!("{}: prewarm {build:?}: {reason}", path.display()))?;
            }
        }
        Ok(config)
    }
}

/// The operation a prewarm build names: a buildd command and Cargo's
/// arguments.
///
/// # Errors
/// When it is empty, names no command, or takes over the slot.
pub fn prewarm_operation(build: &[String]) -> Result<crate::cargo::Operation, String> {
    let (command, args) = build.split_first().ok_or("an empty build")?;
    let command = crate::cargo::Command::parse(command)
        .filter(|command| command.uses_cargo())
        .ok_or_else(|| format!("unknown command `{command}`: prewarming runs Cargo"))?;
    let operation = crate::cargo::Operation {
        command,
        args: args.to_vec(),
        rustflags: Vec::new(),
    };
    operation.validate()?;
    Ok(operation)
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
        assert_eq!(defaults.disk, 40 << 30);
        let write = |text: &str| std::fs::write(home.0.join("config.toml"), text).unwrap();
        assert_eq!(defaults.test_jobs, (defaults.jobs / 2).max(1));
        assert_eq!(defaults.min_free, 15 << 30);
        assert_eq!(defaults.build_headroom, 10 << 30);
        assert!(defaults.memory >= 1 << 30);
        write("slots = 3\njobs = 6\ndisk_gib = 0.5\nmin_free_gib = 0\nmemory_gib = 8\n");
        let expected = Config {
            slots: 3,
            jobs: 6,
            test_jobs: 3,
            disk: 1 << 29,
            min_free: 0,
            build_headroom: 10 << 30,
            memory: 8 << 30,
            unmeasured_memory: None,
            remotes: Vec::new(),
            prewarm: Vec::new(),
        };
        assert_eq!(Config::load(&home.0).unwrap(), expected);
        write("memory_gib = 8\nunmeasured_memory_gib = 2\n");
        assert_eq!(
            Config::load(&home.0).unwrap().unmeasured_memory,
            Some(2 << 30)
        );
        write(
            "[[prewarm]]\nrepository = \"/work/app\"\nbranch = \"main\"\nos = \"linux\"\n\
             builds = [[\"test\", \"--workspace\", \"--no-run\"]]\n",
        );
        let prewarm = &Config::load(&home.0).unwrap().prewarm[0];
        assert_eq!(prewarm.branch, "main");
        assert_eq!(prewarm.os.as_deref(), Some("linux"));
        assert_eq!(
            prewarm_operation(&prewarm.builds[0]).unwrap().command,
            crate::cargo::Command::Test
        );
        for refused in [
            "builds = [[]]",
            "builds = [[\"frobnicate\"]]",
            "builds = [[\"test\", \"--target-dir\", \"x\"]]",
        ] {
            write(&format!(
                "[[prewarm]]\nrepository = \"/work/app\"\nbranch = \"main\"\n{refused}\n"
            ));
            assert!(Config::load(&home.0).is_err(), "{refused}");
        }
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
            "[[remote]]\nname = \"air\"\nssh = \"me@air\"\nos = \"{}\"\ncommand = \"buildd serve\"\n",
            std::env::consts::OS
        ));
        assert_eq!(Config::load(&home.0).unwrap().remotes[0].name, "air");
        for name in ["", "local"] {
            write(&format!(
                "[[remote]]\nname = {name:?}\nssh = \"me@host\"\nos = \"macos\"\ncommand = \"buildd serve\"\n"
            ));
            assert!(Config::load(&home.0).is_err());
        }
        write("slots = 3\n");
        assert_eq!(Config::load(&home.0).unwrap().disk, 60 << 30);
        write("disk_gib = 12\n");
        assert_eq!(Config::load(&home.0).unwrap().disk, 12 << 30);
        write("jobs = 6\ntest_jobs = 6\n");
        assert_eq!(Config::load(&home.0).unwrap().test_jobs, 6);
        for invalid in [
            "slots = 0\n",
            "disk_gib = 0\n",
            "disk_gib = 1e-100\n",
            "memory_gib = 1e-100\n",
            "slot_limit_gib = 12\n",
            "slot = 1\n",
            "jobs = 4\ntest_jobs = 5\n",
            "test_jobs = 0\n",
            "min_free_gib = -1\n",
            "build_headroom_gib = -1\n",
            "build_headroom_gib = nan\n",
            "build_headroom_gib = inf\n",
            "build_headroom_gib = 17179869184\n",
            "disk_gib = 17179869184\n",
            "memory_gib = 17179869184\n",
            "min_free_gib = 17179869184\n",
            "memory_gib = 0\n",
            "unmeasured_memory_gib = 0\n",
            "unmeasured_memory_gib = 1e-100\n",
            "unmeasured_memory_gib = nan\n",
            "unmeasured_memory_gib = inf\n",
            "memory_gib = 8\nunmeasured_memory_gib = 9\n",
        ] {
            write(invalid);
            assert!(Config::load(&home.0).is_err(), "{invalid}");
        }
    }
}
