//! Everything that touches the disk or the launcher. The GUI in `main.rs` only
//! decides *when* to call these.
//!
//! `apply` marks a partially downloaded Epic Games Launcher install as complete
//! so the launcher picks up game files copied in from elsewhere. Every
//! precondition is checked before anything on disk is touched, and the manifest
//! edit is rolled back if deleting `bps` fails.

use serde::Serialize;
use serde_json::Value;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;

pub const DEFAULT_MANIFESTS_DIR: &str = r"C:\ProgramData\Epic\EpicGamesLauncher\Data\Manifests";

/// Where the launcher keeps its install manifests. Can be overridden with the
/// `EPIC_MANIFESTS_DIR` environment variable (handy for testing).
pub fn manifests_dir() -> PathBuf {
    std::env::var_os("EPIC_MANIFESTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_MANIFESTS_DIR))
}

/// One `*.item` file inside `Manifests\Pending`: a download that was started
/// in the launcher but never finished.
#[derive(Clone, Debug)]
pub struct PendingInstall {
    pub item_path: PathBuf,
    pub display_name: String,
    pub folder_name: String,
    pub install_location: PathBuf,
    pub manifest_location: PathBuf,
    pub installation_guid: String,
    /// Size of the full game in bytes, when the manifest says.
    pub expected_size: Option<u64>,
    /// Game executable, relative to the install folder, when the manifest says.
    pub launch_executable: Option<PathBuf>,
    /// Epic store catalog ID, used to find the game's cover art.
    pub catalog_item_id: Option<String>,
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

/// Lists every pending install the launcher knows about.
pub fn scan_pending(manifests_dir: &Path) -> Result<Vec<PendingInstall>, String> {
    let pending_dir = manifests_dir.join("Pending");
    if !manifests_dir.is_dir() {
        return Err(format!(
            "Couldn't find the Epic Games Launcher's data folder ({}). Is the launcher installed?",
            manifests_dir.display()
        ));
    }
    let entries = match fs::read_dir(&pending_dir) {
        Ok(entries) => entries,
        // No Pending folder just means nothing is downloading yet.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("Cannot read {}: {e}", pending_dir.display())),
    };

    let mut installs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else { continue };
        let Ok(json) = serde_json::from_str::<Value>(&text) else { continue };
        let folder_name = str_field(&json, "MandatoryAppFolderName");
        if folder_name.is_empty() {
            continue;
        }
        let display_name = match str_field(&json, "DisplayName") {
            name if name.is_empty() => folder_name.clone(),
            name => name,
        };
        let launch_executable = Some(str_field(&json, "LaunchExecutable"))
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        installs.push(PendingInstall {
            item_path: path,
            display_name,
            folder_name,
            install_location: str_field(&json, "InstallLocation").into(),
            manifest_location: str_field(&json, "ManifestLocation").into(),
            installation_guid: str_field(&json, "InstallationGuid"),
            expected_size: json
                .get("InstallSize")
                .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
                .filter(|&s| s > 0),
            launch_executable,
            catalog_item_id: Some(str_field(&json, "CatalogItemId")).filter(|s| !s.is_empty()),
        });
    }
    installs.sort_by_key(|i| i.display_name.to_lowercase());
    Ok(installs)
}

/// What `apply` needs, plus anything that stops it from running.
#[derive(Clone, Debug)]
pub struct Plan {
    pub install: PendingInstall,
    pub bps_dir: PathBuf,
    pub pending_files: Vec<PathBuf>,
    /// The launcher hasn't downloaded anything yet (no `.egstore\bps`).
    pub waiting_for_download: bool,
    pub problems: Vec<String>,
}

impl Plan {
    pub fn is_ready(&self) -> bool {
        self.problems.is_empty() && !self.waiting_for_download
    }
}

pub fn plan(install: &PendingInstall) -> Plan {
    let mut problems = Vec::new();
    let manifest_location = &install.manifest_location;
    let bps_dir = manifest_location.join("bps");
    let mut pending_files = Vec::new();
    let mut waiting_for_download = false;

    if manifest_location.as_os_str().is_empty() {
        problems.push("Epic's manifest for this game has no ManifestLocation.".into());
    } else if !manifest_location.is_dir() {
        waiting_for_download = true;
    } else {
        waiting_for_download = !bps_dir.is_dir();
        if let Ok(entries) = fs::read_dir(manifest_location.join("Pending")) {
            for entry in entries.flatten() {
                let path = entry.path();
                let stem = path.file_stem().map(|s| s.to_string_lossy().to_string());
                if stem.as_deref() == Some(install.installation_guid.as_str()) {
                    pending_files.push(path);
                }
            }
        }
        if pending_files.is_empty() && !waiting_for_download {
            problems.push(format!(
                "Epic's download data for this game is missing ({} has no files named {}). Remove the game in the launcher and start the download again.",
                manifest_location.join("Pending").display(),
                install.installation_guid,
            ));
        }
    }

    Plan { install: install.clone(), bps_dir, pending_files, waiting_for_download, problems }
}

fn write_tab_indented(value: &Value) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"\t");
    let mut ser = serde_json::Serializer::with_formatter(&mut out, formatter);
    value.serialize(&mut ser).map_err(|e| e.to_string())?;
    Ok(out)
}

/// Marks the install complete in Epic's manifests, reporting each step
/// through `log`:
/// 1. clear `bIsIncompleteInstall` and move the `.item` out of `Pending`,
/// 2. delete `.egstore\bps`,
/// 3. move `.egstore\Pending\<InstallationGuid>.*` into `.egstore`.
pub fn apply(plan: &Plan, log: &mut dyn FnMut(String)) -> Result<(), String> {
    if !plan.is_ready() {
        return Err(plan.problems.join("\n"));
    }
    let install = &plan.install;
    let src_item = &install.item_path;
    let pending_dir = src_item.parent().ok_or("Manifest has no parent folder")?;
    let manifests_dir = pending_dir.parent().ok_or("Pending folder has no parent")?;
    let dest_item = manifests_dir.join(src_item.file_name().ok_or("Manifest has no file name")?);

    // 1. Flag the install as complete and move the manifest out of Pending.
    log(format!("Marking {} as installed", install.display_name));
    let original = fs::read(src_item).map_err(|e| format!("Cannot read manifest: {e}"))?;
    let mut json: Value =
        serde_json::from_slice(&original).map_err(|e| format!("Manifest is not valid JSON: {e}"))?;
    json["bIsIncompleteInstall"] = Value::Bool(false);
    let updated = write_tab_indented(&json)?;
    fs::write(&dest_item, &updated).map_err(|e| format!("Cannot write {}: {e}", dest_item.display()))?;
    if let Err(e) = fs::remove_file(src_item) {
        let _ = fs::remove_file(&dest_item);
        return Err(format!("Cannot move manifest out of Pending: {e}"));
    }

    // 2. Delete the partial-download state. Roll the manifest back on failure.
    log(format!("Deleting {}", plan.bps_dir.display()));
    if let Err(e) = fs::remove_dir_all(&plan.bps_dir) {
        let _ = fs::write(src_item, &original);
        let _ = fs::remove_file(&dest_item);
        log("Rolled back manifest changes.".into());
        return Err(format!("Cannot delete {}: {e}", plan.bps_dir.display()));
    }

    // 3. Promote the pending install files into .egstore.
    for file in &plan.pending_files {
        let dest = install.manifest_location.join(file.file_name().unwrap_or_default());
        log(format!("Moving {}", file.display()));
        if dest.exists() {
            let _ = fs::remove_file(&dest);
        }
        fs::rename(file, &dest).map_err(|e| format!("Cannot move {}: {e}", file.display()))?;
    }

    log("Done.".into());
    Ok(())
}

// ---------------------------------------------------------------------------
// Game files
// ---------------------------------------------------------------------------

fn is_egstore(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().eq_ignore_ascii_case(".egstore")
}

/// Every file under `root` (paths relative to `root`) with its size. The
/// launcher's own `.egstore` folder at the top level is skipped: it belongs to
/// the machine the files came from.
pub fn list_files(root: &Path) -> io::Result<Vec<(PathBuf, u64)>> {
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        for entry in fs::read_dir(root.join(&rel))? {
            let entry = entry?;
            let name = entry.file_name();
            if rel.as_os_str().is_empty() && is_egstore(&name) {
                continue;
            }
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                stack.push(rel.join(&name));
            } else if file_type.is_file() {
                out.push((rel.join(&name), entry.metadata()?.len()));
            }
        }
    }
    Ok(out)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DirStats {
    pub bytes: u64,
    pub files: u64,
}

pub fn dir_stats(root: &Path) -> io::Result<DirStats> {
    let files = list_files(root)?;
    Ok(DirStats { bytes: files.iter().map(|f| f.1).sum(), files: files.len() as u64 })
}

/// People often pick the drive or folder that *contains* the game folder. If
/// `chosen` has a subfolder with the game's folder name, use that instead.
pub fn resolve_game_folder(chosen: &Path, install: &PendingInstall) -> PathBuf {
    let looks_right = |p: &Path| match &install.launch_executable {
        Some(exe) => p.join(exe).is_file(),
        None => false,
    };
    if looks_right(chosen) {
        return chosen.to_path_buf();
    }
    let nested = chosen.join(&install.folder_name);
    if nested.is_dir() && (looks_right(&nested) || install.launch_executable.is_none()) {
        return nested;
    }
    chosen.to_path_buf()
}

/// Canonical form of `p`, even when its last parts don't exist yet.
fn canonical(p: &Path) -> PathBuf {
    let mut existing = p;
    let mut rest = Vec::new();
    loop {
        if let Ok(base) = fs::canonicalize(existing) {
            return rest.iter().rev().fold(base, |acc, part| acc.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                existing = parent;
            }
            _ => return p.to_path_buf(),
        }
    }
}

pub fn same_folder(a: &Path, b: &Path) -> bool {
    canonical(a) == canonical(b)
}

/// One folder is inside the other (but they are not the same folder).
pub fn nested(a: &Path, b: &Path) -> bool {
    let (a, b) = (canonical(a), canonical(b));
    a != b && (a.starts_with(&b) || b.starts_with(&a))
}

#[derive(Default)]
pub struct CopyProgress {
    pub total_bytes: AtomicU64,
    pub copied_bytes: AtomicU64,
    pub total_files: AtomicU64,
    pub copied_files: AtomicU64,
    pub current: Mutex<String>,
    pub cancel: AtomicBool,
}

pub const CANCELLED: &str = "Copy cancelled.";

/// Copies a game folder into the install location, overwriting what is there
/// and skipping `.egstore`. Progress is published through `progress`.
pub fn copy_game(src: &Path, dst: &Path, progress: &CopyProgress) -> Result<(), String> {
    let files = list_files(src).map_err(|e| format!("Cannot read {}: {e}", src.display()))?;
    progress.total_files.store(files.len() as u64, Relaxed);
    progress.total_bytes.store(files.iter().map(|f| f.1).sum(), Relaxed);

    let mut buf = vec![0u8; 4 << 20];
    for (rel, _) in &files {
        if progress.cancel.load(Relaxed) {
            return Err(CANCELLED.into());
        }
        if let Ok(mut current) = progress.current.lock() {
            *current = rel.display().to_string();
        }
        let from = src.join(rel);
        let to = dst.join(rel);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("Cannot create {}: {e}", parent.display()))?;
        }
        copy_file(&from, &to, &mut buf, progress)?;
        progress.copied_files.fetch_add(1, Relaxed);
    }
    Ok(())
}

fn copy_file(from: &Path, to: &Path, buf: &mut [u8], progress: &CopyProgress) -> Result<(), String> {
    let mut reader = fs::File::open(from).map_err(|e| format!("Cannot open {}: {e}", from.display()))?;
    if let Ok(meta) = fs::metadata(to) {
        let mut perms = meta.permissions();
        if perms.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            let _ = fs::set_permissions(to, perms);
        }
    }
    let mut writer = fs::File::create(to).map_err(|e| format!("Cannot write {}: {e}", to.display()))?;
    loop {
        if progress.cancel.load(Relaxed) {
            return Err(CANCELLED.into());
        }
        let n = reader.read(buf).map_err(|e| format!("Cannot read {}: {e}", from.display()))?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n]).map_err(|e| format!("Cannot write {}: {e}", to.display()))?;
        progress.copied_bytes.fetch_add(n as u64, Relaxed);
    }
    Ok(())
}

/// Free space on the drive holding `path` (or its nearest existing parent).
pub fn free_space(path: &Path) -> Option<u64> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let mut existing = path;
        while !existing.exists() {
            existing = existing.parent()?;
        }
        let wide: Vec<u16> = existing.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut free = 0u64;
        // SAFETY: `wide` is NUL-terminated and outlives the call; the other
        // out-parameters are optional and passed as null.
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(free)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

// ---------------------------------------------------------------------------
// Cover art
// ---------------------------------------------------------------------------

/// Image types in the launcher's catalog, best fit for a library tile first.
const COVER_TYPES: [&str; 4] = ["DieselGameBoxTall", "OfferImageTall", "Thumbnail", "DieselGameBox"];

/// Looks up a game's cover URL in the launcher's local catalog cache
/// (`Data\Catalog\catcache.bin`, base64-encoded JSON).
pub fn cover_url(catalog_cache: &[u8], item_id: &str) -> Option<String> {
    use base64::Engine;
    let text: Vec<u8> = catalog_cache.iter().copied().filter(|b| !b.is_ascii_whitespace()).collect();
    let json = base64::engine::general_purpose::STANDARD.decode(text).ok()?;
    let items: Vec<Value> = serde_json::from_slice(&json).ok()?;
    let item = items.iter().find(|i| i.get("id").and_then(Value::as_str) == Some(item_id))?;
    let images = item.get("keyImages")?.as_array()?;
    COVER_TYPES.iter().find_map(|wanted| {
        images
            .iter()
            .find(|img| img.get("type").and_then(Value::as_str) == Some(*wanted))
            .and_then(|img| img.get("url")?.as_str().map(str::to_string))
    })
}

/// Where downloaded covers are kept, so they only download once.
pub fn cover_cache_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("EpicCopy")
        .join("covers")
}

fn image_extension(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("jpg"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("webp"),
        _ => None,
    }
}

fn cached_cover(dir: &Path, item_id: &str) -> Option<PathBuf> {
    ["jpg", "png", "webp"].iter().map(|ext| dir.join(format!("{item_id}.{ext}"))).find(|p| p.is_file())
}

/// Returns a local file with the game's cover art: from the cache, or looked
/// up in the launcher's catalog and downloaded from Epic's CDN. `None` when
/// the game has no catalog entry or the download fails (the UI then falls
/// back to a generated tile).
pub fn load_cover(manifests_dir: &Path, item_id: &str) -> Option<PathBuf> {
    // IDs come from a file on disk; keep them safe to use as a file name.
    if item_id.is_empty() || !item_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return None;
    }
    let dir = cover_cache_dir();
    if let Some(path) = cached_cover(&dir, item_id) {
        return Some(path);
    }
    let catalog = fs::read(manifests_dir.parent()?.join("Catalog").join("catcache.bin")).ok()?;
    let url = cover_url(&catalog, item_id)?;
    if !url.starts_with("https://") {
        return None;
    }
    // Ask the CDN for a tile-sized image instead of the full 1200x1600 one.
    let sized = format!("{url}{}h=640&quality=high&resize=1&w=480", if url.contains('?') { '&' } else { '?' });
    let bytes = download(&sized).or_else(|| download(&url))?;
    let ext = image_extension(&bytes)?;
    fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{item_id}.{ext}"));
    let tmp = dir.join(format!("{item_id}.part"));
    fs::write(&tmp, &bytes).ok()?;
    fs::rename(&tmp, &path).ok()?;
    Some(path)
}

fn download(url: &str) -> Option<Vec<u8>> {
    let response = ureq::get(url).timeout(std::time::Duration::from_secs(15)).call().ok()?;
    let mut bytes = Vec::new();
    response.into_reader().take(16 << 20).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

// ---------------------------------------------------------------------------
// Launcher process
// ---------------------------------------------------------------------------

fn hidden(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// The launcher rewrites its manifests on exit, so it has to be closed first.
pub fn launcher_running() -> bool {
    if !cfg!(windows) {
        return false;
    }
    hidden(Command::new("tasklist").args(["/FI", "IMAGENAME eq EpicGamesLauncher.exe", "/NH"]))
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains("epicgameslauncher.exe"))
        .unwrap_or(false)
}

/// Closes the launcher and its helper processes (including the tray icon).
pub fn close_launcher() -> Result<(), String> {
    let out = hidden(Command::new("taskkill").args(["/IM", "EpicGamesLauncher.exe", "/T", "/F"]))
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() || !launcher_running() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

pub fn open_launcher() -> Result<(), String> {
    hidden(Command::new("cmd").args(["/C", "start", "", "com.epicgames.launcher://store"]))
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Couldn't open the Epic Games Launcher: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
        manifests: PathBuf,
        game: PathBuf,
        egstore: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn fixture(name: &str, with_bps: bool) -> Fixture {
        let root = std::env::temp_dir().join(format!("epic-copy-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let manifests = root.join("Manifests");
        let game = root.join("Games").join("MyGame");
        let egstore = game.join(".egstore");
        fs::create_dir_all(manifests.join("Pending")).unwrap();
        fs::create_dir_all(egstore.join("Pending")).unwrap();
        if with_bps {
            fs::create_dir_all(egstore.join("bps")).unwrap();
            fs::write(egstore.join("bps").join("chunk.bp"), "x").unwrap();
        }
        fs::write(egstore.join("Pending").join("GUID123.manifest"), "m").unwrap();
        fs::write(egstore.join("Pending").join("GUID123.mancpn"), "c").unwrap();
        fs::write(egstore.join("Pending").join("OTHER.manifest"), "o").unwrap();
        let item = serde_json::json!({
            "DisplayName": "My Game",
            "MandatoryAppFolderName": "MyGame",
            "InstallLocation": game,
            "ManifestLocation": egstore,
            "InstallationGuid": "GUID123",
            "InstallSize": 11,
            "LaunchExecutable": "Bin/game.exe",
            "bIsIncompleteInstall": true
        });
        fs::write(manifests.join("Pending").join("ABC.item"), item.to_string()).unwrap();
        Fixture { root, manifests, game, egstore }
    }

    /// A copy of the game somewhere else, e.g. on a USB drive.
    fn source_game(f: &Fixture) -> PathBuf {
        let src = f.root.join("Usb").join("MyGame");
        fs::create_dir_all(src.join("Bin")).unwrap();
        fs::create_dir_all(src.join(".egstore")).unwrap();
        fs::write(src.join("Bin").join("game.exe"), "exe").unwrap();
        fs::write(src.join("data.pak"), "12345678").unwrap();
        fs::write(src.join(".egstore").join("old.manifest"), "from other pc").unwrap();
        src
    }

    #[test]
    fn scans_pending_installs() {
        let f = fixture("scan", true);
        let installs = scan_pending(&f.manifests).unwrap();
        assert_eq!(installs.len(), 1);
        assert_eq!(installs[0].display_name, "My Game");
        assert_eq!(installs[0].expected_size, Some(11));
        assert_eq!(installs[0].launch_executable.as_deref(), Some(Path::new("Bin/game.exe")));
    }

    #[test]
    fn missing_pending_folder_is_not_an_error() {
        let f = fixture("nopending", true);
        fs::remove_dir_all(f.manifests.join("Pending")).unwrap();
        assert!(scan_pending(&f.manifests).unwrap().is_empty());
        assert!(scan_pending(&f.root.join("nope")).is_err());
    }

    #[test]
    fn copies_game_and_skips_egstore() {
        let f = fixture("copy", true);
        let src = source_game(&f);
        let stats = dir_stats(&src).unwrap();
        assert_eq!((stats.files, stats.bytes), (2, 11));

        let progress = CopyProgress::default();
        copy_game(&src, &f.game, &progress).unwrap();
        assert_eq!(progress.copied_bytes.load(Relaxed), 11);
        assert_eq!(progress.copied_files.load(Relaxed), 2);
        assert!(f.game.join("Bin").join("game.exe").is_file());
        assert!(f.game.join("data.pak").is_file());
        assert!(!f.egstore.join("old.manifest").exists());
        assert!(f.egstore.join("bps").is_dir());
    }

    #[test]
    fn cancel_stops_copy() {
        let f = fixture("cancel", true);
        let src = source_game(&f);
        let progress = CopyProgress::default();
        progress.cancel.store(true, Relaxed);
        assert_eq!(copy_game(&src, &f.game, &progress).unwrap_err(), CANCELLED);
    }

    #[test]
    fn resolves_parent_folder_to_game_folder() {
        let f = fixture("resolve", true);
        let src = source_game(&f);
        let install = &scan_pending(&f.manifests).unwrap()[0];
        assert_eq!(resolve_game_folder(src.parent().unwrap(), install), src);
        assert_eq!(resolve_game_folder(&src, install), src);
        assert!(nested(&f.game, &f.game.join("Bin")));
        assert!(same_folder(&f.game, &f.root.join("Games").join("MyGame")));
    }

    #[test]
    fn apply_completes_install() {
        let f = fixture("apply", true);
        let installs = scan_pending(&f.manifests).unwrap();
        let p = plan(&installs[0]);
        assert!(p.is_ready(), "{:?}", p.problems);
        assert_eq!(p.pending_files.len(), 2);

        apply(&p, &mut |_| {}).unwrap();

        assert!(!f.manifests.join("Pending").join("ABC.item").exists());
        let moved: Value =
            serde_json::from_str(&fs::read_to_string(f.manifests.join("ABC.item")).unwrap()).unwrap();
        assert_eq!(moved["bIsIncompleteInstall"], Value::Bool(false));
        assert_eq!(moved["DisplayName"], "My Game");
        assert!(!f.egstore.join("bps").exists());
        assert!(f.egstore.join("GUID123.manifest").exists());
        assert!(f.egstore.join("GUID123.mancpn").exists());
        assert!(f.egstore.join("Pending").join("OTHER.manifest").exists());
    }

    #[test]
    fn missing_bps_waits_without_touching_disk() {
        let f = fixture("nobps", false);
        let installs = scan_pending(&f.manifests).unwrap();
        let p = plan(&installs[0]);
        assert!(p.waiting_for_download);
        assert!(!p.is_ready());
        assert!(apply(&p, &mut |_| {}).is_err());
        assert!(f.manifests.join("Pending").join("ABC.item").exists());
        assert!(f.egstore.join("Pending").join("GUID123.manifest").exists());
    }

    #[test]
    fn finds_cover_in_catalog_cache() {
        use base64::Engine;
        let catalog = serde_json::json!([
            { "id": "other", "keyImages": [{ "type": "DieselGameBoxTall", "url": "https://x/other.png" }] },
            { "id": "abc123", "keyImages": [
                { "type": "DieselGameBox", "url": "https://x/wide.png" },
                { "type": "DieselGameBoxTall", "url": "https://x/tall.png" }
            ] },
            { "id": "wideonly", "keyImages": [{ "type": "DieselGameBox", "url": "https://x/wide2.png" }] }
        ]);
        let encoded = base64::engine::general_purpose::STANDARD.encode(catalog.to_string());
        assert_eq!(cover_url(encoded.as_bytes(), "abc123").as_deref(), Some("https://x/tall.png"));
        assert_eq!(cover_url(encoded.as_bytes(), "wideonly").as_deref(), Some("https://x/wide2.png"));
        assert_eq!(cover_url(encoded.as_bytes(), "missing"), None);
        assert_eq!(cover_url(b"not base64!", "abc123"), None);
        assert_eq!(image_extension(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("jpg"));
        assert_eq!(image_extension(b"<html>"), None);
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(25 * 1024 * 1024 * 1024), "25.0 GB");
    }
}
