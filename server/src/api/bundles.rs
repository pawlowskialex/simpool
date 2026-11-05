//! Installing a directory by manifest, so a reinstall only sends the files that changed.
//!
//! The client posts a manifest of the bundle. If the server is missing any file contents, it
//! answers `{"missing": [hash, …]}`; the client sends those to `/blobs` and posts the manifest
//! again. Contents live in a store shared by every lease, so a new lease starts with what earlier
//! ones uploaded, and an interrupted upload picks up where it stopped.

use super::apps::install_dir;
use super::{scratch, AppState, L};
use crate::error::{ApiError, ApiResult};
use crate::fsutil::{self, blocking, remove_tree};
use crate::proc::simctl;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::routing::post;
use axum::{Json, Router};
use futures::TryStreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path as FsPath, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::io::StreamReader;

/// Blobs used this recently are never evicted, so an install in progress can't lose its files.
const EVICT_GRACE: Duration = Duration::from_secs(3600);

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/blobs", post(upload_blobs))
        .route("/apps/manifest", post(install_manifest))
        .route("/apps/{bundle}/dsyms", post(add_dsym))
}

#[derive(Deserialize)]
pub struct Manifest {
    /// Top-level name, e.g. "MyApp.app". Everything else is relative to it.
    name: String,
    #[serde(default)]
    dirs: Vec<String>,
    #[serde(default)]
    files: Vec<FileEntry>,
    #[serde(default)]
    symlinks: Vec<LinkEntry>,
}

#[derive(Deserialize)]
struct FileEntry {
    path: String,
    sha256: String,
    size: u64,
    #[serde(default)]
    executable: bool,
}

#[derive(Deserialize)]
struct LinkEntry {
    path: String,
    target: String,
}

fn relative(path: &str) -> bool {
    let p = FsPath::new(path);
    !path.is_empty() && p.components().all(|c| matches!(c, Component::Normal(_)))
}

impl Manifest {
    fn validate(&self) -> ApiResult<()> {
        if !relative(&self.name) || self.name.contains('/') {
            return Err(ApiError::bad_request(format!("bad bundle name {:?}", self.name)));
        }
        let paths = self.dirs.iter().chain(self.files.iter().map(|f| &f.path)).chain(self.symlinks.iter().map(|l| &l.path));
        if let Some(bad) = paths.into_iter().find(|p| !relative(p)) {
            return Err(ApiError::bad_request(format!("bad path in manifest: {bad:?}")));
        }
        if let Some(bad) = self.files.iter().find(|f| !is_hash(&f.sha256)) {
            return Err(ApiError::bad_request(format!("bad sha256 for {:?}", bad.path)));
        }
        // Symlinks are created last, so files can't be written through them, but one could be
        // nested under another.
        let links: HashSet<&FsPath> = self.symlinks.iter().map(|l| FsPath::new(&l.path)).collect();
        if let Some(bad) = self.symlinks.iter().find(|l| FsPath::new(&l.path).ancestors().skip(1).any(|a| links.contains(a))) {
            return Err(ApiError::bad_request(format!("symlink {:?} is inside another symlink", bad.path)));
        }
        Ok(())
    }

    fn total_bytes(&self) -> u64 {
        self.files.iter().fold(0u64, |n, f| n.saturating_add(f.size))
    }
}

fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn blob_path(store: &FsPath, hash: &str) -> PathBuf {
    store.join(&hash[..2]).join(hash)
}

/// Marks the manifest's blobs as used and returns the ones the store doesn't have.
fn find_missing(store: &FsPath, m: &Manifest) -> io::Result<Vec<String>> {
    let hashes: BTreeSet<&str> = m.files.iter().map(|f| f.sha256.as_str()).collect();
    let mut missing = Vec::new();
    let now = SystemTime::now();
    for hash in hashes {
        match fs::File::options().write(true).open(blob_path(store, hash)) {
            Ok(f) => f.set_modified(now)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => missing.push(hash.to_string()),
            Err(e) => return Err(e),
        }
    }
    Ok(missing)
}

/// Builds the tree at `root`. `fs::copy` clones on APFS, so this costs almost no disk or time.
/// Modes come from the manifest rather than the source machine, so nothing arrives read-only.
fn materialize(store: &FsPath, m: &Manifest, root: &FsPath) -> io::Result<()> {
    fs::create_dir_all(root)?;
    for d in &m.dirs {
        fs::create_dir_all(root.join(d))?;
    }
    for f in &m.files {
        let dest = root.join(&f.path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(blob_path(store, &f.sha256), &dest)?;
        fs::set_permissions(&dest, fs::Permissions::from_mode(if f.executable { 0o755 } else { 0o644 }))?;
    }
    for l in &m.symlinks {
        let dest = root.join(&l.path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        std::os::unix::fs::symlink(&l.target, &dest)?;
    }
    Ok(())
}

/// Deletes the least recently used blobs until the store fits in `limit` bytes, plus temp files
/// abandoned by interrupted uploads.
fn evict(store: &FsPath, limit: u64) -> io::Result<()> {
    let now = SystemTime::now();
    let old = |t: SystemTime| now.duration_since(t).is_ok_and(|age| age > EVICT_GRACE);
    let mut blobs = Vec::new();
    let mut total = 0u64;
    for entry in fs::read_dir(store)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_file() {
            if old(meta.modified()?) {
                let _ = fs::remove_file(entry.path());
            }
            continue;
        }
        for blob in fs::read_dir(entry.path())? {
            let blob = blob?;
            let meta = blob.metadata()?;
            total += meta.len();
            blobs.push((meta.modified()?, meta.len(), blob.path()));
        }
    }
    blobs.sort();
    for (modified, len, path) in blobs {
        if total <= limit || !old(modified) {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            total -= len;
        }
    }
    Ok(())
}

fn evict_soon(store: PathBuf, limit: u64) {
    tokio::task::spawn_blocking(move || {
        if let Err(e) = evict(&store, limit) {
            tracing::warn!("trimming the blob store: {e}");
        }
    });
}

/// Body: any number of `[sha256: 32 bytes][length: u64 BE][contents]` frames. Each blob is kept as
/// soon as it verifies, so a dropped connection only loses the one in flight.
async fn upload_blobs(State(pool): State<AppState>, L(lease): L, body: Body) -> ApiResult<Json<Value>> {
    let _hold = lease.hold();
    let store = pool.blob_store();
    tokio::fs::create_dir_all(&store).await?;
    let stream = body.into_data_stream().map_err(io::Error::other);
    let mut rd = StreamReader::new(stream);
    let (mut blobs, mut bytes) = (0u64, 0u64);
    loop {
        let mut raw = [0u8; 32];
        match rd.read_exact(&mut raw).await {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(ApiError::bad_request(format!("reading upload: {e}"))),
        }
        let hash: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        let len = rd.read_u64().await.map_err(|e| ApiError::bad_request(format!("reading upload: {e}")))?;
        let tmp = store.join(format!(".tmp-{}", uuid::Uuid::new_v4().simple()));
        let saved = save_blob(&mut rd, len, &hash, &tmp, &store).await;
        if saved.is_err() {
            let _ = tokio::fs::remove_file(&tmp).await;
        }
        saved?;
        blobs += 1;
        bytes += len;
    }
    Ok(Json(json!({ "blobs": blobs, "bytes": bytes })))
}

async fn save_blob(rd: &mut (impl tokio::io::AsyncRead + Unpin), len: u64, hash: &str, tmp: &FsPath, store: &FsPath) -> ApiResult<()> {
    let mut f = tokio::fs::File::create(tmp).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 << 10];
    let mut left = len;
    while left > 0 {
        let want = left.min(buf.len() as u64) as usize;
        let n = rd.read(&mut buf[..want]).await.map_err(|e| ApiError::bad_request(format!("reading upload: {e}")))?;
        if n == 0 {
            return Err(ApiError::bad_request(format!("upload ended inside blob {hash}")));
        }
        hasher.update(&buf[..n]);
        f.write_all(&buf[..n]).await?;
        left -= n as u64;
    }
    f.flush().await?;
    drop(f);
    if format!("{:x}", hasher.finalize()) != hash {
        return Err(ApiError::bad_request(format!("blob {hash} doesn't match its hash; did the file change during the upload?")));
    }
    let dest = blob_path(store, hash);
    tokio::fs::create_dir_all(dest.parent().unwrap()).await?;
    tokio::fs::rename(tmp, &dest).await?;
    Ok(())
}

async fn check_missing(store: &FsPath, m: &std::sync::Arc<Manifest>) -> ApiResult<Vec<String>> {
    let (store, m) = (store.to_path_buf(), m.clone());
    Ok(blocking(move || find_missing(&store, &m)).await?)
}

async fn install_manifest(State(pool): State<AppState>, L(lease): L, Json(m): Json<Manifest>) -> ApiResult<Json<Value>> {
    let _hold = lease.hold();
    m.validate()?;
    let store = pool.blob_store();
    let m = std::sync::Arc::new(m);
    let missing = check_missing(&store, &m).await?;
    if !missing.is_empty() {
        return Ok(Json(json!({ "missing": missing })));
    }
    let dest = scratch(&lease, "app");
    let root = dest.join(&m.name);
    let (s, m2, r) = (store.clone(), m.clone(), root.clone());
    let built = blocking(move || materialize(&s, &m2, &r)).await;
    let res = match built {
        Ok(()) => install_dir(&lease, &dest, m.total_bytes()).await,
        Err(e) => Err(e.into()),
    };
    let d = dest.clone();
    let _ = blocking(move || remove_tree(&d)).await;
    evict_soon(store, pool.cfg.blob_cache_gb.saturating_mul(1 << 30));
    res
}

/// Where lldb will look for a dSYM. It searches next to each binary, not in any configurable path
/// (`target.debug-file-search-paths` has no effect on dSYMs), so the dSYM has to live beside what
/// it describes: `MyApp.app.dSYM` next to the app, `Foo.framework.dSYM` next to the framework.
fn dsym_dest(app: &FsPath, dsym_name: &str) -> Option<PathBuf> {
    let stem = dsym_name.strip_suffix(".dSYM")?;
    let app_name = app.file_name()?.to_str()?;
    if stem == app_name || Some(stem) == app_name.strip_suffix(".app") {
        return Some(app.parent()?.join(format!("{app_name}.dSYM")));
    }
    find_named(app, stem).map(|found| found.with_file_name(dsym_name))
}

fn find_named(dir: &FsPath, name: &str) -> Option<PathBuf> {
    let mut subdirs = Vec::new();
    for entry in fs::read_dir(dir).ok()?.flatten() {
        if entry.file_name() == name {
            return Some(entry.path());
        }
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if is_dir && entry.path().extension().is_none_or(|e| e != "dSYM") {
            subdirs.push(entry.path());
        }
    }
    subdirs.sort();
    subdirs.iter().find_map(|d| find_named(d, name))
}

/// Puts a dSYM where lldb-dap on this Mac will find it. The location depends on the installed app,
/// so the client sends this after every install.
async fn add_dsym(State(pool): State<AppState>, L(lease): L, Path(p): Path<HashMap<String, String>>, Json(m): Json<Manifest>) -> ApiResult<Json<Value>> {
    let _hold = lease.hold();
    m.validate()?;
    if !m.name.ends_with(".dSYM") {
        return Err(ApiError::bad_request(format!("{} isn't a .dSYM", m.name)));
    }
    let store = pool.blob_store();
    let m = std::sync::Arc::new(m);
    let missing = check_missing(&store, &m).await?;
    if !missing.is_empty() {
        return Ok(Json(json!({ "missing": missing })));
    }
    let bundle = &p["bundle"];
    let app = simctl(&["get_app_container", &lease.udid, bundle, "app"]).await.map_err(|e| ApiError::not_found(format!("{e:#}")))?.text();
    let (app, name) = (PathBuf::from(app), m.name.clone());
    let lookup = blocking(move || Ok(dsym_dest(&app, &name))).await?;
    let dest = lookup.ok_or_else(|| ApiError::bad_request(format!("{bundle} has nothing that {} could describe", m.name)))?;
    let (s, m2, d) = (store.clone(), m.clone(), dest.clone());
    blocking(move || {
        fsutil::remove_tree(&d)?;
        materialize(&s, &m2, &d)
    })
    .await?;
    evict_soon(store, pool.cfg.blob_cache_gb.saturating_mul(1 << 30));
    Ok(Json(json!({ "path": dest })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_of(data: &[u8]) -> String {
        format!("{:x}", Sha256::digest(data))
    }

    fn put_blob(store: &FsPath, data: &[u8]) -> String {
        let hash = hash_of(data);
        let path = blob_path(store, &hash);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
        hash
    }

    fn manifest(v: Value) -> Manifest {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn validate_rejects_paths_that_leave_the_bundle() {
        let hash = "0".repeat(64);
        for bad in ["../evil", "/etc/passwd", "a/../../evil", ""] {
            let m = manifest(json!({"name": "My.app", "files": [{"path": bad, "sha256": hash, "size": 1}]}));
            assert!(m.validate().is_err(), "file {bad:?} should be rejected");
            let m = manifest(json!({"name": "My.app", "dirs": [bad]}));
            assert!(m.validate().is_err(), "dir {bad:?} should be rejected");
            let m = manifest(json!({"name": "My.app", "symlinks": [{"path": bad, "target": "x"}]}));
            assert!(m.validate().is_err(), "symlink {bad:?} should be rejected");
        }
        for bad in ["..", "a/b", "/abs", ""] {
            assert!(manifest(json!({"name": bad})).validate().is_err(), "name {bad:?} should be rejected");
        }
    }

    #[test]
    fn validate_rejects_hashes_that_could_be_paths() {
        for bad in ["../../etc/passwd", "short", &"G".repeat(64), &"A".repeat(64)] {
            let m = manifest(json!({"name": "My.app", "files": [{"path": "f", "sha256": bad, "size": 1}]}));
            assert!(m.validate().is_err(), "hash {bad:?} should be rejected");
        }
    }

    #[test]
    fn validate_rejects_symlinks_nested_in_symlinks() {
        let m = manifest(json!({"name": "My.app", "symlinks": [{"path": "a", "target": "/tmp"}, {"path": "a/b", "target": "x"}]}));
        assert!(m.validate().is_err());
    }

    #[test]
    fn missing_lists_only_absent_blobs_once_and_touches_present_ones() {
        let t = tempfile::tempdir().unwrap();
        let have = put_blob(t.path(), b"have");
        let old = SystemTime::now() - Duration::from_secs(90_000);
        fs::File::options().write(true).open(blob_path(t.path(), &have)).unwrap().set_modified(old).unwrap();
        let absent = hash_of(b"absent");
        let m = manifest(json!({"name": "My.app", "files": [
            {"path": "a", "sha256": have, "size": 4},
            {"path": "b", "sha256": absent, "size": 6},
            {"path": "c", "sha256": absent, "size": 6},
        ]}));
        assert_eq!(find_missing(t.path(), &m).unwrap(), vec![absent]);
        let modified = fs::metadata(blob_path(t.path(), &have)).unwrap().modified().unwrap();
        assert!(SystemTime::now().duration_since(modified).unwrap() < Duration::from_secs(60));
    }

    #[test]
    fn materialize_rebuilds_the_tree_with_writable_modes() {
        let t = tempfile::tempdir().unwrap();
        let store = t.path().join("store");
        let exe = put_blob(&store, b"#!binary");
        let plist = put_blob(&store, b"plist");
        fs::set_permissions(blob_path(&store, &plist), fs::Permissions::from_mode(0o444)).unwrap();
        let root = t.path().join("out/My.app");
        let m = manifest(json!({
            "name": "My.app",
            "dirs": ["Frameworks/Empty.framework"],
            "files": [
                {"path": "My", "sha256": exe, "size": 8, "executable": true},
                {"path": "Sub/Info.plist", "sha256": plist, "size": 5},
                {"path": "Copy.plist", "sha256": plist, "size": 5},
            ],
            "symlinks": [{"path": "Sub/Current", "target": "../My"}],
        }));
        materialize(&store, &m, &root).unwrap();
        let mode = |p: &str| fs::metadata(root.join(p)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode("My"), 0o755);
        assert_eq!(mode("Sub/Info.plist"), 0o644);
        assert_eq!(fs::read(root.join("Copy.plist")).unwrap(), b"plist");
        assert!(root.join("Frameworks/Empty.framework").is_dir());
        assert_eq!(fs::read_link(root.join("Sub/Current")).unwrap(), FsPath::new("../My"));
        // The store's copy isn't shared with the tree: changing one leaves the other intact.
        fs::write(root.join("Copy.plist"), b"changed").unwrap();
        assert_eq!(fs::read(blob_path(&store, &plist)).unwrap(), b"plist");
    }

    #[test]
    fn materialize_refuses_a_path_that_is_both_file_and_symlink() {
        let t = tempfile::tempdir().unwrap();
        let store = t.path().join("store");
        let hash = put_blob(&store, b"x");
        let outside = t.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let m = manifest(json!({
            "name": "My.app",
            "files": [{"path": "a/file", "sha256": hash, "size": 1}],
            "symlinks": [{"path": "a", "target": outside}],
        }));
        assert!(materialize(&store, &m, &t.path().join("out")).is_err());
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
    }

    #[test]
    fn evict_drops_oldest_first_and_spares_recent_blobs() {
        let t = tempfile::tempdir().unwrap();
        let age = |h: &str, secs: u64| {
            let f = fs::File::options().write(true).open(blob_path(t.path(), h)).unwrap();
            f.set_modified(SystemTime::now() - Duration::from_secs(secs)).unwrap();
        };
        let oldest = put_blob(t.path(), &[1u8; 100]);
        let older = put_blob(t.path(), &[2u8; 100]);
        let recent = put_blob(t.path(), &[3u8; 100]);
        age(&oldest, 9000);
        age(&older, 8000);
        let stale_tmp = t.path().join(".tmp-stale");
        fs::write(&stale_tmp, b"x").unwrap();
        fs::File::options().write(true).open(&stale_tmp).unwrap().set_modified(SystemTime::now() - Duration::from_secs(9000)).unwrap();

        evict(t.path(), 200).unwrap();
        assert!(!blob_path(t.path(), &oldest).exists());
        assert!(blob_path(t.path(), &older).exists());
        assert!(!stale_tmp.exists());

        // Over the limit, but what's left is either too recent or the limit is met.
        evict(t.path(), 0).unwrap();
        assert!(!blob_path(t.path(), &older).exists());
        assert!(blob_path(t.path(), &recent).exists());
    }

    #[test]
    fn dsyms_go_where_lldb_looks() {
        let t = tempfile::tempdir().unwrap();
        let app = t.path().join("CONTAINER/My.app");
        fs::create_dir_all(app.join("Frameworks/Foo.framework")).unwrap();
        fs::create_dir_all(app.join("PlugIns/Ext.appex")).unwrap();
        fs::write(app.join("Frameworks/libbar.dylib"), b"").unwrap();
        let parent = t.path().join("CONTAINER");
        assert_eq!(dsym_dest(&app, "My.app.dSYM").unwrap(), parent.join("My.app.dSYM"));
        assert_eq!(dsym_dest(&app, "My.dSYM").unwrap(), parent.join("My.app.dSYM"));
        assert_eq!(dsym_dest(&app, "Foo.framework.dSYM").unwrap(), app.join("Frameworks/Foo.framework.dSYM"));
        assert_eq!(dsym_dest(&app, "Ext.appex.dSYM").unwrap(), app.join("PlugIns/Ext.appex.dSYM"));
        assert_eq!(dsym_dest(&app, "libbar.dylib.dSYM").unwrap(), app.join("Frameworks/libbar.dylib.dSYM"));
        assert!(dsym_dest(&app, "Other.framework.dSYM").is_none());
        assert!(dsym_dest(&app, "NotADsym").is_none());
    }
}
