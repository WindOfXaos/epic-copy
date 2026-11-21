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
    point, prelude::*, px, size, App, Application, Bounds, Context, PathPromptOptions,
    TitlebarOptions, WindowBounds, WindowOptions,
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
    fn start(path: &Path) -> Self {
        let path = path.to_path_buf();
        Measure::Running(in_background(move || epic::dir_stats(&path).map_err(|e| e.to_string())))
    }

    fn poll(&mut self) {
        if let Measure::Running(slot) = self {
            if let Some(result) = take(slot) {
                *self = Measure::Done(result);
            }
        }
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
}

enum CopyState {
    Idle,
    Running { progress: Arc<CopyProgress>, result: Slot<Result<(), String>> },
    Done(u64),
    Failed(String),
}

/// Problems with the chosen game folder. Errors block; warnings need an
/// explicit "use them anyway".
#[derive(Default)]
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

struct Wizard {
    manifests_dir: PathBuf,
    installs: Vec<PendingInstall>,
    scan_error: Option<String>,
    selected: Option<PathBuf>,
    plan: Option<Plan>,
    launcher_running: bool,
    launcher_check: Option<Slot<bool>>,
    closing_launcher: Option<Slot<Result<(), String>>>,
    launcher_error: Option<String>,
    source: Option<Source>,
    warnings_accepted: bool,
    copy: CopyState,
    finish_error: Option<String>,
    finished: Option<String>,
    /// Cover art per catalog item ID.
    covers: HashMap<String, Cover>,
    ticks: u64,
}

impl Wizard {
    fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            manifests_dir: epic::manifests_dir(),
            installs: Vec::new(),
            scan_error: None,
            selected: None,
            plan: None,
            launcher_running: epic::launcher_running(),
            launcher_check: None,
            closing_launcher: None,
            launcher_error: None,
            source: None,
            warnings_accepted: false,
            copy: CopyState::Idle,
            finish_error: None,
            finished: None,
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
        if let Some(source) = &mut self.source {
            source.measure.poll();
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
                self.launcher_running = epic::launcher_running();
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
        if self.finished.is_some() {
            return;
        }
        if self.launcher_check.is_none() {
            self.launcher_check = Some(in_background(epic::launcher_running));
        }
        match epic::scan_pending(&self.manifests_dir) {
            Ok(installs) => {
                self.installs = installs;
                self.scan_error = None;
                self.load_covers();
            }
            Err(e) => {
                self.installs.clear();
                self.scan_error = Some(e);
            }
        }
        let still_there = self
            .selected
            .as_ref()
            .and_then(|sel| self.installs.iter().find(|i| &i.item_path == sel))
            .cloned();
        match still_there {
            Some(install) => self.plan = Some(epic::plan(&install)),
            None if self.installs.len() == 1 => {
                let path = self.installs[0].item_path.clone();
                self.select(path);
            }
            None => {
                if self.selected.is_some() {
                    self.reset_selection();
                }
            }
        }
    }

    fn reset_selection(&mut self) {
        self.selected = None;
        self.plan = None;
        self.source = None;
        self.warnings_accepted = false;
        self.copy = CopyState::Idle;
        self.finish_error = None;
    }

    fn select(&mut self, item_path: PathBuf) {
        self.reset_selection();
        if let Some(install) = self.installs.iter().find(|i| i.item_path == item_path) {
            self.plan = Some(epic::plan(install));
            self.selected = Some(item_path);
        }
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
        self.source = Some(Source {
            measure: Measure::start(&path),
            path,
            adjusted_from,
            in_place,
            has_executable,
        });
        self.warnings_accepted = false;
        self.copy = CopyState::Idle;
        self.finish_error = None;
    }

    fn check_source(&self) -> SourceCheck {
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
                    // Only Epic's own partial download is there: nothing to finish with.
                    let barely_started = pct.is_some_and(|p| p < 0.5) || source.has_executable == Some(false);
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
                        if let Some(free) = epic::free_space(target) {
                            if free < stats.bytes {
                                check.errors.push(format!(
                                    "Not enough space on the install drive: {} needed, {} free.",
                                    format_bytes(stats.bytes),
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
        self.launcher_running = epic::launcher_running();
        if self.launcher_running {
            return;
        }
        let Some(plan) = self.plan.clone() else { return };
        if !self.files_ready() {
            self.finish_error = Some("The game files aren't in place yet.".into());
            return;
        }
        match epic::apply(&plan, &mut |_| {}) {
            Ok(()) => self.finished = Some(plan.install.display_name.clone()),
            Err(e) => self.finish_error = Some(e),
        }
    }

    fn start_over(&mut self) {
        self.finished = None;
        self.reset_selection();
        self.rescan();
    }
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
            |_, cx| cx.new(Wizard::new),
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
