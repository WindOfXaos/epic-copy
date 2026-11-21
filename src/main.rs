// Hide the console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! A step-by-step wizard that moves a game you already have (from another PC,
//! a USB drive, a backup...) into the Epic Games Launcher without downloading
//! it again. Each step is detected automatically where possible, so the user
//! only has to act when the app can't do it for them.

mod epic;
mod ui;

use epic::{format_bytes, CopyProgress, DirStats, PendingInstall, Plan};
use gpui::{
    point, prelude::*, px, size, App, Application, Bounds, Context, Entity, PathPromptOptions,
    PromptLevel, TitlebarOptions, Window, WindowBounds, WindowOptions,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often the UI re-reads progress from background work.
const TICK: Duration = Duration::from_millis(250);
/// Re-scan Epic's manifests and the launcher process every N ticks (~2s).
const SCAN_EVERY: u64 = 8;

/// Sidebar labels for the wizard steps.
const STEPS: [&str; 5] = ["Start download", "Close Epic", "Choose files", "Copy files", "Finish"];

// ---------------------------------------------------------------------------
// Background work
// ---------------------------------------------------------------------------

type Slot<T> = Arc<Mutex<Option<T>>>;

/// Runs `f` on its own thread; the result shows up in the returned slot.
fn in_background<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Slot<T> {
    let slot = Arc::new(Mutex::new(None));
    let out = slot.clone();
    std::thread::spawn(move || {
        let result = f();
        if let Ok(mut out) = out.lock() {
            *out = Some(result);
        }
    });
    slot
}

fn take<T>(slot: &Slot<T>) -> Option<T> {
    slot.lock().ok().and_then(|mut s| s.take())
}

/// Size of a folder, measured off the UI thread (games are big).
enum Measure {
    Running(Slot<Result<DirStats, String>>),
    Done(Result<DirStats, String>),
}

impl Measure {
    /// Measures `path`; with `target`, also how much of it a copy would overwrite.
    fn start(path: &Path, target: Option<&Path>) -> Self {
        let path = path.to_path_buf();
        let target = target.map(Path::to_path_buf);
        Measure::Running(in_background(move || {
            epic::dir_stats(&path, target.as_deref()).map_err(|e| e.to_string())
        }))
    }

    /// Returns true when the measurement just finished.
    fn poll(&mut self) -> bool {
        if let Measure::Running(slot) = self {
            if let Some(result) = take(slot) {
                *self = Measure::Done(result);
                return true;
            }
        }
        false
    }

    fn stats(&self) -> Option<DirStats> {
        match self {
            Measure::Done(Ok(stats)) => Some(*stats),
            _ => None,
        }
    }
}

struct Source {
    path: PathBuf,
    /// The user picked a parent folder and we stepped into the game folder.
    adjusted_from: Option<PathBuf>,
    /// The files are already in the install folder; nothing to copy.
    in_place: bool,
    measure: Measure,
    has_executable: Option<bool>,
    /// Result of `Wizard::compute_check`, refreshed when something it depends
    /// on changes. Cached because it touches the disk (canonical paths, free
    /// space) and the UI asks for it several times per frame.
    check: SourceCheck,
}

enum CopyState {
    Idle,
    Running { progress: Arc<CopyProgress>, result: Slot<Result<(), String>> },
    Done(u64),
    Failed(String),
}

/// Problems with the chosen game folder. Errors block; warnings need an
/// explicit "use them anyway".
#[derive(Clone, Default)]
struct SourceCheck {
    measuring: bool,
    errors: Vec<String>,
    warnings: Vec<String>,
    notes: Vec<String>,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

enum Cover {
    Loading(Slot<Option<PathBuf>>),
    /// `None`: no artwork available, use the generated tile.
    Ready(Option<PathBuf>),
}

/// Wizard progress for a game that isn't the selected one right now, so
/// switching between games loses nothing.
struct GameProgress {
    source: Option<Source>,
    warnings_accepted: bool,
    copy: CopyState,
    finish_error: Option<String>,
}

struct Wizard {
    manifests_dir: PathBuf,
    /// Every unfinished download in the launcher, and what's on disk for each.
    installs: Vec<PendingInstall>,
    plans: HashMap<PathBuf, Plan>,
    scan_error: Option<String>,
    /// The game the wizard is working on (its `.item` path). The fields
    /// `plan`, `source`, `warnings_accepted`, `copy` and `finish_error`
    /// belong to it; other games' progress waits in `games`.
    selected: Option<PathBuf>,
    plan: Option<Plan>,
    games: HashMap<PathBuf, GameProgress>,
    /// Games finished in this session, and which one's done screen is shown.
    done: Vec<PendingInstall>,
    viewing_done: Option<usize>,
    launcher_running: bool,
    launcher_check: Option<Slot<bool>>,
    closing_launcher: Option<Slot<Result<(), String>>>,
    launcher_error: Option<String>,
    source: Option<Source>,
    warnings_accepted: bool,
    copy: CopyState,
    finish_error: Option<String>,
    /// A message that outlives the game it's about (shown until dismissed).
    notice: Option<String>,
    /// Cover art per catalog item ID.
    covers: HashMap<String, Cover>,
    ticks: u64,
}

impl Wizard {
    fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            manifests_dir: epic::manifests_dir(),
            installs: Vec::new(),
            plans: HashMap::new(),
            scan_error: None,
            selected: None,
            plan: None,
            games: HashMap::new(),
            done: Vec::new(),
            viewing_done: None,
            launcher_running: epic::launcher_running(),
            launcher_check: None,
            closing_launcher: None,
            launcher_error: None,
            source: None,
            warnings_accepted: false,
            copy: CopyState::Idle,
            finish_error: None,
            notice: None,
            covers: HashMap::new(),
            ticks: 0,
        };
        this.rescan();

        // Drive background polling and auto-detection.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(TICK).await;
            let alive = this.update(cx, |this, cx| {
                this.tick();
                cx.notify();
            });
            if alive.is_err() {
                break;
            }
        })
        .detach();
        this
    }

    /// Starts fetching cover art for games we haven't looked up yet.
    fn load_covers(&mut self) {
        for install in &self.installs {
            let Some(id) = install.catalog_item_id.clone() else { continue };
            self.covers.entry(id.clone()).or_insert_with(|| {
                let manifests_dir = self.manifests_dir.clone();
                Cover::Loading(in_background(move || epic::load_cover(&manifests_dir, &id)))
            });
        }
    }

    fn cover_path(&self, install: &PendingInstall) -> Option<PathBuf> {
        match self.covers.get(install.catalog_item_id.as_ref()?)? {
            Cover::Ready(path) => path.clone(),
            Cover::Loading(_) => None,
        }
    }

    fn tick(&mut self) {
        self.ticks += 1;
        for cover in self.covers.values_mut() {
            if let Cover::Loading(slot) = cover {
                if let Some(path) = take(slot) {
                    *cover = Cover::Ready(path);
                }
            }
        }
        let measured = self.source.as_mut().is_some_and(|s| s.measure.poll());
        if measured {
            self.refresh_check();
        }
        if let CopyState::Running { result, .. } = &self.copy {
            if let Some(result) = take(result) {
                self.copy = match result {
                    Ok(()) => CopyState::Done(self.source_stats().map(|s| s.bytes).unwrap_or(0)),
                    Err(e) => CopyState::Failed(e),
                };
            }
        }
        if let Some(slot) = &self.closing_launcher {
            if let Some(result) = take(slot) {
                self.closing_launcher = None;
                self.launcher_error = result.err();
                // Re-check off the UI thread; a check started before the
                // kill could report a stale "running".
                self.launcher_check = Some(in_background(epic::launcher_running));
            }
        }
        if let Some(slot) = &self.launcher_check {
            if let Some(running) = take(slot) {
                self.launcher_check = None;
                if self.closing_launcher.is_none() {
                    self.launcher_running = running;
                }
            }
        }
        if self.ticks % SCAN_EVERY == 0 {
            self.rescan();
        }
    }

    /// Picks up downloads started in the launcher and whether it is running.
    fn rescan(&mut self) {
        if self.launcher_check.is_none() {
            self.launcher_check = Some(in_background(epic::launcher_running));
        }
        match epic::scan_pending(&self.manifests_dir) {
            Ok(installs) => {
                self.plans = installs.iter().map(|i| (i.item_path.clone(), epic::plan(i))).collect();
                self.installs = installs;
                self.scan_error = None;
                self.load_covers();
            }
            // Keep what we knew: a failed scan (say, a file mid-write) must
            // not make every game, and a running copy, disappear.
            Err(e) => self.scan_error = Some(e),
        }
        // Forget progress for downloads that were removed in the launcher.
        let plans = &self.plans;
        self.games.retain(|item, _| plans.contains_key(item));

        match self.selected.clone() {
            Some(selected) => match self.plans.get(&selected) {
                Some(plan) => self.plan = Some(plan.clone()),
                None => {
                    // The download vanished (removed in Epic, or a Finish that
                    // failed partway). Say so instead of silently moving on.
                    let name = self.install().map(|i| i.display_name.clone()).unwrap_or_default();
                    if let Some(error) = self.finish_error.clone() {
                        self.notice = Some(format!("Finishing {name} failed: {error}"));
                    } else if self.copy_running() {
                        self.notice = Some(format!(
                            "{name} is no longer in Epic's downloads, so copying was stopped. Files copied so far are still in its install folder."
                        ));
                    }
                    self.clear_selected();
                }
            },
            // With a single download there's nothing to choose.
            None if self.viewing_done.is_none() && self.installs.len() == 1 => {
                let path = self.installs[0].item_path.clone();
                self.select(path);
            }
            None => {}
        }
    }

    fn copy_running(&self) -> bool {
        matches!(self.copy, CopyState::Running { .. })
    }

    fn clear_selected(&mut self) {
        // Never leave a copy running with nothing on screen to show or stop it.
        self.cancel_copy();
        self.selected = None;
        self.plan = None;
        self.source = None;
        self.warnings_accepted = false;
        self.copy = CopyState::Idle;
        self.finish_error = None;
    }

    /// Parks the selected game's progress so another game can be shown.
    fn stash_selected(&mut self) {
        if let Some(item) = self.selected.clone() {
            let progress = GameProgress {
                source: self.source.take(),
                warnings_accepted: self.warnings_accepted,
                copy: std::mem::replace(&mut self.copy, CopyState::Idle),
                finish_error: self.finish_error.take(),
            };
            self.games.insert(item, progress);
        }
        self.clear_selected();
    }

    /// "Pick a different game": back to the list of downloads.
    fn reset_selection(&mut self) {
        if !self.copy_running() {
            self.stash_selected();
        }
    }

    /// Switches to a game, restoring where it was left. Not while copying.
    fn select(&mut self, item_path: PathBuf) {
        if self.copy_running() {
            return;
        }
        self.stash_selected();
        self.viewing_done = None;
        let Some(plan) = self.plans.get(&item_path).cloned() else { return };
        if let Some(progress) = self.games.remove(&item_path) {
            self.source = progress.source;
            self.warnings_accepted = progress.warnings_accepted;
            self.copy = progress.copy;
            self.finish_error = progress.finish_error;
        }
        self.plan = Some(plan);
        self.selected = Some(item_path);
        // Free space may have changed while this game was parked.
        self.refresh_check();
    }

    /// Shows the done screen of a game finished earlier in this session.
    fn view_done(&mut self, index: usize) {
        if self.copy_running() || index >= self.done.len() {
            return;
        }
        self.stash_selected();
        self.viewing_done = Some(index);
    }

    /// The finished game whose done screen is showing.
    fn done_view(&self) -> Option<&PendingInstall> {
        self.done.get(self.viewing_done?)
    }

    /// The game shown in the side panel: the selected one or a finished one.
    fn shown_install(&self) -> Option<&PendingInstall> {
        self.done_view().or_else(|| self.install())
    }

    /// Short status for a game in the sidebar list.
    fn progress_of(&self, item: &Path) -> Option<&GameProgress> {
        self.games.get(item)
    }

    fn install(&self) -> Option<&PendingInstall> {
        self.plan.as_ref().map(|p| &p.install)
    }

    fn source_stats(&self) -> Option<DirStats> {
        self.source.as_ref().and_then(|s| s.measure.stats())
    }

    fn set_source(&mut self, chosen: PathBuf) {
        let Some(install) = self.install().cloned() else { return };
        let path = epic::resolve_game_folder(&chosen, &install);
        let adjusted_from = (path != chosen).then_some(chosen);
        let in_place = epic::same_folder(&path, &install.install_location);
        let has_executable = install.launch_executable.as_ref().map(|exe| path.join(exe).is_file());
        let target = (!in_place).then_some(install.install_location.as_path());
        self.source = Some(Source {
            measure: Measure::start(&path, target),
            path,
            adjusted_from,
            in_place,
            has_executable,
            check: SourceCheck::default(),
        });
        self.warnings_accepted = false;
        self.copy = CopyState::Idle;
        self.finish_error = None;
        self.refresh_check();
    }

    /// The cached check for the chosen folder (see `Source::check`).
    fn check_source(&self) -> SourceCheck {
        self.source.as_ref().map(|s| s.check.clone()).unwrap_or_default()
    }

    fn refresh_check(&mut self) {
        let check = self.compute_check();
        if let Some(source) = &mut self.source {
            source.check = check;
        }
    }

    fn compute_check(&self) -> SourceCheck {
        let mut check = SourceCheck::default();
        let (Some(source), Some(install)) = (&self.source, self.install()) else { return check };
        let target = &install.install_location;

        if !source.in_place && epic::nested(&source.path, target) {
            check.errors.push(format!(
                "This folder overlaps the install folder ({}). Pick the folder that holds your copy of the game.",
                target.display()
            ));
        }
        if let Some(from) = &source.adjusted_from {
            check.notes.push(format!("Using the \"{}\" folder inside {}.", install.folder_name, from.display()));
        }
        match &source.measure {
            Measure::Running(_) => check.measuring = true,
            Measure::Done(Err(e)) => check.errors.push(format!("Couldn't read this folder: {e}")),
            Measure::Done(Ok(stats)) => {
                if stats.files == 0 {
                    check.errors.push(if source.in_place {
                        "The install folder is empty. Choose the folder where your copy of the game is.".into()
                    } else {
                        "This folder is empty.".into()
                    });
                } else {
                    let pct = install.expected_size.map(|e| stats.bytes as f64 / e as f64);
                    // Only Epic's own partial download is there: nothing to finish
                    // with. A missing exe alone only decides this when the size is
                    // unknown; otherwise it's a warning below, like for a copy.
                    let barely_started = match pct {
                        Some(p) => p < 0.5,
                        None => source.has_executable == Some(false),
                    };
                    if source.in_place && barely_started {
                        check.errors.push(format!(
                            "The install folder only has {} so far, just what Epic started downloading. Choose the folder where your copy of {} is.",
                            format_bytes(stats.bytes),
                            install.display_name
                        ));
                        return check;
                    }
                    if source.has_executable == Some(false) {
                        let exe = install.launch_executable.as_ref().map(|e| e.display().to_string()).unwrap_or_default();
                        check.warnings.push(format!(
                            "The game's program ({exe}) isn't in this folder, so this may not be {}, or not the right folder level.",
                            install.display_name
                        ));
                    }
                    if let (Some(expected), Some(pct)) = (install.expected_size, pct) {
                        if pct < 0.95 {
                            check.warnings.push(format!(
                                "It holds {} but Epic expects about {}. Files may be missing, or it may be an older version.",
                                format_bytes(stats.bytes),
                                format_bytes(expected)
                            ));
                        } else if pct > 1.2 {
                            check.warnings.push(format!(
                                "It holds {} but Epic expects about {}. It may contain other files too.",
                                format_bytes(stats.bytes),
                                format_bytes(expected)
                            ));
                        }
                    }
                    if !source.in_place {
                        // Files already in the install folder get overwritten, so
                        // only the difference needs free space.
                        if let Some(free) = epic::free_space(target) {
                            if free < stats.bytes_needed() {
                                check.errors.push(format!(
                                    "Not enough space on the install drive: {} more needed, {} free.",
                                    format_bytes(stats.bytes_needed()),
                                    format_bytes(free)
                                ));
                            }
                        }
                    }
                }
            }
        }
        check
    }

    fn source_accepted(&self) -> bool {
        if self.source.is_none() {
            return false;
        }
        let check = self.check_source();
        !check.measuring && check.errors.is_empty() && (check.warnings.is_empty() || self.warnings_accepted)
    }

    fn files_ready(&self) -> bool {
        match &self.source {
            Some(source) if source.in_place => self.source_accepted(),
            Some(_) => matches!(self.copy, CopyState::Done(_)),
            None => false,
        }
    }

    /// Which step the user is on, worked out from what's on disk.
    fn current_step(&self) -> usize {
        if matches!(self.copy, CopyState::Running { .. }) {
            return 3;
        }
        match &self.plan {
            Some(plan) if plan.is_ready() => {}
            _ => return 0,
        }
        if self.launcher_running {
            return 1;
        }
        if self.files_ready() {
            return 4;
        }
        if self.source_accepted() && !self.source.as_ref().is_some_and(|s| s.in_place) {
            return 3;
        }
        2
    }

    // --- actions -----------------------------------------------------------

    fn choose_source(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Use this folder".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(folder) = paths.into_iter().next() else { return };
            this.update(cx, |this, cx| {
                this.set_source(folder);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn close_launcher(&mut self) {
        self.launcher_error = None;
        self.closing_launcher = Some(in_background(epic::close_launcher));
    }

    fn start_copy(&mut self) {
        let (Some(source), Some(install)) = (&self.source, self.install()) else { return };
        let src = source.path.clone();
        let dst = install.install_location.clone();
        let progress = Arc::new(CopyProgress::default());
        let worker = progress.clone();
        self.copy = CopyState::Running {
            result: in_background(move || epic::copy_game(&src, &dst, &worker)),
            progress,
        };
    }

    fn cancel_copy(&mut self) {
        if let CopyState::Running { progress, .. } = &self.copy {
            progress.cancel.store(true, Relaxed);
        }
    }

    fn finish(&mut self) {
        self.finish_error = None;
        // Kept current by the background check every ~2s; the Finish step is
        // only shown while it says Epic is closed.
        if self.launcher_running {
            return;
        }
        let Some(plan) = self.plan.clone() else { return };
        if !self.files_ready() {
            self.finish_error = Some("The game files aren't in place yet.".into());
            return;
        }
        match epic::apply(&plan, &mut |_| {}) {
            Ok(()) => {
                // It's no longer a pending download: show its done screen and
                // let the list offer the next one.
                self.clear_selected();
                self.done.push(plan.install);
                self.viewing_done = Some(self.done.len() - 1);
                self.rescan();
            }
            Err(e) => self.finish_error = Some(e),
        }
    }

    /// The download to suggest after finishing one.
    fn next_pending(&self) -> Option<&PendingInstall> {
        self.installs.first()
    }

    /// From a done screen: on to the next download, or back to step 1.
    fn next_game(&mut self) {
        self.viewing_done = None;
        if let Some(next) = self.next_pending().map(|i| i.item_path.clone()) {
            self.select(next);
        }
        self.rescan();
    }
}


/// Closing the window mid-copy would leave a half-copied game behind without
/// a word; ask first.
fn confirm_close_while_copying(window: &mut Window, cx: &mut App, wizard: Entity<Wizard>) {
    window.on_window_should_close(cx, move |window, cx| {
        if !wizard.read(cx).copy_running() {
            return true;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            "A copy is still running",
            Some("Quitting stops it. Files copied so far stay in the install folder; you can copy again later to complete them."),
            &["Stop and quit", "Keep copying"],
            cx,
        );
        let wizard = wizard.clone();
        window
            .spawn(cx, async move |cx| {
                if answer.await == Ok(0) {
                    wizard.update(cx, |wizard, _| wizard.cancel_copy()).ok();
                    cx.update(|window, _| window.remove_window()).ok();
                }
            })
            .detach();
        false
    });
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1080.), px(720.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(720.), px(560.))),
                // Draw our own title bar, like the Epic Games Launcher does.
                titlebar: Some(TitlebarOptions {
                    title: Some("Epic Copy".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(12.), px(12.))),
                }),
                ..Default::default()
            },
            |window, cx| {
                let wizard = cx.new(Wizard::new);
                confirm_close_while_copying(window, cx, wizard.clone());
                wizard
            },
        )
        .expect("failed to open window");
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}
