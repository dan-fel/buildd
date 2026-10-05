//! How far apart two trees of a repository are, measured as the work Cargo
//! redoes when a slot holding one checks out the other.
//!
//! The changed paths come from `git diff-tree`. Once the repository's
//! workspace is known, a changed package costs every workspace package that
//! has to be compiled again: itself and all packages that depend on it,
//! directly or not. A change to the lockfile or the workspace's build
//! configuration costs every package. Until then, every changed path costs
//! one.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::Deserialize;

use crate::git;
use crate::scheduler::Distance;
use crate::snapshot::Revision;

/// How many distances [`GitDistance`] remembers before it starts over.
const KNOWN_DISTANCES: usize = 4096;

/// Distances between trees from `git diff-tree`, weighed by each
/// repository's workspace once it is known, remembered per pair of trees.
#[derive(Default)]
pub(crate) struct GitDistance {
    known: HashMap<(PathBuf, Revision, Revision), u64>,
    workspaces: HashMap<PathBuf, Workspace>,
}

impl GitDistance {
    /// Weighs distances in `repository` by `workspace` from now on.
    pub(crate) fn learn(&mut self, repository: PathBuf, workspace: Workspace) {
        if self.workspaces.get(&repository) != Some(&workspace) {
            self.known.retain(|(known, ..), _| *known != repository);
            self.workspaces.insert(repository, workspace);
        }
    }
}

impl Distance for GitDistance {
    fn distance(&mut self, repository: &Path, from: &Revision, to: &Revision) -> u64 {
        if from == to {
            return 0;
        }
        let (low, high) = if from < to { (from, to) } else { (to, from) };
        let key = (repository.to_owned(), low.clone(), high.clone());
        if let Some(distance) = self.known.get(&key) {
            return *distance;
        }
        let mut command = git::command(repository);
        command
            .arg("--git-dir")
            .arg(repository)
            .args(["diff-tree", "-r", "--name-only", "--no-renames"])
            .arg(low.to_string())
            .arg(high.to_string());
        let paths = match git::run(command) {
            Ok(paths) => paths,
            // An unknown distance ranks behind every known one.
            Err(error) => {
                eprintln!("buildd: could not compare trees: {error}");
                return u64::MAX;
            }
        };
        let distance = match self.workspaces.get(repository) {
            Some(workspace) => workspace.weigh(paths.lines()),
            None => paths.lines().count() as u64,
        };
        if self.known.len() >= KNOWN_DISTANCES {
            self.known.clear();
        }
        self.known.insert(key, distance);
        distance
    }
}

/// A Cargo workspace at the top of a repository: each package's directory,
/// relative to the top, and the packages compiled again when it changes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Workspace {
    /// Package directories, longest first, so the first one containing a
    /// path owns it.
    directories: Vec<(PathBuf, usize)>,
    /// For each package, itself and every package depending on it.
    rebuilds: Vec<Vec<usize>>,
}

impl Workspace {
    /// The workspace whose root is `root`, from `cargo metadata`; None when
    /// `root` holds no workspace.
    pub(crate) fn read(root: &Path) -> Result<Option<Self>, String> {
        let manifest = root.join("Cargo.toml");
        if !manifest.exists() {
            return Ok(None);
        }
        let output = std::process::Command::new("cargo")
            .args([
                "metadata",
                "--no-deps",
                "--offline",
                "--format-version",
                "1",
            ])
            .arg("--manifest-path")
            .arg(&manifest)
            .current_dir(root)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| format!("could not run cargo metadata: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "cargo metadata failed in {}: {}",
                root.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let text = String::from_utf8(output.stdout)
            .map_err(|_| "cargo metadata printed output that is not UTF-8".to_owned())?;
        Self::from_metadata(&text, root).map(Some)
    }

    /// The workspace `cargo metadata --no-deps` described as `json`, for a
    /// workspace whose root is `root`.
    pub(crate) fn from_metadata(json: &str, root: &Path) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct Metadata {
            packages: Vec<Package>,
        }
        #[derive(Deserialize)]
        struct Package {
            manifest_path: PathBuf,
            dependencies: Vec<Dependency>,
        }
        #[derive(Deserialize)]
        struct Dependency {
            path: Option<PathBuf>,
        }
        let metadata = serde_json::from_str::<Metadata>(json).map_err(|error| {
            format!("cargo metadata printed an unexpected description: {error}")
        })?;
        let directories = metadata
            .packages
            .iter()
            .map(|package| {
                package
                    .manifest_path
                    .parent()
                    .expect("a manifest lives in a directory")
                    .to_owned()
            })
            .collect::<Vec<_>>();
        let index = directories
            .iter()
            .enumerate()
            .map(|(index, directory)| (directory.as_path(), index))
            .collect::<HashMap<_, _>>();
        // dependents[a] lists the packages that depend on a directly.
        let mut dependents = vec![Vec::new(); directories.len()];
        for (dependent, package) in metadata.packages.iter().enumerate() {
            for dependency in &package.dependencies {
                if let Some(&depended) = dependency.path.as_deref().and_then(|path| index.get(path))
                {
                    dependents[depended].push(dependent);
                }
            }
        }
        let rebuilds = (0..directories.len())
            .map(|package| {
                let mut seen = HashSet::from([package]);
                let mut pending = vec![package];
                while let Some(next) = pending.pop() {
                    for &dependent in &dependents[next] {
                        if seen.insert(dependent) {
                            pending.push(dependent);
                        }
                    }
                }
                let mut rebuilt = seen.into_iter().collect::<Vec<_>>();
                rebuilt.sort_unstable();
                rebuilt
            })
            .collect();
        let mut relative = directories
            .into_iter()
            .enumerate()
            .map(|(package, directory)| {
                let directory = directory
                    .strip_prefix(root)
                    .map_or_else(|_| directory.clone(), Path::to_path_buf);
                (directory, package)
            })
            .collect::<Vec<_>>();
        relative.sort_by_key(|(directory, _)| std::cmp::Reverse(directory.as_os_str().len()));
        Ok(Self {
            directories: relative,
            rebuilds,
        })
    }

    /// What changing `paths`, relative to the top of the repository, costs:
    /// the packages compiled again, plus one for each changed path outside
    /// every package.
    pub(crate) fn weigh<'a>(&self, paths: impl IntoIterator<Item = &'a str>) -> u64 {
        let mut rebuilt = HashSet::new();
        let mut outside = 0;
        for path in paths {
            if is_build_configuration(path) {
                rebuilt.extend(0..self.rebuilds.len());
                continue;
            }
            let owner = self
                .directories
                .iter()
                .find(|(directory, _)| Path::new(path).starts_with(directory));
            match owner {
                Some((_, package)) => rebuilt.extend(&self.rebuilds[*package]),
                None => outside += 1,
            }
        }
        rebuilt.len() as u64 + outside
    }
}

/// Whether a change to `path` can change how every package compiles: the
/// lockfile, the workspace manifest, Cargo's configuration, the toolchain.
fn is_build_configuration(path: &str) -> bool {
    matches!(path, "Cargo.lock" | "Cargo.toml")
        || path.starts_with(".cargo/")
        || path.starts_with("rust-toolchain")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A workspace at /w: `domain` is used by `engine` and `gui`, and `gui`
    /// by `app`; `leaf` stands alone.
    fn workspace() -> Workspace {
        let package = |name: &str, dependencies: &[&str]| {
            serde_json::json!({
                "name": name,
                "manifest_path": format!("/w/crates/{name}/Cargo.toml"),
                "dependencies": dependencies
                    .iter()
                    .map(|dependency| serde_json::json!({
                        "name": dependency,
                        "path": format!("/w/crates/{dependency}"),
                    }))
                    .chain(std::iter::once(serde_json::json!({ "name": "serde", "path": null })))
                    .collect::<Vec<_>>(),
            })
        };
        let metadata = serde_json::json!({
            "packages": [
                package("domain", &[]),
                package("engine", &["domain"]),
                package("gui", &["domain"]),
                package("app", &["gui", "engine"]),
                package("leaf", &[]),
                package("gui-extra", &[]),
            ]
        });
        Workspace::from_metadata(&metadata.to_string(), Path::new("/w")).unwrap()
    }

    #[test]
    fn a_change_costs_the_packages_compiled_again() {
        let workspace = workspace();
        assert_eq!(workspace.weigh(["crates/leaf/src/lib.rs"]), 1);
        assert_eq!(workspace.weigh(["crates/app/src/main.rs"]), 1);
        assert_eq!(
            workspace.weigh(["crates/gui/src/a.rs", "crates/gui/src/b.rs"]),
            2
        );
        // domain, engine, gui and app.
        assert_eq!(workspace.weigh(["crates/domain/src/lib.rs"]), 4);
        assert_eq!(
            workspace.weigh(["crates/domain/src/lib.rs", "crates/gui/src/a.rs"]),
            4
        );
        // The longest directory owns a path: gui-extra is not gui.
        assert_eq!(workspace.weigh(["crates/gui-extra/src/lib.rs"]), 1);
        assert_eq!(workspace.weigh(["README.md", "docs/x.md"]), 2);
        assert_eq!(workspace.weigh(["Cargo.lock"]), 6);
        assert_eq!(workspace.weigh([".cargo/config.toml", "README.md"]), 7);
    }

    #[test]
    fn a_learned_workspace_changes_only_its_repositorys_distances() {
        let mut distance = GitDistance::default();
        let key = |repository: &str| {
            (
                PathBuf::from(repository),
                Revision::of_tree("a"),
                Revision::of_tree("b"),
            )
        };
        distance.known.insert(key("/one"), 1);
        distance.known.insert(key("/two"), 2);
        distance.learn("/one".into(), workspace());
        assert!(!distance.known.contains_key(&key("/one")));
        assert_eq!(distance.known.get(&key("/two")), Some(&2));
        // Learning the same workspace again keeps what is known.
        distance.known.insert(key("/one"), 3);
        distance.learn("/one".into(), workspace());
        assert_eq!(distance.known.get(&key("/one")), Some(&3));
    }
}
