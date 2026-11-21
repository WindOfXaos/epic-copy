// Hide the console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod epic;

use epic::{PendingInstall, Plan};
use gpui::{
    div, prelude::*, px, rgb, size, App, Application, Bounds, ClickEvent, Context, FontWeight, Hsla,
    PathPromptOptions, Rgba, ScrollHandle, SharedString, TitlebarOptions, Window, WindowBounds, WindowOptions,
};
use std::path::PathBuf;

// Palette
const BG: u32 = 0x16181d;
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

#[derive(Clone, Copy, PartialEq)]
enum LogKind {
    Info,
    Ok,
    Err,
}

struct EpicCopy {
    manifests_dir: PathBuf,
    installs: Vec<PendingInstall>,
    scan_error: Option<String>,
    selected: Option<usize>,
    plan: Option<Plan>,
    launcher_running: bool,
    confirm_armed: bool,
    log_scroll: ScrollHandle,
    log: Vec<(LogKind, SharedString)>,
}

impl EpicCopy {
    fn new() -> Self {
        let mut this = Self {
            manifests_dir: epic::manifests_dir(),
            installs: Vec::new(),
            scan_error: None,
            selected: None,
            plan: None,
            launcher_running: false,
            confirm_armed: false,
            log_scroll: ScrollHandle::new(),
            log: Vec::new(),
        };
        this.refresh();
        this
    }

    fn push_log(&mut self, kind: LogKind, line: impl Into<SharedString>) {
        self.log.push((kind, line.into()));
        self.log_scroll.scroll_to_bottom();
    }

    fn refresh(&mut self) {
        let previous = self.selected.and_then(|i| self.installs.get(i)).map(|i| i.item_path.clone());
        self.launcher_running = epic::launcher_running();
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
        let selected = previous
            .and_then(|p| self.installs.iter().position(|i| i.item_path == p))
            .or(if self.installs.len() == 1 { Some(0) } else { None });
        self.select(selected);
    }

    fn select(&mut self, index: Option<usize>) {
        self.selected = index;
        self.plan = index.and_then(|i| self.installs.get(i)).map(epic::plan);
        self.confirm_armed = false;
    }

    fn choose_folder(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Select game folder".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(folder) = paths.into_iter().next() else { return };
            this.update(cx, |this, cx| {
                this.refresh();
                match epic::find_for_folder(&this.installs, &folder) {
                    Some(i) => {
                        this.select(Some(i));
                        let name = this.installs[i].display_name.clone();
                        this.push_log(LogKind::Info, format!("{} matches pending install \"{name}\"", folder.display()));
                    }
                    None => this.push_log(
                        LogKind::Err,
                        format!(
                            "No pending install uses the folder name \"{}\". Start the download in the launcher first.",
                            folder.file_name().unwrap_or_default().to_string_lossy()
                        ),
                    ),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn complete(&mut self) {
        if !self.confirm_armed {
            self.confirm_armed = true;
            return;
        }
        self.confirm_armed = false;
        self.launcher_running = epic::launcher_running();
        if self.launcher_running {
            self.push_log(LogKind::Err, "Close the Epic Games Launcher first.");
            return;
        }
        let Some(plan) = self.plan.clone() else { return };
        let mut lines = Vec::new();
        let result = epic::apply(&plan, &mut |l| lines.push(l));
        for line in lines {
            self.push_log(LogKind::Info, line);
        }
        match result {
            Ok(()) => {
                self.push_log(LogKind::Ok, format!("{} is ready to launch.", plan.install.display_name));
                // The completed install is no longer pending, so it drops out of the list.
                self.refresh();
            }
            Err(e) => self.push_log(LogKind::Err, e),
        }
    }
}

fn hsla(color: u32) -> Hsla {
    Rgba::from(rgb(color)).into()
}

fn button(
    id: &'static str,
    label: impl Into<SharedString>,
    bg: u32,
    hover: u32,
    enabled: bool,
) -> gpui::Stateful<gpui::Div> {
    // Dark text on the amber "confirm" state, light text everywhere else.
    let fg = if bg == WARN { BG } else { TEXT };
    let el = div()
        .id(id)
        .px_4()
        .py_2()
        .rounded_md()
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .child(label.into());
    if enabled {
        el.bg(rgb(bg)).text_color(rgb(fg)).cursor_pointer().hover(move |s| s.bg(rgb(hover)))
    } else {
        el.bg(rgb(PANEL_HOVER)).text_color(rgb(MUTED))
    }
}

fn section_title(text: &'static str) -> gpui::Div {
    div().text_xs().font_weight(FontWeight::BOLD).text_color(rgb(MUTED)).child(text)
}

fn kv(key: &'static str, value: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex()
        .gap_2()
        .text_sm()
        .child(div().w(px(130.)).flex_none().text_color(rgb(MUTED)).child(key))
        .child(div().flex_1().min_w_0().text_color(rgb(TEXT)).child(value.into()))
}

fn check_row(ok: bool, text: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex()
        .gap_2()
        .text_sm()
        .items_start()
        .child(
            div()
                .flex_none()
                .mt(px(5.))
                .size(px(10.))
                .rounded_full()
                .bg(rgb(if ok { OK } else { ERR })),
        )
        .child(div().flex_1().min_w_0().text_color(rgb(TEXT)).child(text.into()))
}

impl Render for EpicCopy {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Launcher status banner
        let (banner_color, banner_text) = if self.launcher_running {
            (ERR, "Epic Games Launcher is running. Exit it (check the tray) before completing an install.")
        } else {
            (OK, "Epic Games Launcher is closed.")
        };
        let banner = div()
            .flex()
            .items_center()
            .gap_3()
            .px_4()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(rgb(banner_color))
            .bg(hsla(banner_color).opacity(0.12))
            .child(div().size(px(8.)).rounded_full().bg(rgb(banner_color)))
            .child(div().flex_1().text_sm().text_color(rgb(TEXT)).child(banner_text))
            .child(
                button("refresh", "Refresh", PANEL_HOVER, BORDER, true)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.refresh();
                        cx.notify();
                    })),
            );

        // Pending installs list
        let mut list = div().flex().flex_col().gap_2();
        if let Some(err) = &self.scan_error {
            list = list.child(div().text_sm().text_color(rgb(ERR)).child(err.clone()));
        } else if self.installs.is_empty() {
            list = list.child(
                div()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child("No pending installs. Start downloading the game in the launcher, cancel after ~10 MB, then press Refresh."),
            );
        }
        for (i, install) in self.installs.iter().enumerate() {
            let selected = self.selected == Some(i);
            list = list.child(
                div()
                    .id(("install", i))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(if selected { ACCENT } else { BORDER }))
                    .bg(rgb(if selected { PANEL_HOVER } else { PANEL }))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(PANEL_HOVER)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.select(Some(i));
                        cx.notify();
                    }))
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(rgb(TEXT)).child(install.display_name.clone()))
                    .child(div().text_xs().text_color(rgb(MUTED)).child(install.install_location.display().to_string())),
            );
        }

        let left = div()
            .flex()
            .flex_col()
            .gap_3()
            .w(px(300.))
            .flex_none()
            .min_h_0()
            .child(section_title("PENDING INSTALLS"))
            .child(div().id("installs").flex_1().min_h_0().overflow_y_scroll().child(list))
            .child(
                button("choose", "Choose game folder…", PANEL_HOVER, BORDER, true)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.choose_folder(cx))),
            );

        // Details + action
        let mut details = div().flex().flex_col().gap_3().child(section_title("DETAILS"));
        match &self.plan {
            None => {
                details = details.child(
                    div().text_sm().text_color(rgb(MUTED)).child("Select a pending install, or choose the game folder you copied the files into."),
                );
            }
            Some(plan) => {
                let install = &plan.install;
                let mut checks = div().flex().flex_col().gap_1();
                checks = checks.child(check_row(!self.launcher_running, "Epic Games Launcher is closed"));
                if plan.is_ready() {
                    checks = checks
                        .child(check_row(true, format!("Found {}", plan.bps_dir.display())))
                        .child(check_row(true, format!("{} pending install file(s) to promote", plan.pending_files.len())));
                }
                for problem in &plan.problems {
                    checks = checks.child(check_row(false, problem.clone()));
                }

                let can_apply = plan.is_ready() && !self.launcher_running;
                let label = if self.confirm_armed {
                    "Click again to confirm"
                } else {
                    "Complete install"
                };
                let (bg, hover) = if self.confirm_armed { (WARN, WARN) } else { (ACCENT, ACCENT_HOVER) };
                let mut action = button("complete", label, bg, hover, can_apply);
                if can_apply {
                    action = action.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.complete();
                        cx.notify();
                    }));
                }

                details = details
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .p_4()
                            .rounded_md()
                            .bg(rgb(PANEL))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .child(div().text_lg().font_weight(FontWeight::BOLD).text_color(rgb(TEXT)).child(install.display_name.clone()))
                            .child(kv("Folder name", install.folder_name.clone()))
                            .child(kv("Install location", install.install_location.display().to_string()))
                            .child(kv("Manifest location", install.manifest_location.display().to_string()))
                            .child(kv("Installation GUID", install.installation_guid.clone())),
                    )
                    .child(section_title("CHECKS"))
                    .child(checks)
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child("This marks the install complete, deletes .egstore\\bps and moves the pending manifest files. Back up your game data first."),
                    )
                    .child(div().flex().child(action));
            }
        }

        // Log
        let mut log = div().flex().flex_col().gap_1();
        for (kind, line) in &self.log {
            let color = match kind {
                LogKind::Info => MUTED,
                LogKind::Ok => OK,
                LogKind::Err => ERR,
            };
            log = log.child(div().text_xs().text_color(rgb(color)).child(line.clone()));
        }
        let log_panel = div()
            .flex()
            .flex_col()
            .gap_2()
            .h(px(130.))
            .flex_none()
            .child(section_title("LOG"))
            .child(
                div()
                    .id("log")
                    .flex_1()
                    .min_h_0()
                    .p_3()
                    .rounded_md()
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .overflow_y_scroll()
                    .track_scroll(&self.log_scroll)
                    .child(log),
            );

        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_4()
            .p_5()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_xl().font_weight(FontWeight::BOLD).child("Epic Copy"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(MUTED))
                            .child("Finish an Epic Games Launcher install from game files copied from another machine."),
                    ),
            )
            .child(banner)
            .child(
                div().flex().flex_1().gap_5().min_h_0().child(left).child(
                    div()
                        .id("details")
                        .flex_1()
                        .min_w_0()
                        .min_h_0()
                        .overflow_y_scroll()
                        .child(details),
                ),
            )
            .child(log_panel)
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(960.), px(760.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("Epic Copy".into()), ..Default::default() }),
                ..Default::default()
            },
            |_, cx| cx.new(|_| EpicCopy::new()),
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
