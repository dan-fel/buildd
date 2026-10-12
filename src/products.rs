//! What a build hands its client besides output: the executables Cargo
//! produced, copied out of the slot before the slot takes another build, or
//! the outputs a `run` command left, moved out of its own directory.

use std::path::{Path, PathBuf};

/// An executable Cargo reported: its `compiler-artifact` message and the
/// files it names, the executable and its debug information.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Product {
    pub(crate) message: String,
    pub(crate) files: Vec<PathBuf>,
}

/// Copies every file of `products` into `directory`, each replacing a file
/// of the same name there as a whole, and returns each product's message
/// with its paths naming the copies.
///
/// # Errors
/// When two products name the same file, or a copy fails.
pub(crate) fn copy(products: &[Product], directory: &Path) -> Result<Vec<String>, String> {
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("could not create {}: {error}", directory.display()))?;
    let mut names = std::collections::HashSet::new();
    for file in products.iter().flat_map(|product| &product.files) {
        let name = file_name(file)?;
        if !names.insert(name.to_owned()) {
            return Err(format!(
                "two executables would be copied to {}",
                directory.join(name).display()
            ));
        }
    }
    products
        .iter()
        .map(|product| {
            for file in &product.files {
                let to = directory.join(file_name(file)?);
                install(&to, |partial| copy_tree(file, partial))
                    .map_err(|error| failed("copy", file, &to, &error))?;
            }
            Ok(rewrite(&product.message, directory))
        })
        .collect()
}

/// Moves every entry of `staged` into `directory`, each replacing an entry
/// of the same name there as a whole, and returns where each went, in name
/// order.
///
/// # Errors
/// When `directory` cannot be made, or a move fails.
pub(crate) fn deliver(staged: &Path, directory: &Path) -> Result<Vec<PathBuf>, String> {
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("could not create {}: {error}", directory.display()))?;
    let mut names = std::fs::read_dir(staged)
        .and_then(|entries| {
            entries
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(|error| format!("could not read {}: {error}", staged.display()))?;
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let (from, to) = (staged.join(&name), directory.join(&name));
            install(&to, |partial| move_tree(&from, partial))
                .map_err(|error| failed("move", &from, &to, &error))?;
            Ok(to)
        })
        .collect()
}

fn failed(what: &str, from: &Path, to: &Path, error: &std::io::Error) -> String {
    format!(
        "could not {what} {} to {}: {error}",
        from.display(),
        to.display()
    )
}

fn file_name(path: &Path) -> Result<&std::ffi::OsStr, String> {
    path.file_name()
        .ok_or_else(|| format!("Cargo reported a file without a name: {}", path.display()))
}

/// Puts a new `to`, a file or a directory, in place through a partial one
/// next to it that `fill` makes, so `to` is either the old or the complete
/// new one.
fn install(to: &Path, fill: impl FnOnce(&Path) -> std::io::Result<()>) -> std::io::Result<()> {
    let mut partial = to.as_os_str().to_owned();
    partial.push(".buildd-partial");
    let partial = PathBuf::from(partial);
    remove(&partial)?;
    fill(&partial)?;
    if std::fs::symlink_metadata(to).is_ok_and(|metadata| metadata.is_dir()) {
        // A directory cannot be renamed over a non-empty one.
        std::fs::remove_dir_all(to)?;
    }
    std::fs::rename(&partial, to)
}

/// Moves `from` to `to`: renames it, or copies it where `to` is on another
/// file system, which a rename cannot reach.
fn move_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::rename(from, to) {
        Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => copy_tree(from, to),
        moved => moved,
    }
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(from)?;
    if metadata.is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(from)?, to)
    } else if metadata.is_dir() {
        std::fs::create_dir(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        std::fs::set_permissions(to, metadata.permissions())
    } else {
        std::fs::copy(from, to).map(drop)
    }
}

fn remove(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// `message` with its `filenames` and `executable` moved into `directory`.
fn rewrite(message: &str, directory: &Path) -> String {
    let mut value: serde_json::Value =
        serde_json::from_str(message).expect("a product's message is Cargo's JSON");
    let moved = |path: &serde_json::Value| {
        let path = Path::new(path.as_str().expect("Cargo's paths are strings"));
        let name = path.file_name().expect("checked before copying");
        serde_json::Value::String(directory.join(name).to_string_lossy().into_owned())
    };
    if let Some(files) = value
        .get_mut("filenames")
        .and_then(serde_json::Value::as_array_mut)
    {
        for file in files {
            *file = moved(file);
        }
    }
    if let Some(executable) = value.get_mut("executable") {
        *executable = moved(executable);
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::tests::TempDir;

    fn product(slot: &Path, name: &str) -> Product {
        let executable = slot.join(name);
        let symbols = slot.join(format!("{name}.dSYM"));
        std::fs::write(&executable, name).unwrap();
        std::fs::create_dir_all(symbols.join("Contents")).unwrap();
        std::fs::write(symbols.join("Contents/Info.plist"), "plist").unwrap();
        let message = serde_json::json!({
            "reason": "compiler-artifact",
            "target": {"name": name},
            "filenames": [executable, symbols],
            "executable": executable,
        });
        Product {
            message: message.to_string(),
            files: vec![executable, symbols],
        }
    }

    #[test]
    fn executables_and_their_symbols_are_copied_and_their_messages_name_the_copies() {
        let slot = TempDir::new();
        let out = TempDir::new();
        let destination = out.0.join("bin");
        let products = [product(&slot.0, "app"), product(&slot.0, "tool")];
        // An older copy is replaced as a whole.
        std::fs::create_dir_all(destination.join("app.dSYM/stale")).unwrap();
        let messages = copy(&products, &destination).unwrap();
        assert_eq!(
            std::fs::read_to_string(destination.join("app")).unwrap(),
            "app"
        );
        assert_eq!(
            std::fs::read_to_string(destination.join("tool.dSYM/Contents/Info.plist")).unwrap(),
            "plist"
        );
        assert!(!destination.join("app.dSYM/stale").exists());
        let first: serde_json::Value = serde_json::from_str(&messages[0]).unwrap();
        assert_eq!(
            first["executable"],
            destination.join("app").to_str().unwrap()
        );
        assert_eq!(
            first["filenames"][1],
            destination.join("app.dSYM").to_str().unwrap()
        );
        assert_eq!(first["target"]["name"], "app");
        assert_eq!(
            std::fs::read_dir(&destination).unwrap().count(),
            4,
            "no partial copies stay"
        );
    }

    #[test]
    fn a_commands_outputs_replace_their_namesakes_as_wholes() {
        let staged = TempDir::new();
        let out = TempDir::new();
        let destination = out.0.join("images");
        std::fs::write(staged.0.join("disk.img"), "new disk").unwrap();
        std::fs::create_dir_all(staged.0.join("kernel/modules")).unwrap();
        std::fs::write(staged.0.join("kernel/modules/a.ko"), "a").unwrap();
        std::fs::create_dir_all(destination.join("kernel/stale")).unwrap();
        std::fs::write(destination.join("disk.img"), "old disk").unwrap();
        std::fs::write(destination.join("kept"), "kept").unwrap();
        let delivered = deliver(&staged.0, &destination).unwrap();
        assert_eq!(
            delivered,
            [destination.join("disk.img"), destination.join("kernel")]
        );
        assert_eq!(
            std::fs::read_to_string(destination.join("disk.img")).unwrap(),
            "new disk"
        );
        assert!(destination.join("kernel/modules/a.ko").exists());
        assert!(!destination.join("kernel/stale").exists());
        assert!(destination.join("kept").exists(), "other entries stay");
        assert_eq!(std::fs::read_dir(&staged.0).unwrap().count(), 0, "moved");
        assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 3);
    }

    #[test]
    fn two_products_with_one_name_are_refused_before_anything_is_copied() {
        let slot = TempDir::new();
        let other = TempDir::new();
        let out = TempDir::new();
        let products = [product(&slot.0, "app"), product(&other.0, "app")];
        let error = copy(&products, &out.0).unwrap_err();
        assert!(error.contains("two executables"), "{error}");
        assert_eq!(std::fs::read_dir(&out.0).unwrap().count(), 0);
    }
}
