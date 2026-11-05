//! Trees that may contain read-only directories. Bazel and Nix outputs do, and archives keep those
//! modes. `simctl install` copies modes as it goes, so it creates a read-only directory and then
//! can't write into it, and `remove_dir_all` can't empty one.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Gives the owner full access to everything under `root`. Symlinks are left alone, not followed.
pub fn make_writable(root: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(root)?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    let mode = meta.permissions().mode();
    let want = mode | if meta.is_dir() { 0o700 } else { 0o600 };
    // Directories come first: their entries can't be listed or changed until this is done.
    if want != mode {
        fs::set_permissions(root, fs::Permissions::from_mode(want))?;
    }
    if meta.is_dir() {
        for entry in fs::read_dir(root)? {
            make_writable(&entry?.path())?;
        }
    }
    Ok(())
}

/// `remove_dir_all` that also handles read-only directories. A missing path is not an error.
pub fn remove_tree(root: &Path) -> io::Result<()> {
    match fs::remove_dir_all(root) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            make_writable(root)?;
            fs::remove_dir_all(root)
        }
        other => other,
    }
}

/// Runs filesystem work off the async runtime.
pub async fn blocking<T: Send + 'static>(f: impl FnOnce() -> io::Result<T> + Send + 'static) -> io::Result<T> {
    tokio::task::spawn_blocking(f).await.map_err(io::Error::other)?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(p: &Path) -> u32 {
        fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn read_only_tree(root: &Path) {
        let sub = root.join("My.app/Frameworks");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("file"), b"x").unwrap();
        fs::set_permissions(sub.join("file"), fs::Permissions::from_mode(0o444)).unwrap();
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o555)).unwrap();
        fs::set_permissions(root.join("My.app"), fs::Permissions::from_mode(0o555)).unwrap();
    }

    #[test]
    fn make_writable_opens_read_only_trees_and_keeps_other_bits() {
        let t = tempfile::tempdir().unwrap();
        read_only_tree(t.path());
        make_writable(t.path()).unwrap();
        assert_eq!(mode(&t.path().join("My.app/Frameworks")), 0o755);
        assert_eq!(mode(&t.path().join("My.app/Frameworks/file")), 0o644);
    }

    #[test]
    fn make_writable_does_not_follow_symlinks() {
        let t = tempfile::tempdir().unwrap();
        let outside = t.path().join("outside");
        fs::write(&outside, b"x").unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o444)).unwrap();
        let tree = t.path().join("tree");
        fs::create_dir(&tree).unwrap();
        std::os::unix::fs::symlink(&outside, tree.join("link")).unwrap();
        make_writable(&tree).unwrap();
        assert_eq!(mode(&outside), 0o444);
    }

    #[test]
    fn remove_tree_removes_read_only_trees() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("scratch");
        fs::create_dir(&root).unwrap();
        read_only_tree(&root);
        assert_eq!(fs::remove_dir_all(&root).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        remove_tree(&root).unwrap();
        assert!(!root.exists());
        remove_tree(&root).unwrap();
    }
}
