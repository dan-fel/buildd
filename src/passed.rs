//! Test binaries that passed, and which of them a nextest run may skip.
//!
//! A slot remembers each test binary that passed in a run of every test: its
//! nextest binary id, its executable as a file (size, modification time,
//! inode), its package's directory and the tree it passed at. A later run of
//! every test skips a binary when all of these hold:
//!
//! - its executable is the same file: Cargo rewrites it whenever any Rust
//!   input across its crates, build scripts and dependencies changes;
//! - no path changed since that tree in its own package, in a workspace
//!   package it depends on, or outside every package (fixtures, the
//!   lockfile, the toolchain): data a test reads at run time lives there.
//!
//! Anything else runs again.

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::distance::Workspace;
use crate::products::Product;
use crate::snapshot::Revision;

/// A test binary that passed in a slot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct PassedBinary {
    pub(crate) id: String,
    pub(crate) executable: PathBuf,
    pub(crate) file: FileIdentity,
    /// Its package's directory, relative to the top of the checkout.
    pub(crate) package: PathBuf,
    pub(crate) tree: Revision,
}

/// What tells two versions of a file apart without reading it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct FileIdentity {
    pub(crate) size: u64,
    pub(crate) modified_ns: i128,
    pub(crate) inode: u64,
}

impl FileIdentity {
    /// The identity of the file at `path`; None when it is gone.
    pub(crate) fn of(path: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        Some(Self {
            size: metadata.len(),
            modified_ns: i128::from(metadata.mtime()) * 1_000_000_000
                + i128::from(metadata.mtime_nsec()),
            inode: metadata.ino(),
        })
    }
}

/// A test binary a build produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TestBinary {
    /// nextest's id: `package`, `package::test`, `package::bin/name`, ...
    pub(crate) id: String,
    pub(crate) executable: PathBuf,
    /// Its package's directory, relative to the top of the checkout.
    pub(crate) package: PathBuf,
}

impl TestBinary {
    /// The record of this binary passing at `tree`; None when its
    /// executable is gone.
    pub(crate) fn passed(&self, tree: &Revision) -> Option<PassedBinary> {
        Some(PassedBinary {
            id: self.id.clone(),
            executable: self.executable.clone(),
            file: FileIdentity::of(&self.executable)?,
            package: self.package.clone(),
            tree: tree.clone(),
        })
    }
}

/// The test binaries among `products`, Cargo's executables of a build of a
/// checkout at `checkout`.
pub(crate) fn test_binaries(products: &[Product], checkout: &Path) -> Vec<TestBinary> {
    products
        .iter()
        .filter_map(|product| test_binary(&product.message, checkout))
        .collect()
}

fn test_binary(message: &str, checkout: &Path) -> Option<TestBinary> {
    #[derive(Deserialize)]
    struct Artifact {
        package_id: String,
        manifest_path: PathBuf,
        target: Target,
        profile: Profile,
        executable: Option<PathBuf>,
    }
    #[derive(Deserialize)]
    struct Target {
        kind: Vec<String>,
        name: String,
    }
    #[derive(Deserialize)]
    struct Profile {
        test: bool,
    }
    let artifact = serde_json::from_str::<Artifact>(message).ok()?;
    if !artifact.profile.test {
        return None;
    }
    let package = package_name(&artifact.package_id)?;
    let kind = artifact.target.kind.first()?.as_str();
    let id = match kind {
        "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro" => package,
        "test" => format!("{package}::{}", artifact.target.name),
        "bin" | "example" | "bench" => format!("{package}::{kind}/{}", artifact.target.name),
        _ => return None,
    };
    let directory = artifact.manifest_path.parent()?;
    Some(TestBinary {
        id,
        executable: artifact.executable?,
        package: directory.strip_prefix(checkout).ok()?.to_owned(),
    })
}

/// The package name in a Cargo package id: `path+file:///w/x#name@1.0`, or
/// `path+file:///w/name#1.0` when the directory has the package's name.
fn package_name(id: &str) -> Option<String> {
    let (url, fragment) = id.rsplit_once('#')?;
    match fragment.split_once('@') {
        Some((name, _)) => Some(name.to_owned()),
        None => Some(url.rsplit('/').next()?.to_owned()),
    }
}

/// The binaries among `binaries` that may be skipped at `tree`, given what
/// `passed` in the slot, the `workspace`'s packages, and `changed`, the paths
/// that differ between an earlier tree and `tree`.
pub(crate) fn skippable<'a>(
    binaries: &'a [TestBinary],
    passed: &[PassedBinary],
    tree: &Revision,
    workspace: &Workspace,
    mut changed: impl FnMut(&Revision) -> Result<Vec<String>, String>,
) -> Vec<&'a TestBinary> {
    binaries
        .iter()
        .filter(|binary| {
            let Some(record) = passed
                .iter()
                .find(|record| record.id == binary.id && record.executable == binary.executable)
            else {
                return false;
            };
            if FileIdentity::of(&binary.executable) != Some(record.file) {
                return false;
            }
            if record.tree == *tree {
                return true;
            }
            // A tree that cannot be compared is not trusted.
            changed(&record.tree).is_ok_and(|paths| {
                !workspace.invalidates(paths.iter().map(String::as_str), &binary.package)
            })
        })
        .collect()
}

/// A nextest filterset that leaves out `skipped`.
pub(crate) fn excluding(skipped: &[&TestBinary]) -> String {
    let ids = skipped
        .iter()
        .map(|binary| format!("binary_id(={})", binary.id))
        .collect::<Vec<_>>();
    format!("not ({})", ids.join(" | "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::tests::TempDir;

    fn tree(name: &str) -> Revision {
        serde_json::from_value(serde_json::Value::String(name.into())).unwrap()
    }

    fn artifact(package_id: &str, manifest: &str, kind: &str, name: &str, test: bool) -> String {
        serde_json::json!({
            "reason": "compiler-artifact",
            "package_id": package_id,
            "manifest_path": manifest,
            "target": {"kind": [kind], "name": name},
            "profile": {"test": test},
            "executable": format!("/s/target/debug/deps/{name}-0123456789abcdef"),
        })
        .to_string()
    }

    #[test]
    fn test_binaries_get_nextests_ids_and_their_package_directory() {
        let products = [
            artifact(
                "path+file:///s/crates/gui#app-gui@0.1.0",
                "/s/crates/gui/Cargo.toml",
                "lib",
                "app_gui",
                true,
            ),
            artifact(
                "path+file:///s/crates/app#0.1.0",
                "/s/crates/app/Cargo.toml",
                "bin",
                "app",
                true,
            ),
            artifact(
                "path+file:///s/crates/app#0.1.0",
                "/s/crates/app/Cargo.toml",
                "test",
                "edge",
                true,
            ),
            // The binary itself, not its tests.
            artifact(
                "path+file:///s/crates/app#0.1.0",
                "/s/crates/app/Cargo.toml",
                "bin",
                "app",
                false,
            ),
        ]
        .map(|message| Product {
            message,
            files: Vec::new(),
        });
        let binaries = test_binaries(&products, Path::new("/s"));
        let ids = binaries
            .iter()
            .map(|binary| binary.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["app-gui", "app::bin/app", "app::edge"]);
        assert_eq!(binaries[0].package, Path::new("crates/gui"));
        assert_eq!(
            excluding(&binaries.iter().collect::<Vec<_>>()[..2]),
            "not (binary_id(=app-gui) | binary_id(=app::bin/app))"
        );
    }

    /// `domain` is used by `app`; `leaf` stands alone.
    fn workspace() -> Workspace {
        let package = |name: &str, dependencies: &[&str]| {
            serde_json::json!({
                "manifest_path": format!("/s/crates/{name}/Cargo.toml"),
                "dependencies": dependencies
                    .iter()
                    .map(|dependency| serde_json::json!({ "path": format!("/s/crates/{dependency}") }))
                    .collect::<Vec<_>>(),
            })
        };
        let metadata = serde_json::json!({
            "packages": [package("domain", &[]), package("app", &["domain"]), package("leaf", &[])],
        });
        Workspace::from_metadata(&metadata.to_string(), Path::new("/s")).unwrap()
    }

    #[test]
    fn a_binary_is_skipped_only_while_it_and_everything_it_reads_stayed_the_same() {
        let directory = TempDir::new();
        let binary = |name: &str| {
            let executable = directory.0.join(name);
            std::fs::write(&executable, name).unwrap();
            TestBinary {
                id: name.into(),
                executable,
                package: PathBuf::from(format!("crates/{name}")),
            }
        };
        let binaries = [binary("app"), binary("leaf")];
        let passed = binaries
            .iter()
            .map(|binary| binary.passed(&tree("t1")).unwrap())
            .collect::<Vec<_>>();
        let skipped = |changes: &[&str]| {
            let changes = changes
                .iter()
                .map(|path| (*path).to_owned())
                .collect::<Vec<_>>();
            skippable(&binaries, &passed, &tree("t2"), &workspace(), |_| {
                Ok(changes.clone())
            })
            .iter()
            .map(|binary| binary.id.clone())
            .collect::<Vec<_>>()
        };
        assert_eq!(skipped(&[]), ["app", "leaf"]);
        // A fixture of a package app depends on: app runs, leaf does not.
        assert_eq!(skipped(&["crates/domain/tests/fixture.json"]), ["leaf"]);
        // A path outside every package, or the lockfile: everything runs.
        assert!(skipped(&["assets/font.ttf"]).is_empty());
        assert!(skipped(&["Cargo.lock"]).is_empty());
        // A rewritten executable runs whatever changed.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&binaries[1].executable, "rebuilt").unwrap();
        assert_eq!(skipped(&[]), ["app"]);
        // A tree that cannot be compared is not trusted.
        let none = skippable(&binaries[..1], &passed, &tree("t2"), &workspace(), |_| {
            Err("unknown tree".into())
        });
        assert!(none.is_empty());
    }
}
