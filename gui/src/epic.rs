//! Port of `script.ps1`: marks a partially downloaded Epic Games Launcher
//! install as complete so the launcher picks up game files copied in by hand.
//!
//! Unlike the script, every precondition is checked *before* anything on disk
//! is touched, and the manifest edit is rolled back if deleting `bps` fails.

use serde::Serialize;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const DEFAULT_MANIFESTS_DIR: &str = r"C:\ProgramData\Epic\EpicGamesLauncher\Data\Manifests";

/// Where the launcher keeps its install manifests. Can be overridden with the
/// `EPIC_MANIFESTS_DIR` environment variable (handy for testing).
pub fn manifests_dir() -> PathBuf {
    std::env::var_os("EPIC_MANIFESTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_MANIFESTS_DIR))
}

/// One `*.item` file inside `Manifests\Pending`.
#[derive(Clone, Debug)]
pub struct PendingInstall {
    pub item_path: PathBuf,
    pub display_name: String,
    pub folder_name: String,
    pub install_location: PathBuf,
    pub manifest_location: PathBuf,
    pub installation_guid: String,
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

/// Lists every pending install the launcher knows about.
pub fn scan_pending(manifests_dir: &Path) -> Result<Vec<PendingInstall>, String> {
    let pending_dir = manifests_dir.join("Pending");
    let entries = fs::read_dir(&pending_dir)
        .map_err(|e| format!("Cannot read {}: {e}", pending_dir.display()))?;

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
        installs.push(PendingInstall {
            item_path: path,
            display_name,
            folder_name,
            install_location: str_field(&json, "InstallLocation").into(),
            manifest_location: str_field(&json, "ManifestLocation").into(),
            installation_guid: str_field(&json, "InstallationGuid"),
        });
    }
    installs.sort_by(|a, b| a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase()));
    Ok(installs)
}

/// Finds the pending install whose folder name matches the chosen game folder,
/// the same rule the script uses (`Split-Path -Leaf` of the working directory).
pub fn find_for_folder<'a>(installs: &'a [PendingInstall], game_folder: &Path) -> Option<usize> {
    let leaf = game_folder.file_name()?.to_string_lossy();
    installs.iter().position(|i| i.folder_name.eq_ignore_ascii_case(&leaf))
}

/// What `apply` would do, plus anything that stops it from running.
#[derive(Clone, Debug)]
pub struct Plan {
    pub install: PendingInstall,
    pub bps_dir: PathBuf,
    pub pending_files: Vec<PathBuf>,
    pub problems: Vec<String>,
}

impl Plan {
    pub fn is_ready(&self) -> bool {
        self.problems.is_empty()
    }
}

pub fn plan(install: &PendingInstall) -> Plan {
    let mut problems = Vec::new();
    let manifest_location = &install.manifest_location;
    let bps_dir = manifest_location.join("bps");
    let mut pending_files = Vec::new();

    if manifest_location.as_os_str().is_empty() {
        problems.push("Manifest has no ManifestLocation.".into());
    } else if !manifest_location.is_dir() {
        problems.push(format!("{} does not exist.", manifest_location.display()));
    } else {
        if !bps_dir.is_dir() {
            problems.push(format!(
                "{} not found. Start the download in the launcher, let it fetch a few MB, then cancel it.",
                bps_dir.display()
            ));
        }
        if let Ok(entries) = fs::read_dir(manifest_location.join("Pending")) {
            for entry in entries.flatten() {
                let path = entry.path();
                let stem = path.file_stem().map(|s| s.to_string_lossy().to_string());
                if stem.as_deref() == Some(install.installation_guid.as_str()) {
                    pending_files.push(path);
                }
            }
        }
        if pending_files.is_empty() {
            problems.push(format!(
                "No pending files named {} in {}.",
                install.installation_guid,
                manifest_location.join("Pending").display()
            ));
        }
    }

    Plan { install: install.clone(), bps_dir, pending_files, problems }
}

fn write_tab_indented(value: &Value) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"\t");
    let mut ser = serde_json::Serializer::with_formatter(&mut out, formatter);
    value.serialize(&mut ser).map_err(|e| e.to_string())?;
    Ok(out)
}

/// Performs the same steps as `script.ps1`, reporting each one through `log`.
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
    log(format!("Modifying {}", src_item.display()));
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
    log(format!("Moved manifest to {}", dest_item.display()));

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

    log("Done! Open the Epic Games Launcher; the game should show Launch.".into());
    Ok(())
}

/// The launcher rewrites its manifests on exit, so it has to be closed first.
pub fn launcher_running() -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq EpicGamesLauncher.exe", "/NH"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains("epicgameslauncher.exe"))
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
        manifests: PathBuf,
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
            "bIsIncompleteInstall": true
        });
        fs::write(manifests.join("Pending").join("ABC.item"), item.to_string()).unwrap();
        Fixture { root, manifests, egstore }
    }

    #[test]
    fn scans_and_matches_folder() {
        let f = fixture("scan", true);
        let installs = scan_pending(&f.manifests).unwrap();
        assert_eq!(installs.len(), 1);
        assert_eq!(installs[0].display_name, "My Game");
        assert_eq!(find_for_folder(&installs, Path::new(r"D:\Games\mygame")), Some(0));
        assert_eq!(find_for_folder(&installs, Path::new(r"D:\Games\Other")), None);
    }

    #[test]
    fn apply_completes_install() {
        let f = fixture("apply", true);
        let installs = scan_pending(&f.manifests).unwrap();
        let p = plan(&installs[0]);
        assert!(p.is_ready(), "{:?}", p.problems);
        assert_eq!(p.pending_files.len(), 2);

        let mut lines = Vec::new();
        apply(&p, &mut |l| lines.push(l)).unwrap();

        assert!(!f.manifests.join("Pending").join("ABC.item").exists());
        let moved: Value =
            serde_json::from_str(&fs::read_to_string(f.manifests.join("ABC.item")).unwrap()).unwrap();
        assert_eq!(moved["bIsIncompleteInstall"], Value::Bool(false));
        assert_eq!(moved["DisplayName"], "My Game");
        assert!(!f.egstore.join("bps").exists());
        assert!(f.egstore.join("GUID123.manifest").exists());
        assert!(f.egstore.join("GUID123.mancpn").exists());
        assert!(f.egstore.join("Pending").join("OTHER.manifest").exists());
        assert!(lines.last().unwrap().starts_with("Done"));
    }

    #[test]
    fn missing_bps_blocks_without_touching_disk() {
        let f = fixture("nobps", false);
        let installs = scan_pending(&f.manifests).unwrap();
        let p = plan(&installs[0]);
        assert!(!p.is_ready());
        assert!(apply(&p, &mut |_| {}).is_err());
        assert!(f.manifests.join("Pending").join("ABC.item").exists());
        assert!(f.egstore.join("Pending").join("GUID123.manifest").exists());
    }
}
