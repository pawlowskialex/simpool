//! Describes a bundle as a manifest of hashes, so the server can ask for only what it lacks.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

pub struct Scan {
    pub manifest: Value,
    /// Hash -> a local file with those contents, and its size.
    pub blobs: HashMap<String, (PathBuf, u64)>,
}

struct LocalFile {
    rel: String,
    path: PathBuf,
    size: u64,
    executable: bool,
}

#[derive(Default)]
struct Walk {
    dirs: Vec<String>,
    files: Vec<LocalFile>,
    symlinks: Vec<(String, String)>,
}

#[cfg(unix)]
fn is_executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_: &fs::Metadata) -> bool {
    true
}

/// Lists `dir` into `out`. A symlink that stays inside the bundle is kept as a symlink. One that
/// leaves it (build tools like Bazel make these) wouldn't resolve on the server, so what it points
/// at is sent instead.
fn walk(dir: &Path, rel: &str, root: &Path, chain: &mut Vec<PathBuf>, out: &mut Walk) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?.collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| anyhow!("{} isn't valid UTF-8", path.display()))?;
        let child = if rel.is_empty() { name.to_string() } else { format!("{rel}/{name}") };
        let meta = fs::symlink_metadata(&path).with_context(|| format!("reading {}", path.display()))?;
        if meta.file_type().is_symlink() {
            let target = fs::read_link(&path)?;
            let Ok(resolved) = fs::canonicalize(&path) else {
                // Dangling here, so it means nothing we could send. Keep it as it is.
                let target = target.to_str().ok_or_else(|| anyhow!("{} isn't valid UTF-8", path.display()))?;
                out.symlinks.push((child, target.to_string()));
                continue;
            };
            if resolved.starts_with(root) && !target.is_absolute() {
                let target = target.to_str().ok_or_else(|| anyhow!("{} isn't valid UTF-8", path.display()))?;
                out.symlinks.push((child, target.to_string()));
            } else if resolved.is_dir() {
                if chain.contains(&resolved) {
                    bail!("{} loops back to {}", path.display(), resolved.display());
                }
                out.dirs.push(child.clone());
                chain.push(resolved.clone());
                walk(&resolved, &child, root, chain, out)?;
                chain.pop();
            } else {
                add_file(&resolved, child, out)?;
            }
        } else if meta.is_dir() {
            out.dirs.push(child.clone());
            chain.push(path.clone());
            walk(&path, &child, root, chain, out)?;
            chain.pop();
        } else if meta.is_file() {
            add_file(&path, child, out)?;
        } else {
            eprintln!("simpool: skipping {}: not a file, directory or symlink", path.display());
        }
    }
    Ok(())
}

fn add_file(path: &Path, rel: String, out: &mut Walk) -> Result<()> {
    let meta = fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    out.files.push(LocalFile { rel, path: path.to_path_buf(), size: meta.len(), executable: is_executable(&meta) });
    Ok(())
}

fn hash_file(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut f, &mut hasher).with_context(|| format!("reading {}", path.display()))?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// Hashes on every core; a big app is mostly a few large files and many small ones.
fn hash_all(files: &[LocalFile]) -> Result<Vec<String>> {
    let hashes: Vec<Mutex<String>> = files.iter().map(|_| Mutex::new(String::new())).collect();
    let next = AtomicUsize::new(0);
    let failure = Mutex::new(None);
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(file) = files.get(i) else { return };
                    match hash_file(&file.path) {
                        Ok(h) => *hashes[i].lock().unwrap() = h,
                        Err(e) => {
                            failure.lock().unwrap().get_or_insert(e);
                            return;
                        }
                    }
                }
            });
        }
    });
    if let Some(e) = failure.into_inner().unwrap() {
        return Err(e);
    }
    Ok(hashes.into_iter().map(|h| h.into_inner().unwrap()).collect())
}

/// Scans the bundle at `path`, sent to the server under the name `name`.
pub fn scan(path: &Path, name: &str) -> Result<Scan> {
    let root = fs::canonicalize(path).with_context(|| format!("reading {}", path.display()))?;
    let mut out = Walk::default();
    walk(&root, "", &root, &mut vec![root.clone()], &mut out)?;
    let hashes = hash_all(&out.files)?;
    let mut blobs = HashMap::new();
    let mut files = Vec::with_capacity(out.files.len());
    for (f, hash) in out.files.iter().zip(&hashes) {
        files.push(json!({"path": f.rel, "sha256": hash, "size": f.size, "executable": f.executable}));
        blobs.entry(hash.clone()).or_insert_with(|| (f.path.clone(), f.size));
    }
    let symlinks: Vec<Value> = out.symlinks.iter().map(|(path, target)| json!({"path": path, "target": target})).collect();
    Ok(Scan { manifest: json!({"name": name, "dirs": out.dirs, "files": files, "symlinks": symlinks}), blobs })
}

/// The name a bundle goes by: its own, unless that's something like "." or a symlink without the
/// bundle's extension, in which case it's the name of what it resolves to.
pub fn bundle_name(path: &Path) -> Result<String> {
    let own = path.file_name().and_then(|n| n.to_str()).filter(|n| n.ends_with(".app") || n.ends_with(".dSYM"));
    if let Some(n) = own {
        return Ok(n.to_string());
    }
    let real = fs::canonicalize(path).with_context(|| format!("reading {}", path.display()))?;
    real.file_name().and_then(|n| n.to_str()).map(String::from).ok_or_else(|| anyhow!("{} has no usable name", path.display()))
}

/// dSYMs that build tools leave next to a bundle: the app's own and those of what it embeds.
pub fn find_dsyms(installed: &Path) -> Vec<PathBuf> {
    let Some(parent) = installed.parent() else { return Vec::new() };
    let Some(name) = installed.file_name().and_then(|n| n.to_str()) else { return Vec::new() };
    let mut names = vec![format!("{name}.dSYM")];
    if let Some(stem) = name.strip_suffix(".ipa") {
        names = vec![format!("{stem}.app.dSYM")];
    }
    for sub in ["Frameworks", "PlugIns", "Extensions"] {
        let Ok(entries) = fs::read_dir(installed.join(sub)) else { continue };
        names.extend(entries.flatten().filter_map(|e| e.file_name().to_str().map(|n| format!("{n}.dSYM"))));
    }
    names.sort();
    names.into_iter().map(|n| parent.join(n)).filter(|p| p.is_dir()).collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn entry<'a>(scan: &'a Scan, path: &str) -> &'a Value {
        scan.manifest["files"].as_array().unwrap().iter().find(|f| f["path"] == path).unwrap_or_else(|| panic!("{path} not in manifest"))
    }

    fn write(path: &Path, data: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }

    #[test]
    fn manifest_lists_every_kind_of_entry() {
        let t = tempfile::tempdir().unwrap();
        let app = t.path().join("My.app");
        write(&app.join("My"), b"binary");
        fs::set_permissions(app.join("My"), fs::Permissions::from_mode(0o755)).unwrap();
        write(&app.join("Frameworks/Foo.framework/Foo"), b"foo");
        fs::create_dir_all(app.join("Empty")).unwrap();
        symlink("Foo.framework/Foo", app.join("Frameworks/inside")).unwrap();

        let s = scan(&app, "My.app").unwrap();
        assert_eq!(s.manifest["name"], "My.app");
        assert_eq!(entry(&s, "My")["executable"], true);
        assert_eq!(entry(&s, "My")["size"], 6);
        assert_eq!(entry(&s, "My")["sha256"], format!("{:x}", Sha256::digest(b"binary")));
        assert_eq!(entry(&s, "Frameworks/Foo.framework/Foo")["executable"], false);
        let dirs = s.manifest["dirs"].as_array().unwrap();
        assert!(dirs.contains(&json!("Empty")) && dirs.contains(&json!("Frameworks/Foo.framework")));
        assert_eq!(s.manifest["symlinks"], json!([{"path": "Frameworks/inside", "target": "Foo.framework/Foo"}]));
    }

    #[test]
    fn identical_files_share_one_blob() {
        let t = tempfile::tempdir().unwrap();
        let app = t.path().join("My.app");
        write(&app.join("a"), b"same");
        write(&app.join("b"), b"same");
        write(&app.join("c"), b"other");
        let s = scan(&app, "My.app").unwrap();
        assert_eq!(s.blobs.len(), 2);
        assert_eq!(s.manifest["files"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn read_only_trees_scan_fine() {
        let t = tempfile::tempdir().unwrap();
        let app = t.path().join("My.app");
        write(&app.join("Sub/file"), b"x");
        fs::set_permissions(app.join("Sub/file"), fs::Permissions::from_mode(0o444)).unwrap();
        fs::set_permissions(app.join("Sub"), fs::Permissions::from_mode(0o555)).unwrap();
        let s = scan(&app, "My.app");
        fs::set_permissions(app.join("Sub"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(entry(&s.unwrap(), "Sub/file")["size"], 1);
    }

    #[test]
    fn symlinks_leaving_the_bundle_are_sent_as_their_contents() {
        let t = tempfile::tempdir().unwrap();
        let app = t.path().join("My.app");
        fs::create_dir_all(&app).unwrap();
        write(&t.path().join("cas/out.bin"), b"outside");
        write(&t.path().join("cas/dir/nested"), b"nested");
        symlink(t.path().join("cas/out.bin"), app.join("file_link")).unwrap();
        symlink("../cas/dir", app.join("dir_link")).unwrap();
        symlink("/does/not/exist", app.join("dangling")).unwrap();

        let s = scan(&app, "My.app").unwrap();
        assert_eq!(entry(&s, "file_link")["sha256"], format!("{:x}", Sha256::digest(b"outside")));
        assert_eq!(entry(&s, "dir_link/nested")["sha256"], format!("{:x}", Sha256::digest(b"nested")));
        assert!(s.manifest["dirs"].as_array().unwrap().contains(&json!("dir_link")));
        assert_eq!(s.manifest["symlinks"], json!([{"path": "dangling", "target": "/does/not/exist"}]));
    }

    #[test]
    fn symlink_loops_fail_instead_of_recursing_forever() {
        let t = tempfile::tempdir().unwrap();
        let app = t.path().join("My.app");
        write(&t.path().join("cas/dir/file"), b"x");
        symlink(t.path().join("cas/dir"), { fs::create_dir_all(&app).unwrap(); app.join("out") }).unwrap();
        symlink(t.path().join("cas/dir"), t.path().join("cas/dir/back")).unwrap();
        assert!(scan(&app, "My.app").err().unwrap().to_string().contains("loops back"));
    }

    #[test]
    fn names_come_from_the_bundle_not_the_link() {
        let t = tempfile::tempdir().unwrap();
        let app = t.path().join("Real.app");
        fs::create_dir_all(&app).unwrap();
        symlink(&app, t.path().join("current")).unwrap();
        symlink(&app, t.path().join("Named.app")).unwrap();
        assert_eq!(bundle_name(&t.path().join("current")).unwrap(), "Real.app");
        assert_eq!(bundle_name(&t.path().join("Named.app")).unwrap(), "Named.app");
    }

    #[test]
    fn finds_sibling_dsyms_for_the_app_and_what_it_embeds() {
        let t = tempfile::tempdir().unwrap();
        let app = t.path().join("My.app");
        fs::create_dir_all(app.join("Frameworks/Foo.framework")).unwrap();
        fs::create_dir_all(app.join("Frameworks/NoSym.framework")).unwrap();
        fs::create_dir_all(app.join("PlugIns/Ext.appex")).unwrap();
        for d in ["My.app.dSYM", "Foo.framework.dSYM", "Ext.appex.dSYM", "Unrelated.dSYM"] {
            fs::create_dir_all(t.path().join(d)).unwrap();
        }
        let found: Vec<_> = find_dsyms(&app).iter().map(|p| p.file_name().unwrap().to_str().unwrap().to_string()).collect();
        assert_eq!(found, ["Ext.appex.dSYM", "Foo.framework.dSYM", "My.app.dSYM"]);

        let ipa = t.path().join("My.ipa");
        fs::write(&ipa, b"").unwrap();
        let found: Vec<_> = find_dsyms(&ipa).iter().map(|p| p.file_name().unwrap().to_str().unwrap().to_string()).collect();
        assert_eq!(found, ["My.app.dSYM"]);
    }
}
