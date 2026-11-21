// Hide the console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! A step-by-step wizard that moves a game you already have (from another PC,
//! a USB drive, a backup...) into the Epic Games Launcher without downloading
//! it again. Each step is detected automatically where possible, so the user
//! only has to act when the app can't do it for them.

mod epic;

use epic::{format_bytes, CopyProgress, DirStats, PendingInstall, Plan};
use gpui::{
    div, prelude::*, px, relative, rgb, size, AnyElement, App, Application, Bounds, ClickEvent,
    Context, Div, FontWeight, Hsla, PathPromptOptions, Rgba, SharedString, Stateful,
    TitlebarOptions, Window, WindowBounds, WindowOptions,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::Duration;

// Palette
const BG: u32 = 0x16181d;
const SIDEBAR: u32 = 0x1b1e24;
const PANEL: u32 = 0x1f2229;
const PANEL_HOVER: u32 = 0x2a2e37;
const BORDER: u32 = 0x343944;
const TEXT: u32 = 0xe6e8ee;
const MUTED: u32 = 0x9aa1ad;
const ACCENT: u32 = 0x3b82f6;
const ACCENT_HOVER: u32 = 0x2563eb;
const OK: u32 = 0x22c55e;
const WARN: u32 = 0xf59e0b;
const ERR: u32 = 0xef4444;

/// How often the UI re-reads progress from background work.
const TICK: Duration = Duration::from_millis(250);
/// Re-scan Epic's manifests and the launcher process every N ticks (~2s).
const SCAN_EVERY: u64 = 8;

const STEPS: [&str; 5] = [
    "Start the download in Epic",
    "Close the Epic Games Launcher",
    "Choose your game files",
    "Copy the files",
    "Finish",
];

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

    fn tick(&mut self) {
        self.ticks += 1;
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

// ---------------------------------------------------------------------------
// Building blocks
// ---------------------------------------------------------------------------

fn hsla(color: u32) -> Hsla {
    Rgba::from(rgb(color)).into()
}

#[derive(Clone, Copy, PartialEq)]
enum Style {
    Primary,
    Secondary,
    Warning,
}

fn button(id: &'static str, label: impl Into<SharedString>, style: Style) -> Stateful<Div> {
    let (bg, hover, fg) = match style {
        Style::Primary => (ACCENT, ACCENT_HOVER, TEXT),
        Style::Secondary => (PANEL_HOVER, BORDER, TEXT),
        Style::Warning => (WARN, WARN, BG),
    };
    div()
        .id(id)
        .flex_none()
        .px_4()
        .py_2()
        .rounded_md()
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .bg(rgb(bg))
        .text_color(rgb(fg))
        .cursor_pointer()
        .hover(move |s| s.bg(rgb(hover)))
        .child(label.into())
}

fn heading(text: impl Into<SharedString>) -> Div {
    div().text_xl().font_weight(FontWeight::BOLD).text_color(rgb(TEXT)).child(text.into())
}

fn para(text: impl Into<SharedString>) -> Div {
    div().text_sm().text_color(rgb(MUTED)).child(text.into())
}

fn card() -> Div {
    div().flex().flex_col().gap_2().p_4().rounded_md().bg(rgb(PANEL)).border_1().border_color(rgb(BORDER))
}

fn kv(key: &'static str, value: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .gap_2()
        .text_sm()
        .child(div().w(px(110.)).flex_none().text_color(rgb(MUTED)).child(key))
        .child(div().flex_1().min_w_0().text_color(rgb(TEXT)).child(value.into()))
}

fn callout(color: u32, text: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .gap_3()
        .items_start()
        .px_3()
        .py_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(color))
        .bg(hsla(color).opacity(0.10))
        .child(div().flex_none().mt(px(5.)).size(px(8.)).rounded_full().bg(rgb(color)))
        .child(div().flex_1().min_w_0().text_sm().text_color(rgb(TEXT)).child(text.into()))
}

fn numbered(n: usize, text: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .gap_3()
        .items_start()
        .child(
            div()
                .flex_none()
                .size(px(22.))
                .rounded_full()
                .bg(rgb(PANEL_HOVER))
                .flex()
                .items_center()
                .justify_center()
                .text_xs()
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(TEXT))
                .child(n.to_string()),
        )
        .child(div().flex_1().min_w_0().pt(px(2.)).text_sm().text_color(rgb(TEXT)).child(text.into()))
}

fn waiting(ticks: u64, text: &str) -> Div {
    let dots = ".".repeat((ticks / 2 % 4) as usize);
    div()
        .flex()
        .items_center()
        .gap_2()
        .text_sm()
        .text_color(rgb(MUTED))
        .child(div().size(px(8.)).rounded_full().bg(rgb(if ticks / 2 % 2 == 0 { ACCENT } else { BORDER })))
        .child(format!("{text}{dots}"))
}

fn progress_bar(fraction: f32) -> Div {
    div()
        .w_full()
        .h(px(10.))
        .rounded_full()
        .bg(rgb(PANEL_HOVER))
        .child(div().h_full().rounded_full().bg(rgb(ACCENT)).w(relative(fraction.clamp(0.0, 1.0))))
}

fn size_and_files(stats: DirStats) -> String {
    let files = if stats.files == 1 { "1 file".to_string() } else { format!("{} files", stats.files) };
    format!("{} in {files}", format_bytes(stats.bytes))
}

fn row() -> Div {
    div().flex().flex_wrap().gap_2().items_center()
}

// ---------------------------------------------------------------------------
// Steps
// ---------------------------------------------------------------------------

impl Wizard {
    fn step_summary(&self, step: usize) -> Option<String> {
        let install = self.install()?;
        Some(match step {
            0 => install.display_name.clone(),
            1 => "Closed".into(),
            2 => match &self.source {
                Some(s) if s.in_place => "Already in the install folder".into(),
                Some(s) => s.path.display().to_string(),
                None => return None,
            },
            3 => match (&self.source, &self.copy) {
                (Some(s), _) if s.in_place => "Not needed".into(),
                (_, CopyState::Done(bytes)) => format!("Copied {}", format_bytes(*bytes)),
                _ => return None,
            },
            _ => return None,
        })
    }

    fn render_sidebar(&self) -> Div {
        let current = if self.finished.is_some() { STEPS.len() } else { self.current_step() };
        let mut list = div().flex().flex_col().gap_1();
        for (i, title) in STEPS.iter().enumerate() {
            let (dot_bg, dot_fg, label, title_color) = if i < current {
                (OK, BG, "\u{2713}".to_string(), TEXT)
            } else if i == current {
                (ACCENT, TEXT, (i + 1).to_string(), TEXT)
            } else {
                (PANEL_HOVER, MUTED, (i + 1).to_string(), MUTED)
            };
            let summary = (i < current).then(|| self.step_summary(i)).flatten();
            list = list.child(
                div()
                    .flex()
                    .gap_3()
                    .items_start()
                    .p_2()
                    .rounded_md()
                    .when(i == current, |el| el.bg(rgb(PANEL)))
                    .child(
                        div()
                            .flex_none()
                            .size(px(24.))
                            .rounded_full()
                            .bg(rgb(dot_bg))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_xs()
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgb(dot_fg))
                            .child(label),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .pt(px(3.))
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(if i == current { FontWeight::SEMIBOLD } else { FontWeight::NORMAL })
                                    .text_color(rgb(title_color))
                                    .child(*title),
                            )
                            .children(summary.map(|s| div().text_xs().text_color(rgb(MUTED)).child(s))),
                    ),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap_4()
            .w(px(270.))
            .flex_none()
            .p_4()
            .bg(rgb(SIDEBAR))
            .border_r_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_lg().font_weight(FontWeight::BOLD).text_color(rgb(TEXT)).child("Epic Copy"))
                    .child(div().text_xs().text_color(rgb(MUTED)).child(
                        "Add a game you already have to the Epic Games Launcher without downloading it again.",
                    )),
            )
            .child(list)
    }

    fn render_download_step(&self, cx: &mut Context<Self>) -> Div {
        let mut body = div().flex().flex_col().gap_4().child(heading("Start the download in Epic"));

        if let Some(err) = &self.scan_error {
            return body.child(callout(ERR, err.clone()));
        }

        let open_button = button("open-epic", "Open Epic Games Launcher", Style::Primary)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.launcher_error = epic::open_launcher().err();
                cx.notify();
            }));

        // Several unfinished downloads and none picked yet.
        if self.plan.is_none() && self.installs.len() > 1 {
            body = body.child(para("Epic has several unfinished downloads. Which game are you adding?"));
            let mut list = div().flex().flex_col().gap_2();
            for (i, install) in self.installs.iter().enumerate() {
                let path = install.item_path.clone();
                list = list.child(
                    div()
                        .id(("install", i))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .p_3()
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(BORDER))
                        .bg(rgb(PANEL))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(PANEL_HOVER)).border_color(rgb(ACCENT)))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.select(path.clone());
                            cx.notify();
                        }))
                        .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(rgb(TEXT)).child(install.display_name.clone()))
                        .child(div().text_xs().text_color(rgb(MUTED)).child(install.install_location.display().to_string())),
                );
            }
            return body.child(list);
        }

        match &self.plan {
            // Nothing started yet: explain exactly what to do in the launcher.
            None => body
                .child(para(
                    "Epic needs to know about the game before it can use your files. Start the download, then stop it straight away:",
                ))
                .child(
                    card()
                        .gap_3()
                        .child(numbered(1, "Open the Epic Games Launcher and go to your Library."))
                        .child(numbered(2, "Click the game and press Install. Choose the drive and folder where you want it, then confirm."))
                        .child(numbered(3, "Let it download about 10 MB, then click the X next to the progress bar to cancel the download."))
                        .child(numbered(4, "Come back here. This page notices the download by itself.")),
                )
                .child(row().child(open_button))
                .children(self.launcher_error.clone().map(|e| callout(ERR, e)))
                .child(waiting(self.ticks, "Waiting for a download to start")),

            Some(plan) => {
                let install = &plan.install;
                body = body.child(
                    card()
                        .child(div().text_lg().font_weight(FontWeight::BOLD).text_color(rgb(TEXT)).child(install.display_name.clone()))
                        .child(kv("Installing to", install.install_location.display().to_string()))
                        .children(install.expected_size.map(|s| kv("Game size", format_bytes(s)))),
                );
                if plan.waiting_for_download {
                    body = body
                        .child(para(
                            "Epic hasn't downloaded anything for this game yet. In the launcher, let the download run until about 10 MB is done, then cancel it with the X.",
                        ))
                        .child(row().child(open_button))
                        .child(waiting(self.ticks, "Waiting for Epic to download a little data"));
                }
                for problem in &plan.problems {
                    body = body.child(callout(ERR, problem.clone()));
                }
                if self.installs.len() > 1 {
                    body = body.child(row().child(
                        button("other-game", "Pick a different game", Style::Secondary).on_click(cx.listener(
                            |this, _: &ClickEvent, _, cx| {
                                this.reset_selection();
                                cx.notify();
                            },
                        )),
                    ));
                }
                body
            }
        }
    }

    fn render_close_step(&self, cx: &mut Context<Self>) -> Div {
        let closing = self.closing_launcher.is_some();
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(heading("Close the Epic Games Launcher"))
            .child(para(
                "Epic has what it needs. Now it has to be fully closed, including the icon in the system tray, otherwise it would undo the changes when it exits.",
            ))
            .child(if closing {
                waiting(self.ticks, "Closing Epic")
            } else {
                row().child(
                    button("close-epic", "Close it for me", Style::Primary)
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.close_launcher();
                            cx.notify();
                        })),
                )
            })
            .children(self.launcher_error.clone().map(|e| {
                callout(ERR, format!("Couldn't close it: {e}. Right-click the Epic icon in the system tray and choose Exit."))
            }))
            .child(para("You can also exit it yourself; this step moves on as soon as Epic is closed."))
    }

    fn render_source_step(&self, cx: &mut Context<Self>) -> Div {
        let Some(install) = self.install() else { return div() };
        let mut body = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(heading("Where are your game files?"))
            .child(para(format!(
                "Choose the {} folder you brought from the other PC, USB drive or backup. It's usually called \"{}\" and contains the game's files and folders. Epic Copy copies it into place for you.",
                install.display_name, install.folder_name
            )));

        let choose = button("choose", if self.source.is_some() { "Choose a different folder" } else { "Choose game folder" }, if self.source.is_some() { Style::Secondary } else { Style::Primary })
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.choose_source(cx)));
        let in_place = button("in-place", "The files are already in the install folder", Style::Secondary)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                if let Some(target) = this.install().map(|i| i.install_location.clone()) {
                    this.set_source(target);
                }
                cx.notify();
            }));

        let Some(source) = &self.source else {
            return body.child(row().child(choose).child(in_place));
        };

        let check = self.check_source();
        let mut summary = card().child(kv(
            if source.in_place { "Install folder" } else { "Copy from" },
            source.path.display().to_string(),
        ));
        summary = match source.measure.stats() {
            Some(stats) => summary.child(kv("Contains", size_and_files(stats))),
            None => summary.child(kv("Contains", "Measuring...")),
        };
        if let Some(expected) = install.expected_size {
            summary = summary.child(kv("Epic expects", format_bytes(expected)));
        }
        body = body.child(summary);

        for note in &check.notes {
            body = body.child(callout(ACCENT, note.clone()));
        }
        for error in &check.errors {
            body = body.child(callout(ERR, error.clone()));
        }
        if check.measuring {
            body = body.child(waiting(self.ticks, "Checking the files"));
        } else if check.errors.is_empty() && !check.warnings.is_empty() {
            for warning in &check.warnings {
                body = body.child(callout(WARN, warning.clone()));
            }
            body = body.child(row().child(
                button("accept", "Use these files anyway", Style::Warning).on_click(cx.listener(
                    |this, _: &ClickEvent, _, cx| {
                        this.warnings_accepted = true;
                        cx.notify();
                    },
                )),
            ));
        }
        body.child(row().child(choose).when(!source.in_place, |r| r.child(in_place)))
    }

    fn render_copy_step(&self, cx: &mut Context<Self>) -> Div {
        let (Some(source), Some(install)) = (&self.source, self.install()) else { return div() };
        let stats = source.measure.stats().unwrap_or_default();
        let mut body = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(heading("Copy the game files"))
            .child(
                card()
                    .child(kv("From", source.path.display().to_string()))
                    .child(kv("To", install.install_location.display().to_string()))
                    .child(kv("Size", size_and_files(stats))),
            );

        match &self.copy {
            CopyState::Idle | CopyState::Done(_) => {
                body = body
                    .child(para("Files that are already there are replaced. Your original folder isn't changed."))
                    .child(row().child(
                        button("copy", "Start copying", Style::Primary).on_click(cx.listener(
                            |this, _: &ClickEvent, _, cx| {
                                this.start_copy();
                                cx.notify();
                            },
                        )),
                    ));
            }
            CopyState::Running { progress, .. } => {
                let total = progress.total_bytes.load(Relaxed).max(1);
                let done = progress.copied_bytes.load(Relaxed);
                let current = progress.current.lock().map(|c| c.clone()).unwrap_or_default();
                let cancelling = progress.cancel.load(Relaxed);
                body = body
                    .child(progress_bar(done as f32 / total as f32))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .text_sm()
                            .text_color(rgb(TEXT))
                            .child(format!("{} of {}", format_bytes(done), format_bytes(total)))
                            .child(format!(
                                "{} / {} files",
                                progress.copied_files.load(Relaxed),
                                progress.total_files.load(Relaxed)
                            )),
                    )
                    .child(div().text_xs().text_color(rgb(MUTED)).child(current))
                    .child(if cancelling {
                        waiting(self.ticks, "Stopping")
                    } else {
                        row().child(button("cancel", "Cancel", Style::Secondary).on_click(cx.listener(
                            |this, _: &ClickEvent, _, cx| {
                                this.cancel_copy();
                                cx.notify();
                            },
                        )))
                    });
            }
            CopyState::Failed(error) => {
                let message = if error == epic::CANCELLED {
                    "Copy cancelled. Files copied so far stay where they are; starting again replaces them.".to_string()
                } else {
                    error.clone()
                };
                body = body
                    .child(callout(if error == epic::CANCELLED { WARN } else { ERR }, message))
                    .child(row().child(
                        button("retry", "Start copying again", Style::Primary).on_click(cx.listener(
                            |this, _: &ClickEvent, _, cx| {
                                this.start_copy();
                                cx.notify();
                            },
                        )),
                    ));
            }
        }
        body
    }

    fn render_finish_step(&self, cx: &mut Context<Self>) -> Div {
        let Some(install) = self.install() else { return div() };
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(heading("Ready to finish"))
            .child(para(format!(
                "The game files are in place. Finishing tells Epic that {} is fully installed, so it shows Launch instead of Resume.",
                install.display_name
            )))
            .child(
                card()
                    .child(kv("Game", install.display_name.clone()))
                    .child(kv("Install folder", install.install_location.display().to_string())),
            )
            .children(self.finish_error.clone().map(|e| callout(ERR, e)))
            .child(row().child(
                button("finish", "Finish", Style::Primary).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.finish();
                    cx.notify();
                })),
            ))
    }

    fn render_done(&self, name: &str, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .size(px(48.))
                    .rounded_full()
                    .bg(rgb(OK))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_xl()
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(BG))
                    .child("\u{2713}"),
            )
            .child(heading(format!("{name} is ready")))
            .child(para(
                "Open the Epic Games Launcher: the game should show Launch. If Epic says it needs to verify the files, let it finish, that only checks them and doesn't download the game again.",
            ))
            .children(self.launcher_error.clone().map(|e| callout(ERR, e)))
            .child(
                row()
                    .child(button("open-epic-done", "Open Epic Games Launcher", Style::Primary).on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.launcher_error = epic::open_launcher().err();
                            cx.notify();
                        }),
                    ))
                    .child(button("again", "Add another game", Style::Secondary).on_click(cx.listener(
                        |this, _: &ClickEvent, _, cx| {
                            this.start_over();
                            cx.notify();
                        },
                    ))),
            )
    }
}

impl Render for Wizard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content: AnyElement = match self.finished.clone() {
            Some(name) => self.render_done(&name, cx).into_any_element(),
            None => match self.current_step() {
                0 => self.render_download_step(cx).into_any_element(),
                1 => self.render_close_step(cx).into_any_element(),
                2 => self.render_source_step(cx).into_any_element(),
                3 => self.render_copy_step(cx).into_any_element(),
                _ => self.render_finish_step(cx).into_any_element(),
            },
        };

        div()
            .size_full()
            .flex()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .child(self.render_sidebar())
            .child(
                div()
                    .id("content")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_y_scroll()
                    .child(div().max_w(px(640.)).p_8().child(content)),
            )
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(960.), px(640.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("Epic Copy".into()), ..Default::default() }),
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
