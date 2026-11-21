//! Look and feel: a dark, Epic Games Launcher-style shell (custom title bar,
//! sidebar navigation, big headings, cyan accent) around the wizard steps.

use super::{CopyState, Wizard, STEPS};
use crate::epic::{self, format_bytes, DirStats, PendingInstall};
use gpui::{
    div, hsla, img, linear_color_stop, linear_gradient, prelude::*, px, relative, rgb, AnyElement,
    ClickEvent, Context, Div, FontWeight, Hsla, Image, ImageFormat, ObjectFit, Rgba, SharedString,
    Stateful, Window, WindowControlArea,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, OnceLock};

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

const BG: u32 = 0x101014;
const SURFACE: u32 = 0x18181c;
const SURFACE_2: u32 = 0x202024;
const HOVER: u32 = 0x2a2a2f;
const BORDER: u32 = 0x2a2a30;
const TEXT: u32 = 0xffffff;
const TEXT_2: u32 = 0xc7c7cc;
const MUTED: u32 = 0x8e8e96;
const ACCENT: u32 = 0x26bbff;
const ACCENT_HOVER: u32 = 0x66d0ff;
const ON_ACCENT: u32 = 0x101014;
const OK: u32 = 0x3ccf8e;
const WARN: u32 = 0xffb547;
const ERR: u32 = 0xff6161;
const CLOSE_HOVER: u32 = 0xc42b1c;

/// Ships with Windows 10 and 11.
const ICON_FONT: &str = "Segoe MDL2 Assets";
const I_DOWNLOAD: char = '\u{E896}';
const I_POWER: char = '\u{E7E8}';
const I_FOLDER: char = '\u{E8B7}';
const I_COPY: char = '\u{E8C8}';
const I_FLAG: char = '\u{E7C1}';
const I_CHECK: char = '\u{E73E}';
const I_PLAY: char = '\u{E768}';
const I_INFO: char = '\u{E946}';
const I_WARNING: char = '\u{E7BA}';
const I_ERROR: char = '\u{E783}';
const I_OPEN: char = '\u{E8A7}';
const I_ADD: char = '\u{E710}';
const I_MIN: char = '\u{E921}';
const I_MAX: char = '\u{E922}';
const I_RESTORE: char = '\u{E923}';
const I_CLOSE: char = '\u{E8BB}';

const STEP_ICONS: [char; 5] = [I_DOWNLOAD, I_POWER, I_FOLDER, I_COPY, I_FLAG];

fn logo() -> Arc<Image> {
    static LOGO: OnceLock<Arc<Image>> = OnceLock::new();
    LOGO.get_or_init(|| {
        Arc::new(Image::from_bytes(ImageFormat::Png, include_bytes!("../assets/logo-128.png").to_vec()))
    })
    .clone()
}

fn color(c: u32) -> Hsla {
    Rgba::from(rgb(c)).into()
}

// ---------------------------------------------------------------------------
// Building blocks
// ---------------------------------------------------------------------------

fn icon(glyph: char) -> Div {
    div().font_family(ICON_FONT).child(glyph.to_string())
}

#[derive(Clone, Copy, PartialEq)]
enum Style {
    Primary,
    Secondary,
    Warning,
}

fn button(id: &'static str, glyph: Option<char>, label: impl Into<SharedString>, style: Style) -> Stateful<Div> {
    let (bg, hover, fg) = match style {
        Style::Primary => (ACCENT, ACCENT_HOVER, ON_ACCENT),
        Style::Secondary => (SURFACE_2, HOVER, TEXT),
        Style::Warning => (WARN, 0xffc978, ON_ACCENT),
    };
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .gap_2()
        .h(px(42.))
        .px_5()
        .rounded(px(8.))
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .bg(rgb(bg))
        .text_color(rgb(fg))
        .cursor_pointer()
        .hover(move |s| s.bg(rgb(hover)))
        .children(glyph.map(|g| icon(g).text_size(px(14.))))
        .child(label.into())
}

fn para(text: impl Into<SharedString>) -> Div {
    div().text_size(px(15.)).line_height(px(22.)).text_color(rgb(TEXT_2)).child(text.into())
}

fn section_label(text: &'static str) -> Div {
    div().text_size(px(11.)).font_weight(FontWeight::BOLD).text_color(rgb(TEXT)).child(text)
}

fn panel() -> Div {
    div().flex().flex_col().rounded(px(12.)).bg(rgb(SURFACE)).border_1().border_color(rgb(BORDER)).overflow_hidden()
}

fn panel_row() -> Div {
    div().px_4().py_3().border_b_1().border_color(rgb(BORDER))
}

fn kv(key: &'static str, value: impl Into<SharedString>) -> Div {
    panel_row()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_xs().text_color(rgb(MUTED)).child(key))
        .child(div().text_sm().text_color(rgb(TEXT)).child(value.into()))
}

fn callout(kind: u32, text: impl Into<SharedString>) -> Div {
    let glyph = match kind {
        ERR => I_ERROR,
        WARN => I_WARNING,
        _ => I_INFO,
    };
    div()
        .flex()
        .rounded(px(8.))
        .bg(rgb(SURFACE))
        .border_1()
        .border_color(rgb(BORDER))
        .overflow_hidden()
        .child(div().w(px(3.)).flex_none().bg(rgb(kind)))
        .child(
            div()
                .flex()
                .flex_1()
                .min_w_0()
                .gap_3()
                .px_4()
                .py_3()
                .child(icon(glyph).flex_none().mt(px(3.)).text_size(px(14.)).text_color(rgb(kind)))
                .child(div().flex_1().min_w_0().text_sm().line_height(px(20.)).text_color(rgb(TEXT)).child(text.into())),
        )
}

fn numbered(n: usize, text: impl Into<SharedString>, last: bool) -> Div {
    div()
        .flex()
        .gap_4()
        .items_center()
        .px_4()
        .py_3()
        .when(!last, |el| el.border_b_1().border_color(rgb(BORDER)))
        .child(
            div()
                .flex_none()
                .size(px(28.))
                .rounded_full()
                .bg(rgb(SURFACE_2))
                .flex()
                .items_center()
                .justify_center()
                .text_xs()
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(ACCENT))
                .child(n.to_string()),
        )
        .child(div().flex_1().min_w_0().text_sm().line_height(px(20.)).text_color(rgb(TEXT)).child(text.into()))
}

fn waiting(ticks: u64, text: &str) -> Div {
    let on = ticks / 2 % 2 == 0;
    div()
        .flex()
        .items_center()
        .gap_3()
        .text_sm()
        .text_color(rgb(TEXT_2))
        .child(
            div()
                .size(px(10.))
                .rounded_full()
                .bg(color(ACCENT).opacity(if on { 1.0 } else { 0.3 })),
        )
        .child(text.to_string())
}

fn row() -> Div {
    div().flex().flex_wrap().gap_3().items_center()
}

fn size_and_files(stats: DirStats) -> String {
    let files = if stats.files == 1 { "1 file".to_string() } else { format!("{} files", stats.files) };
    format!("{} in {files}", format_bytes(stats.bytes))
}

/// A stable pair of colours per game, so its tile always looks the same.
fn tile_colors(name: &str) -> (Hsla, Hsla) {
    let hash = name.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3));
    let hue = (hash % 360) as f32 / 360.0;
    (hsla(hue, 0.65, 0.5, 1.0), hsla((hue + 0.94) % 1.0, 0.75, 0.16, 1.0))
}

fn initials(name: &str) -> String {
    let letters: String = name
        .split(|c: char| !c.is_alphanumeric())
        .filter_map(|w| w.chars().next())
        .take(2)
        .collect();
    letters.to_uppercase()
}

/// Library-style cover: Epic's artwork when we have it, otherwise a tile
/// generated from the game's name.
fn cover(name: &str, image: Option<PathBuf>, w: f32, h: f32) -> AnyElement {
    let radius = px(if w > 80. { 8. } else { 4. });
    if let Some(path) = image {
        return img(path)
            .w(px(w))
            .h(px(h))
            .flex_none()
            .rounded(radius)
            .object_fit(ObjectFit::Cover)
            .into_any_element();
    }
    let (a, b) = tile_colors(name);
    div()
        .w(px(w))
        .h(px(h))
        .flex_none()
        .rounded(radius)
        .bg(linear_gradient(160., linear_color_stop(a, 0.), linear_color_stop(b, 1.)))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px((w * 0.3).clamp(14., 72.)))
        .font_weight(FontWeight::BLACK)
        .text_color(color(TEXT).opacity(0.92))
        .child(initials(name))
        .into_any_element()
}

/// Long paths, shortened for narrow panels: `C:\…\Games\RocketLeague`.
fn short_path(path: &Path) -> String {
    let full = path.display().to_string();
    if full.chars().count() <= 40 {
        return full;
    }
    let parts: Vec<String> = path.components().map(|c| c.as_os_str().to_string_lossy().to_string()).collect();
    if parts.len() <= 4 {
        return full;
    }
    let root = parts[0].trim_end_matches('\\');
    format!("{root}\\\u{2026}\\{}\\{}", parts[parts.len() - 2], parts[parts.len() - 1])
}

/// Sizes that follow the window width.
#[derive(Clone, Copy)]
struct Layout {
    /// Icon-only sidebar on narrow windows.
    compact: bool,
    sidebar_w: f32,
    pad: f32,
    /// Game panel above the content instead of in its own column.
    stacked: bool,
    /// Width of the full-height game column on the right.
    side_w: f32,
    height: f32,
    heading: f32,
}

/// Height of the floating title bar; columns start their content below it.
const TITLEBAR: f32 = 32.;
/// Longest comfortable line for paragraphs.
const READABLE: f32 = 880.;

impl Layout {
    fn new(width: f32, height: f32) -> Self {
        let compact = width < 1000.;
        let sidebar_w = if compact { 76. } else { 260. };
        let pad = if width < 900. { 24. } else { 40. };
        let side_w = (width * 0.26).clamp(300., 460.);
        let stacked = width - sidebar_w - side_w - 2. * pad < 520.;
        let content = width - sidebar_w - 2. * pad - if stacked { 0. } else { side_w };
        Layout {
            compact,
            sidebar_w,
            pad,
            stacked,
            side_w,
            height,
            heading: if content < 640. { 30. } else { 40. },
        }
    }
}

// ---------------------------------------------------------------------------
// Screens
// ---------------------------------------------------------------------------

struct Screen {
    title: String,
    subtitle: Option<String>,
    body: Div,
}

impl Screen {
    fn new(title: impl Into<String>, subtitle: impl Into<String>, body: Div) -> Self {
        Self { title: title.into(), subtitle: Some(subtitle.into()).filter(|s: &String| !s.is_empty()), body }
    }
}

fn body() -> Div {
    div().flex().flex_col().gap_5()
}

impl Wizard {
    fn step_index(&self) -> usize {
        if self.finished.is_some() {
            STEPS.len()
        } else {
            self.current_step()
        }
    }

    /// The status line under the game's tile, like the launcher's library.
    fn game_status(&self) -> (char, String, u32) {
        if self.finished.is_some() {
            return (I_PLAY, "Ready to launch".into(), ACCENT);
        }
        if let CopyState::Running { progress, .. } = &self.copy {
            let total = progress.total_bytes.load(Relaxed).max(1);
            let pct = progress.copied_bytes.load(Relaxed) * 100 / total;
            return (I_DOWNLOAD, format!("Copying {pct}%"), ACCENT);
        }
        if self.files_ready() {
            return (I_CHECK, "Files in place".into(), OK);
        }
        if self.source_accepted() {
            return (I_COPY, "Ready to copy".into(), TEXT_2);
        }
        match &self.plan {
            Some(plan) if plan.waiting_for_download => (I_DOWNLOAD, "Waiting for Epic".into(), TEXT_2),
            Some(_) => (I_DOWNLOAD, "Needs game files".into(), TEXT_2),
            None => (I_DOWNLOAD, String::new(), TEXT_2),
        }
    }

    fn render_download_step(&self, cx: &mut Context<Self>) -> Screen {
        if let Some(err) = &self.scan_error {
            return Screen::new("Start the download in Epic", "", body().child(callout(ERR, err.clone())));
        }

        let open_button = button("open-epic", Some(I_OPEN), "Open Epic Games Launcher", Style::Primary)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.launcher_error = epic::open_launcher().err();
                cx.notify();
            }));

        if self.plan.is_none() && self.installs.len() > 1 {
            let mut list = panel();
            let count = self.installs.len();
            for (i, install) in self.installs.iter().enumerate() {
                let path = install.item_path.clone();
                list = list.child(
                    div()
                        .id(("install", i))
                        .flex()
                        .items_center()
                        .gap_4()
                        .px_4()
                        .py_3()
                        .when(i + 1 < count, |el| el.border_b_1().border_color(rgb(BORDER)))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(SURFACE_2)))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.select(path.clone());
                            cx.notify();
                        }))
                        .child(cover(&install.display_name, self.cover_path(install), 42., 56.))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .gap_1()
                                .child(div().text_base().font_weight(FontWeight::SEMIBOLD).child(install.display_name.clone()))
                                .child(div().text_xs().text_color(rgb(MUTED)).child(install.install_location.display().to_string())),
                        ),
                );
            }
            return Screen::new(
                "Which game?",
                "Epic has several unfinished downloads. Pick the game you're adding.",
                body().child(list),
            );
        }

        match &self.plan {
            None => Screen::new(
                "Start the download in Epic",
                "Epic needs to know about the game before it can use your files. Start the download, then stop it straight away.",
                body()
                    .child(
                        panel()
                            .child(numbered(1, "Open the Epic Games Launcher and go to your Library.", false))
                            .child(numbered(2, "Click the game and press Install. Choose the drive and folder where you want it, then confirm.", false))
                            .child(numbered(3, "Let it download about 10 MB, then press the X next to the progress bar to cancel.", false))
                            .child(numbered(4, "Come back here. This page notices the download by itself.", true)),
                    )
                    .child(row().child(open_button))
                    .children(self.launcher_error.clone().map(|e| callout(ERR, e)))
                    .child(waiting(self.ticks, "Waiting for a download to start")),
            ),
            Some(plan) => {
                let mut b = body();
                if plan.waiting_for_download {
                    b = b
                        .child(row().child(open_button))
                        .children(self.launcher_error.clone().map(|e| callout(ERR, e)))
                        .child(waiting(self.ticks, "Waiting for Epic to download a little data"));
                }
                for problem in &plan.problems {
                    b = b.child(callout(ERR, problem.clone()));
                }
                if self.installs.len() > 1 {
                    b = b.child(row().child(
                        button("other-game", None, "Pick a different game", Style::Secondary).on_click(cx.listener(
                            |this, _: &ClickEvent, _, cx| {
                                this.reset_selection();
                                cx.notify();
                            },
                        )),
                    ));
                }
                Screen::new(
                    format!("Download {}", plan.install.display_name),
                    if plan.waiting_for_download {
                        "Epic hasn't downloaded anything for this game yet. In the launcher, let the download run until about 10 MB is done, then cancel it with the X."
                    } else {
                        ""
                    },
                    b,
                )
            }
        }
    }

    fn render_close_step(&self, cx: &mut Context<Self>) -> Screen {
        let closing = self.closing_launcher.is_some();
        Screen::new(
            "Close the Epic Games Launcher",
            "Epic has what it needs. Now it has to be fully closed, including the icon in the system tray, otherwise it would undo the changes when it exits.",
            body()
                .child(if closing {
                    waiting(self.ticks, "Closing Epic")
                } else {
                    row().child(
                        button("close-epic", Some(I_POWER), "Close it for me", Style::Primary).on_click(cx.listener(
                            |this, _: &ClickEvent, _, cx| {
                                this.close_launcher();
                                cx.notify();
                            },
                        )),
                    )
                })
                .children(self.launcher_error.clone().map(|e| {
                    callout(ERR, format!("Couldn't close it: {e}. Right-click the Epic icon in the system tray and choose Exit."))
                }))
                .child(div().text_sm().text_color(rgb(MUTED)).child("You can also exit it yourself. This step moves on as soon as Epic is closed.")),
        )
    }

    fn render_source_step(&self, cx: &mut Context<Self>) -> Screen {
        let Some(install) = self.install() else { return Screen::new("", "", body()) };
        let subtitle = format!(
            "Choose the {} folder you brought from the other PC, USB drive or backup. It's usually called \"{}\". Epic Copy checks it and copies it into place for you.",
            install.display_name, install.folder_name
        );
        let has_source = self.source.is_some();
        let choose = button(
            "choose",
            Some(I_FOLDER),
            if has_source { "Choose a different folder" } else { "Choose game folder" },
            if has_source { Style::Secondary } else { Style::Primary },
        )
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.choose_source(cx)));
        let in_place = button("in-place", None, "The files are already in the install folder", Style::Secondary)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                if let Some(target) = this.install().map(|i| i.install_location.clone()) {
                    this.set_source(target);
                }
                cx.notify();
            }));

        let Some(source) = &self.source else {
            return Screen::new("Where are your game files?", subtitle, body().child(row().child(choose).child(in_place)));
        };

        let check = self.check_source();
        let mut summary = panel().child(kv(
            if source.in_place { "Install folder" } else { "Copy from" },
            source.path.display().to_string(),
        ));
        summary = summary.child(kv(
            "Contains",
            source.measure.stats().map(size_and_files).unwrap_or_else(|| "Measuring...".into()),
        ));
        if let Some(expected) = install.expected_size {
            summary = summary.child(kv("Epic expects", format_bytes(expected)));
        }

        let mut b = body().child(summary);
        for note in &check.notes {
            b = b.child(callout(ACCENT, note.clone()));
        }
        for error in &check.errors {
            b = b.child(callout(ERR, error.clone()));
        }
        if check.measuring {
            b = b.child(waiting(self.ticks, "Checking the files"));
        } else if check.errors.is_empty() && !check.warnings.is_empty() {
            for warning in &check.warnings {
                b = b.child(callout(WARN, warning.clone()));
            }
            b = b.child(row().child(
                button("accept", None, "Use these files anyway", Style::Warning).on_click(cx.listener(
                    |this, _: &ClickEvent, _, cx| {
                        this.warnings_accepted = true;
                        cx.notify();
                    },
                )),
            ));
        }
        Screen::new(
            "Where are your game files?",
            subtitle,
            b.child(row().child(choose).when(!source.in_place, |r| r.child(in_place))),
        )
    }

    fn render_copy_step(&self, cx: &mut Context<Self>) -> Screen {
        let (Some(source), Some(install)) = (&self.source, self.install()) else { return Screen::new("", "", body()) };
        let stats = source.measure.stats().unwrap_or_default();
        let info = panel()
            .child(kv("From", source.path.display().to_string()))
            .child(kv("To", install.install_location.display().to_string()))
            .child(kv("Size", size_and_files(stats)));
        let mut b = body();

        match &self.copy {
            CopyState::Idle | CopyState::Done(_) => {
                b = b.child(info).child(row().child(
                    button("copy", Some(I_COPY), "Start copying", Style::Primary).on_click(cx.listener(
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
                let fraction = done as f32 / total as f32;
                let current = progress.current.lock().map(|c| c.clone()).unwrap_or_default();
                let cancelling = progress.cancel.load(Relaxed);
                b = b
                    .child(
                        panel().child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_3()
                                .p_4()
                                .child(
                                    div()
                                        .flex()
                                        .items_end()
                                        .justify_between()
                                        .child(
                                            div()
                                                .text_size(px(28.))
                                                .font_weight(FontWeight::BOLD)
                                                .child(format!("{}%", (fraction * 100.0) as u32)),
                                        )
                                        .child(
                                            div().text_sm().text_color(rgb(TEXT_2)).child(format!(
                                                "{} of {}  \u{00b7}  {} / {} files",
                                                format_bytes(done),
                                                format_bytes(total),
                                                progress.copied_files.load(Relaxed),
                                                progress.total_files.load(Relaxed)
                                            )),
                                        ),
                                )
                                .child(
                                    div()
                                        .w_full()
                                        .h(px(6.))
                                        .rounded_full()
                                        .bg(rgb(SURFACE_2))
                                        .child(div().h_full().rounded_full().bg(rgb(ACCENT)).w(relative(fraction.clamp(0.0, 1.0)))),
                                )
                                .child(div().text_xs().text_color(rgb(MUTED)).child(current)),
                        ),
                    )
                    .child(if cancelling {
                        waiting(self.ticks, "Stopping")
                    } else {
                        row().child(button("cancel", None, "Cancel", Style::Secondary).on_click(cx.listener(
                            |this, _: &ClickEvent, _, cx| {
                                this.cancel_copy();
                                cx.notify();
                            },
                        )))
                    })
                    .child(info);
            }
            CopyState::Failed(error) => {
                let cancelled = error == epic::CANCELLED;
                let message = if cancelled {
                    "Copy cancelled. Files copied so far stay where they are; starting again replaces them.".to_string()
                } else {
                    error.clone()
                };
                b = b.child(callout(if cancelled { WARN } else { ERR }, message)).child(row().child(
                    button("retry", Some(I_COPY), "Start copying again", Style::Primary).on_click(cx.listener(
                        |this, _: &ClickEvent, _, cx| {
                            this.start_copy();
                            cx.notify();
                        },
                    )),
                ));
                b = b.child(info);
            }
        }
        Screen::new(
            "Copy the game files",
            "Files already in the install folder are replaced. Your original folder isn't changed.",
            b,
        )
    }

    fn render_finish_step(&self, cx: &mut Context<Self>) -> Screen {
        let Some(install) = self.install() else { return Screen::new("", "", body()) };
        Screen::new(
            "Ready to finish",
            format!(
                "The game files are in place. Finishing tells Epic that {} is fully installed, so it shows Launch instead of Resume.",
                install.display_name
            ),
            body().children(self.finish_error.clone().map(|e| callout(ERR, e))).child(row().child(
                button("finish", Some(I_CHECK), "Finish", Style::Primary).on_click(cx.listener(
                    |this, _: &ClickEvent, _, cx| {
                        this.finish();
                        cx.notify();
                    },
                )),
            )),
        )
    }

    fn render_done(&self, name: &str, cx: &mut Context<Self>) -> Screen {
        Screen::new(
            format!("{name} is ready"),
            "Open the Epic Games Launcher and the game should show Launch. If Epic says it needs to verify the files, let it finish: that only checks them and doesn't download the game again.",
            body().children(self.launcher_error.clone().map(|e| callout(ERR, e))).child(
                row()
                    .child(
                        button("open-epic-done", Some(I_OPEN), "Open Epic Games Launcher", Style::Primary).on_click(
                            cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.launcher_error = epic::open_launcher().err();
                                cx.notify();
                            }),
                        ),
                    )
                    .child(button("again", Some(I_ADD), "Add another game", Style::Secondary).on_click(cx.listener(
                        |this, _: &ClickEvent, _, cx| {
                            this.start_over();
                            cx.notify();
                        },
                    ))),
            ),
        )
    }

    // --- chrome ------------------------------------------------------------

    fn render_titlebar(&self, window: &Window) -> Div {
        let caption = |id: &'static str, glyph: char, area: WindowControlArea, hover: u32| {
            div()
                .id(id)
                .w(px(46.))
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .font_family(ICON_FONT)
                .text_size(px(10.))
                .text_color(rgb(TEXT_2))
                .hover(move |s| s.bg(rgb(hover)).text_color(rgb(TEXT)))
                .window_control_area(area)
                .child(glyph.to_string())
        };
        // Floats over the columns so the side panel's background reaches the top.
        div()
            .absolute()
            .top_0()
            .left_0()
            .flex()
            .h(px(TITLEBAR))
            .w_full()
            .child(div().flex_1().h_full().window_control_area(WindowControlArea::Drag))
            .child(caption("min", I_MIN, WindowControlArea::Min, HOVER))
            .child(caption(
                "max",
                if window.is_maximized() { I_RESTORE } else { I_MAX },
                WindowControlArea::Max,
                HOVER,
            ))
            .child(caption("close", I_CLOSE, WindowControlArea::Close, CLOSE_HOVER))
    }


    /// Shared height for the logo row and the status pill so they line up.
    fn header_row() -> Div {
        div().flex().flex_none().items_center().h(px(44.)).mt(px(4.))
    }

    fn render_sidebar(&self, layout: Layout) -> Div {
        let current = self.step_index();
        let compact = layout.compact;
        let mut nav = div().flex().flex_col().gap_1();
        for (i, title) in STEPS.iter().enumerate() {
            let done = i < current;
            let active = i == current;
            let (glyph, glyph_color, text_color) = if done {
                (I_CHECK, OK, TEXT_2)
            } else if active {
                (STEP_ICONS[i], TEXT, TEXT)
            } else {
                (STEP_ICONS[i], MUTED, MUTED)
            };
            nav = nav.child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .h(px(48.))
                    .px_4()
                    .rounded(px(8.))
                    .when(compact, |el| el.justify_center())
                    .when(active, |el| el.bg(rgb(SURFACE_2)))
                    .child(
                        div()
                            .relative()
                            .flex_none()
                            .w(px(20.))
                            .flex()
                            .justify_center()
                            .child(icon(glyph).text_size(px(18.)).text_color(rgb(glyph_color)))
                            .when(active, |el| {
                                el.child(
                                    div().absolute().top(px(-3.)).left(px(-6.)).size(px(6.)).rounded_full().bg(rgb(ACCENT)),
                                )
                            }),
                    )
                    .when(!compact, |el| {
                        el.child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(15.))
                                .font_weight(if active { FontWeight::SEMIBOLD } else { FontWeight::NORMAL })
                                .text_color(rgb(text_color))
                                .child(*title),
                        )
                    }),
            );
        }

        let game = self.install().map(|install| {
            let (glyph, status, status_color) = self.game_status();
            let tile = cover(&install.display_name, self.cover_path(install), 40., 53.);
            if compact {
                return div().flex().justify_center().child(tile);
            }
            div()
                .flex()
                .flex_col()
                .gap_3()
                .child(div().px_4().child(section_label("YOUR GAME")))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .px_4()
                        .child(tile)
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .gap_1()
                                .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(install.display_name.clone()))
                                .when(!status.is_empty(), |el| {
                                    el.child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .text_xs()
                                            .text_color(rgb(status_color))
                                            .child(icon(glyph).text_size(px(10.)))
                                            .child(status),
                                    )
                                }),
                        ),
                )
        });

        let footer = if self.finished.is_some() {
            if compact { "\u{2713}".to_string() } else { "All done".to_string() }
        } else if compact {
            format!("{}/{}", current + 1, STEPS.len())
        } else {
            format!("Step {} of {}", current + 1, STEPS.len())
        };

        div()
            .flex()
            .flex_col()
            .w(px(layout.sidebar_w))
            .flex_none()
            .h_full()
            .pt(px(TITLEBAR))
            .px_3()
            .pb_5()
            .child(
                Self::header_row()
                    .px(px(if compact { 0. } else { 16. }))
                    .when(compact, |el| el.justify_center())
                    .gap_3()
                    .child(img(logo()).size(px(40.)).flex_none())
                    .when(!compact, |el| {
                        el.child(
                            div()
                                .flex()
                                .flex_col()
                                .child(div().text_lg().font_weight(FontWeight::BLACK).line_height(px(20.)).child("Epic Copy"))
                                .child(div().text_xs().text_color(rgb(MUTED)).child("Game transfer helper")),
                        )
                    }),
            )
            .child(div().h(px(36.)))
            .child(nav)
            .child(div().h(px(32.)))
            .children(game)
            .child(div().flex_1())
            .child(
                div()
                    .w_full()
                    .flex()
                    .when(compact, |el| el.justify_center())
                    .when(!compact, |el| el.px_4())
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(footer),
            )
    }

    fn game_details(&self, install: &PendingInstall) -> Div {
        let mut details = panel().w_full().child(kv("Install folder", short_path(&install.install_location)));
        if let Some(size) = install.expected_size {
            details = details.child(kv("Game size", format_bytes(size)));
        }
        if let Some(source) = self.source.as_ref().filter(|s| !s.in_place) {
            details = details.child(kv("Copying from", short_path(&source.path)));
        }
        if let CopyState::Done(bytes) = &self.copy {
            details = details.child(kv("Copied", format_bytes(*bytes)));
        }
        details
    }

    fn game_title(&self, install: &PendingInstall, size: f32) -> Div {
        let (glyph, status, status_color) = self.game_status();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_size(px(size)).font_weight(FontWeight::BOLD).child(install.display_name.clone()))
            .when(!status.is_empty(), |el| {
                el.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_sm()
                        .text_color(rgb(status_color))
                        .child(icon(glyph).text_size(px(12.)))
                        .child(status),
                )
            })
    }

    /// The game's tile and where it's going, like a library card. Beside the
    /// content on wide windows, a compact card above it on narrow ones.
    fn render_game_panel(&self, install: &PendingInstall, layout: Layout) -> Stateful<Div> {
        let image = self.cover_path(install);
        if layout.stacked {
            let (glyph, status, status_color) = self.game_status();
            let mut facts = vec![short_path(&install.install_location)];
            if let Some(size) = install.expected_size {
                facts.push(format_bytes(size));
            }
            return div()
                .id("game-card")
                .flex()
                .items_center()
                .gap_4()
                .p_3()
                .rounded(px(12.))
                .bg(rgb(SURFACE))
                .border_1()
                .border_color(rgb(BORDER))
                .child(cover(&install.display_name, image, 60., 80.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(div().text_base().font_weight(FontWeight::BOLD).child(install.display_name.clone()))
                        .child(div().text_xs().text_color(rgb(MUTED)).child(facts.join("  \u{00b7}  ")))
                        .when(!status.is_empty(), |el| {
                            el.child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .text_sm()
                                    .text_color(rgb(status_color))
                                    .child(icon(glyph).text_size(px(12.)))
                                    .child(status),
                            )
                        }),
                );
        }
        // A full-height column on the right, like the launcher's side panels.
        // The cover lines up with the status pill at the top.
        const SIDE_PAD: f32 = 24.;
        let w = layout.side_w - 2. * SIDE_PAD;
        let h = (w * 4. / 3.).min(layout.height * 0.55).max(w * 0.75);
        div()
            .id("game-panel")
            .flex()
            .flex_col()
            .gap_4()
            .w(px(layout.side_w))
            .flex_none()
            .h_full()
            .overflow_y_scroll()
            .px(px(SIDE_PAD))
            .pt(px(TITLEBAR + 6.))
            .pb(px(SIDE_PAD))
            .bg(rgb(SURFACE))
            .border_l_1()
            .border_color(rgb(BORDER))
            // Portrait, like the launcher's library covers.
            .child(cover(&install.display_name, image, w, h))
            .child(self.game_title(install, 20.))
            .child(self.game_details(install))
    }
}

impl Render for Wizard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        let layout = Layout::new(f32::from(viewport.width), f32::from(viewport.height));
        let screen = match self.finished.clone() {
            Some(name) => self.render_done(&name, cx),
            None => match self.current_step() {
                0 => self.render_download_step(cx),
                1 => self.render_close_step(cx),
                2 => self.render_source_step(cx),
                3 => self.render_copy_step(cx),
                _ => self.render_finish_step(cx),
            },
        };
        let (pill_icon, pill_text) = if self.finished.is_some() {
            (I_CHECK, "Done".to_string())
        } else {
            let step = self.current_step();
            (STEP_ICONS[step.min(4)], format!("Step {} of {}  \u{00b7}  {}", step + 1, STEPS.len(), STEPS[step.min(4)]))
        };
        let game_panel = self.install().cloned().map(|install| self.render_game_panel(&install, layout));
        let (panel_above, panel_side) = if layout.stacked { (game_panel, None) } else { (None, game_panel) };

        // Content starts right next to the sidebar and fills the space up to
        // the game column; only paragraphs keep a readable line length.
        let main = div()
            .id("main")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .px(px(layout.pad))
                    .pt(px(TITLEBAR))
                    .pb_10()
                    .child(
                        // Status pill where the launcher has its search box.
                        Self::header_row().child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .h(px(40.))
                                .px_5()
                                .rounded_full()
                                .bg(rgb(SURFACE_2))
                                .text_sm()
                                .text_color(rgb(TEXT_2))
                                .child(icon(pill_icon).text_size(px(13.)))
                                .child(pill_text),
                        ),
                    )
                    .child(
                        div()
                            .mt(px(36.))
                            .text_size(px(layout.heading))
                            .line_height(px(layout.heading * 1.2))
                            .font_weight(FontWeight::BLACK)
                            .child(screen.title),
                    )
                    .child(
                        div()
                            .mt_6()
                            .flex()
                            .flex_col()
                            .gap_6()
                            .children(panel_above)
                            .children(screen.subtitle.map(|s| para(s).max_w(px(READABLE))))
                            .child(screen.body),
                    ),
            );

        div()
            .relative()
            .size_full()
            .flex()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .child(self.render_sidebar(layout))
            .child(main)
            .children(panel_side)
            .child(self.render_titlebar(window))
    }
}
