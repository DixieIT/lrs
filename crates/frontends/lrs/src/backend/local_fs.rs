//! The filesystem adapter behind `sdk::FsPort` for the in-process runtime.
//!
//! Every path must stay inside the project root: reads and listings are
//! checked after symlinks are resolved, and writes are checked on their
//! nearest existing ancestor and may not contain `..`. This is the only code
//! in the local composition that touches `std::fs` on a tool's behalf.

use sdk::{Error, FsPort, Result};
use std::path::{Component, Path, PathBuf};

/// Largest file `read` returns. Bigger files are refused with their size, so
/// one read cannot flood the model's context.
const MAX_READ_BYTES: u64 = 1024 * 1024;

pub struct LocalFs {
    root: PathBuf,
}

impl LocalFs {
    pub fn new(root: &Path) -> std::io::Result<Self> {
        Ok(Self { root: root.canonicalize()? })
    }

    fn outside(path: &Path) -> Error {
        Error::PermissionDenied(format!("{} is outside the project", path.display()))
    }

    /// An existing path, resolved and confined to the root.
    fn existing(&self, path: &Path) -> Result<PathBuf> {
        let resolved = path.canonicalize().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Error::InvalidInput(format!("{} does not exist", path.display())),
            _ => Error::Unavailable(format!("{}: {e}", path.display())),
        })?;
        if resolved.starts_with(&self.root) { Ok(resolved) } else { Err(Self::outside(path)) }
    }

    /// A path that may not exist yet, confined to the root through its
    /// nearest existing ancestor.
    fn writable(&self, path: &Path) -> Result<PathBuf> {
        if path.components().any(|c| c == Component::ParentDir) {
            return Err(Error::PermissionDenied(format!("{} contains ..", path.display())));
        }
        let mut ancestor = path;
        while !ancestor.exists() {
            ancestor = ancestor.parent().ok_or_else(|| Self::outside(path))?;
        }
        let mut resolved = self.existing(ancestor)?;
        let rest = path.strip_prefix(ancestor).map_err(|_| Self::outside(path))?;
        // Component by component: `canonicalize` returns a verbatim `\\?\`
        // path on Windows, where `/` is not a separator and a trailing one
        // (what `join("")` adds when the file already exists) is an invalid
        // name. `push` of each component writes native separators and nothing
        // for an empty rest.
        for part in rest.components() {
            resolved.push(part);
        }
        Ok(resolved)
    }
}

fn io(path: &Path, e: std::io::Error) -> Error {
    Error::Unavailable(format!("{}: {e}", path.display()))
}

impl FsPort for LocalFs {
    fn read(&self, path: &Path) -> Result<String> {
        let file = self.existing(path)?;
        let size = std::fs::metadata(&file).map_err(|e| io(path, e))?.len();
        if size > MAX_READ_BYTES {
            return Err(Error::InvalidInput(format!(
                "{} is {size} bytes; files over {MAX_READ_BYTES} bytes are not read", path.display()
            )));
        }
        std::fs::read_to_string(&file).map_err(|e| match e.kind() {
            std::io::ErrorKind::InvalidData => Error::InvalidInput(format!("{} is not UTF-8 text", path.display())),
            _ => io(path, e),
        })
    }

    fn write(&self, path: &Path, content: &str) -> Result<()> {
        let target = self.writable(path)?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io(path, e))?;
        }
        std::fs::write(&target, content).map_err(|e| io(path, e))
    }

    fn list(&self, path: &Path) -> Result<Vec<String>> {
        let dir = self.existing(path)?;
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&dir).map_err(|e| io(path, e))? {
            let entry = entry.map_err(|e| io(path, e))?;
            let mut name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type().map_err(|e| io(path, e))?.is_dir() {
                name.push('/');
            }
            entries.push(name);
        }
        entries.sort();
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> (PathBuf, LocalFs) {
        let base = std::env::temp_dir().join(format!("lrs-fs-{}", std::process::id()))
            .join(format!("{:?}", std::thread::current().id()).replace(['(', ')'], ""));
        std::fs::create_dir_all(base.join("proj/src")).unwrap();
        std::fs::write(base.join("secret.txt"), "outside").unwrap();
        let fs = LocalFs::new(&base.join("proj")).unwrap();
        (base, fs)
    }

    #[test]
    fn reads_writes_and_lists_inside_the_root() {
        let (base, fs) = project();
        let root = base.join("proj");
        fs.write(&root.join("src/new/a.txt"), "hello").unwrap();
        assert_eq!(fs.read(&root.join("src/new/a.txt")).unwrap(), "hello");
        assert_eq!(fs.list(&root).unwrap(), ["src/"]);
        assert_eq!(fs.list(&root.join("src")).unwrap(), ["new/"]);
    }

    #[test]
    fn an_existing_file_can_be_overwritten() {
        let (base, fs) = project();
        let file = base.join("proj/src/main.py");
        fs.write(&file, "first").unwrap();
        fs.write(&file, "second").unwrap();
        assert_eq!(fs.read(&file).unwrap(), "second");
    }

    #[test]
    fn forward_slashes_work_for_new_and_existing_paths() {
        // Tools join the model's `a/b/c` onto the project root as given.
        let (base, fs) = project();
        let root = base.join("proj");
        fs.write(&root.join("src/new_pkg/deep/mod.py"), "x").unwrap();
        fs.write(&root.join("src/new_pkg/deep/mod.py"), "y").unwrap();
        assert_eq!(fs.read(&root.join("src/new_pkg/deep/mod.py")).unwrap(), "y");
        assert_eq!(fs.list(&root.join("src/new_pkg")).unwrap(), ["deep/"]);
    }

    #[test]
    fn refuses_every_way_out_of_the_root() {
        let (base, fs) = project();
        let root = base.join("proj");
        for denied in [
            fs.read(&base.join("secret.txt")).unwrap_err(),
            fs.read(&root.join("../secret.txt")).unwrap_err(),
            fs.write(&root.join("../escape.txt"), "x").unwrap_err(),
            fs.write(&base.join("escape.txt"), "x").unwrap_err(),
            fs.list(&base).unwrap_err(),
        ] {
            assert!(matches!(denied, Error::PermissionDenied(_)), "{denied:?}");
        }
        assert!(!base.join("escape.txt").exists());
    }

    #[test]
    fn missing_files_and_huge_files_are_input_errors() {
        let (base, fs) = project();
        let root = base.join("proj");
        assert!(matches!(fs.read(&root.join("nope")).unwrap_err(), Error::InvalidInput(_)));
        let big = "x".repeat(usize::try_from(MAX_READ_BYTES).unwrap() + 1);
        fs.write(&root.join("big.txt"), &big).unwrap();
        assert!(matches!(fs.read(&root.join("big.txt")).unwrap_err(), Error::InvalidInput(_)));
    }
}
