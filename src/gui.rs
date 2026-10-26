use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, FontFamily, FontId, RichText, Sense, Stroke, TextEdit, Vec2};

use crate::http::{filename_from_url, start_url_download_reported};
use crate::identity::{self, AppKind, VERSION_TAG};
use crate::paths;
use crate::progress::{format_eta, format_size, format_speed, is_stopped, JobSnapshot};
use crate::torrent::{magnet_display_name, magnet_info_hash, start_download_reported};
use crate::update::{self, AvailableUpdate};
use crate::validate::{is_valid_download_url, is_valid_magnet_url};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ThemeId {
    Phosphor,
    Amber,
    Ice,
}

#[derive(Clone, Copy)]
struct Theme {
    bg: Color32,
    row: Color32,
    row_alt: Color32,
    fg: Color32,
    dim: Color32,
    fail: Color32,
}

fn mix_rgb(a: Color32, b: Color32, toward_b: f32) -> Color32 {
    let t = toward_b.clamp(0.0, 1.0);
    let blend = |x: u8, y: u8| ((x as f32) * (1.0 - t) + (y as f32) * t).round() as u8;
    Color32::from_rgb(
        blend(a.r(), b.r()),
        blend(a.g(), b.g()),
        blend(a.b(), b.b()),
    )
}

fn row_accent(palette: Theme, settled: bool) -> Color32 {
    if settled {
        palette.fg
    } else {
        mix_rgb(palette.fg, palette.dim, 0.65)
    }
}

fn theme(id: ThemeId) -> Theme {
    match id {
        ThemeId::Phosphor => Theme {
            bg: Color32::from_rgb(10, 10, 10),
            row: Color32::from_rgb(16, 16, 16),
            row_alt: Color32::from_rgb(22, 22, 22),
            fg: Color32::from_rgb(110, 255, 110),
            dim: Color32::from_rgb(62, 140, 62),
            fail: Color32::from_rgb(255, 90, 90),
        },
        ThemeId::Amber => Theme {
            bg: Color32::from_rgb(12, 9, 4),
            row: Color32::from_rgb(22, 16, 6),
            row_alt: Color32::from_rgb(30, 22, 8),
            fg: Color32::from_rgb(255, 184, 72),
            dim: Color32::from_rgb(168, 112, 32),
            fail: Color32::from_rgb(255, 96, 64),
        },
        ThemeId::Ice => Theme {
            bg: Color32::from_rgb(6, 12, 18),
            row: Color32::from_rgb(10, 20, 30),
            row_alt: Color32::from_rgb(14, 28, 40),
            fg: Color32::from_rgb(140, 220, 255),
            dim: Color32::from_rgb(70, 120, 150),
            fail: Color32::from_rgb(255, 120, 140),
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Magnet,
    Http,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Self::Magnet => "MAGNET",
            Self::Http => "HTTP",
        }
    }

    fn toggle(self) -> Self {
        match self {
            Self::Magnet => Self::Http,
            Self::Http => Self::Magnet,
        }
    }
}

const MAX_ACTIVE_DOWNLOADS: usize = 4;
const CHECKING_UPDATES: &str = "checking for updates…";
const NO_UPDATES: &str = "no updates found";
const STATUS_HOLD: Duration = Duration::from_millis(1500);

enum WorkerCmd {
    Download {
        id: u64,
        mode: Mode,
        source: String,
        snapshot: Arc<JobSnapshot>,
        seed: bool,
        dest: PathBuf,
    },
    Check,
    Install(AvailableUpdate),
}

enum WorkerEvent {
    UpdateAvailable(AvailableUpdate),
    UpToDate,
    UpdateFailed(String),
    Installed(String),
    DownloadFailed { id: u64, error: String },
}

struct Job {
    id: u64,
    kind: Mode,
    source: String,
    snapshot: Arc<JobSnapshot>,
    added: String,
    last_bytes: u64,
    last_uploaded: u64,
    last_check: Instant,
    speed: f64,
    up_speed: f64,
    seed: bool,
    dest: PathBuf,
}

struct Toast {
    id: u64,
    text: String,
    fail: bool,
    expires_at: Option<Instant>,
}

struct RestartDraft {
    id: u64,
    kind: Mode,
    seed: bool,
    dest: PathBuf,
}

struct GuiApp {
    theme: ThemeId,
    applied_theme: ThemeId,
    mode: Mode,
    input: String,
    dest: PathBuf,
    jobs: Vec<Job>,
    next_id: u64,
    tx: Sender<WorkerCmd>,
    events: Receiver<WorkerEvent>,
    auto_update: bool,
    seed: bool,
    toasts: Vec<Toast>,
    next_toast: u64,
    restart: Option<RestartDraft>,
    pending: Option<AvailableUpdate>,
    banner_dismissed: bool,
    installed_tag: Option<String>,
}

pub fn run() -> eframe::Result<()> {
    update::cleanup_replaced_binary();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(identity::banner())
            .with_inner_size([960.0, 580.0])
            .with_min_inner_size([760.0, 440.0])
            .with_icon(crate::icon::app_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "torrenter",
        options,
        Box::new(|cc| Ok(Box::new(GuiApp::new(cc)))),
    )
}

impl GuiApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        thread::spawn(move || worker_loop(cmd_rx, event_tx));
        let config = update::load_config();
        let auto_update = config.auto_update;
        let seed = config.seed;
        let dest = paths::effective_dir(&config);
        let launch_error = paths::ensure_dir(&dest)
            .err()
            .map(|error| format!("{error:#}"));
        let _ = cmd_tx.send(WorkerCmd::Check);
        let mut app = Self {
            theme: ThemeId::Phosphor,
            applied_theme: ThemeId::Phosphor,
            mode: Mode::Magnet,
            input: String::new(),
            dest,
            jobs: Vec::new(),
            next_id: 1,
            tx: cmd_tx,
            events: event_rx,
            auto_update,
            seed,
            toasts: Vec::new(),
            next_toast: 1,
            restart: None,
            pending: None,
            banner_dismissed: false,
            installed_tag: None,
        };
        if let Some(error) = launch_error {
            app.toast_fail(error);
        }
        app.apply_theme(&cc.egui_ctx);
        app
    }

    fn apply_theme(&self, ctx: &egui::Context) {
        let palette = theme(self.theme);
        let mut visuals = egui::Visuals::dark();
        visuals.override_text_color = Some(palette.fg);
        visuals.panel_fill = palette.bg;
        visuals.window_fill = palette.bg;
        visuals.extreme_bg_color = palette.row;
        visuals.faint_bg_color = palette.row_alt;
        visuals.widgets.inactive.bg_fill = palette.row;
        visuals.widgets.hovered.bg_fill = palette.row_alt;
        visuals.widgets.active.bg_fill = palette.row_alt;
        visuals.widgets.open.bg_fill = palette.row_alt;
        visuals.selection.bg_fill =
            Color32::from_rgba_unmultiplied(palette.fg.r(), palette.fg.g(), palette.fg.b(), 40);
        visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, palette.fg);
        visuals.interact_cursor = Some(egui::CursorIcon::PointingHand);
        ctx.set_visuals(visuals);
        ctx.style_mut(|style| {
            style.spacing.item_spacing = Vec2::new(10.0, 8.0);
        });
    }

    fn queue_job(&mut self) {
        let source = self.input.trim().to_string();
        if source.is_empty() {
            self.toast_fail("paste a magnet or HTTP URL first");
            return;
        }
        let valid = match self.mode {
            Mode::Magnet => is_valid_magnet_url(&source),
            Mode::Http => is_valid_download_url(&source),
        };
        if !valid {
            self.toast_fail(format!("invalid {} URL", self.mode.label().to_lowercase()));
            return;
        }
        if let Some(note) = self.duplicate_active_note(self.mode, &source) {
            self.toast_fail(note);
            return;
        }
        let snapshot = Arc::new(JobSnapshot::default());
        snapshot.set_status("queued");
        match self.mode {
            Mode::Magnet => {
                if let Some(name) = magnet_display_name(&source) {
                    snapshot.set_title(name);
                }
            }
            Mode::Http => snapshot.set_title(filename_from_url(&source, None)),
        }
        let id = self.next_id;
        self.next_id += 1;
        let seed = self.mode == Mode::Magnet && self.seed;
        let dest = self.dest.clone();
        let job = Job {
            id,
            kind: self.mode,
            source: source.clone(),
            snapshot: Arc::clone(&snapshot),
            added: today(),
            last_bytes: 0,
            last_uploaded: 0,
            last_check: Instant::now(),
            speed: 0.0,
            up_speed: 0.0,
            seed,
            dest: dest.clone(),
        };
        let _ = self.tx.send(WorkerCmd::Download {
            id,
            mode: self.mode,
            source,
            snapshot,
            seed,
            dest,
        });
        self.jobs.push(job);
        self.input.clear();
    }

    fn restart_job(&mut self, index: usize, seed: bool, dest: PathBuf) -> bool {
        let Some(job) = self.jobs.get(index) else {
            return false;
        };
        let kind = job.kind;
        let source = job.source.clone();
        if let Some(note) = self.duplicate_active_note(kind, &source) {
            self.toast_fail(note);
            return false;
        }
        let title = job.snapshot.title();
        let snapshot = Arc::new(JobSnapshot::default());
        snapshot.set_status("queued");
        if !title.is_empty() {
            snapshot.set_title(title);
        }
        let id = self.next_id;
        self.next_id += 1;
        let seed = kind == Mode::Magnet && seed;
        let _ = self.tx.send(WorkerCmd::Download {
            id,
            mode: kind,
            source: source.clone(),
            snapshot: Arc::clone(&snapshot),
            seed,
            dest: dest.clone(),
        });
        self.jobs[index] = Job {
            id,
            kind,
            source,
            snapshot,
            added: today(),
            last_bytes: 0,
            last_uploaded: 0,
            last_check: Instant::now(),
            speed: 0.0,
            up_speed: 0.0,
            seed,
            dest,
        };
        true
    }

    fn duplicate_active_note(&self, kind: Mode, source: &str) -> Option<String> {
        let conflict = self.jobs.iter().any(|job| {
            if !job.snapshot.is_active() || job.kind != kind {
                return false;
            }
            match kind {
                Mode::Magnet => magnet_info_hash(source).is_some_and(|hash| {
                    magnet_info_hash(&job.source).as_deref() == Some(hash.as_str())
                }),
                Mode::Http => job.source == source,
            }
        });
        conflict.then(|| match kind {
            Mode::Magnet => "that magnet is already downloading".into(),
            Mode::Http => "that URL is already downloading".into(),
        })
    }

    fn toast_info(&mut self, text: impl Into<String>) {
        self.push_toast(text.into(), false, Some(STATUS_HOLD));
    }

    fn toast_hold(&mut self, text: impl Into<String>) {
        self.push_toast(text.into(), false, None);
    }

    fn toast_fail(&mut self, text: impl Into<String>) {
        self.push_toast(text.into(), true, None);
    }

    fn push_toast(&mut self, text: String, fail: bool, hold: Option<Duration>) {
        if text.is_empty() {
            return;
        }
        let id = self.next_toast;
        self.next_toast += 1;
        self.toasts.push(Toast {
            id,
            text,
            fail,
            expires_at: hold.map(|duration| Instant::now() + duration),
        });
    }

    fn expire_toasts(&mut self) {
        let now = Instant::now();
        self.toasts
            .retain(|toast| toast.expires_at.is_none_or(|deadline| now < deadline));
    }

    fn replace_checking_toast(&mut self) {
        if let Some(toast) = self
            .toasts
            .iter_mut()
            .find(|toast| toast.text == CHECKING_UPDATES)
        {
            toast.text = note_after_up_to_date(CHECKING_UPDATES)
                .unwrap_or(NO_UPDATES)
                .into();
            toast.fail = false;
            toast.expires_at = Some(Instant::now() + STATUS_HOLD);
        } else {
            self.toast_info(NO_UPDATES);
        }
    }

    fn apply_picked_dir(&mut self, path: PathBuf, job_local: bool) {
        let result = if job_local {
            paths::persistable_dir(&path).inspect(|stored| {
                if let Some(draft) = self.restart.as_mut() {
                    draft.dest = stored.clone();
                }
            })
        } else {
            update::set_download_dir(&path).map(|config| {
                let dest = paths::effective_dir(&config);
                self.dest = dest.clone();
                dest
            })
        };
        if let Err(error) = result {
            self.toast_fail(format!("{error:#}"));
        }
    }

    fn pick_folder(start: &Path) -> Option<PathBuf> {
        let mut dialog = rfd::FileDialog::new().set_title("download folder");
        if start.exists() {
            dialog = dialog.set_directory(start);
        } else if let Some(parent) = start.parent().filter(|path| path.exists()) {
            dialog = dialog.set_directory(parent);
        }
        dialog.pick_folder()
    }

    fn reveal_path(&mut self, kind: Mode, source: &str, dest: &Path) {
        let mut target = dest.to_path_buf();
        if kind == Mode::Magnet {
            if let Some(hash) = magnet_info_hash(source) {
                let nested = dest.join(hash);
                if nested.is_dir() {
                    target = nested;
                }
            }
        }
        if let Err(error) = open::that(&target) {
            self.toast_fail(format!("could not open folder: {error}"));
        }
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                WorkerEvent::UpdateAvailable(update) => {
                    self.banner_dismissed = false;
                    self.toasts.retain(|toast| toast.text != CHECKING_UPDATES);
                    if self.auto_update {
                        let _ = self.tx.send(WorkerCmd::Install(update.clone()));
                        self.toast_hold(format!("installing {}…", update.tag));
                    }
                    self.pending = Some(update);
                }
                WorkerEvent::UpToDate => {
                    self.pending = None;
                    if self
                        .toasts
                        .iter()
                        .any(|toast| toast.text == CHECKING_UPDATES)
                    {
                        self.replace_checking_toast();
                    }
                }
                WorkerEvent::UpdateFailed(error) => {
                    if self.auto_update {
                        self.toast_fail(error);
                    }
                }
                WorkerEvent::Installed(tag) => {
                    self.pending = None;
                    self.toasts
                        .retain(|toast| !toast.text.starts_with("installing "));
                    self.installed_tag = Some(tag);
                }
                WorkerEvent::DownloadFailed { id, error } => {
                    if let Some(job) = self.jobs.iter().find(|job| job.id == id) {
                        if !job.snapshot.is_stopped() {
                            job.snapshot.set_error(error);
                        }
                    }
                }
            }
        }
    }
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        self.expire_toasts();
        if self.applied_theme != self.theme {
            self.apply_theme(ctx);
            self.applied_theme = self.theme;
        }
        let palette = theme(self.theme);
        ctx.request_repaint_after(Duration::from_millis(200));

        const CONTROLS_H: f32 = 28.0;
        const URL_H: f32 = 28.0;
        const DEST_H: f32 = 28.0;
        let header_h = 16.0 + CONTROLS_H + URL_H + 10.0 + DEST_H + 16.0;
        egui::TopBottomPanel::top("header")
            .exact_height(header_h)
            .resizable(false)
            .frame(egui::Frame::NONE.fill(palette.bg).inner_margin(16.0))
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let width = ui.available_width();
                let (controls, _) =
                    ui.allocate_exact_size(egui::vec2(width, CONTROLS_H), egui::Sense::hover());
                ui.allocate_new_ui(
                    egui::UiBuilder::new()
                        .max_rect(controls)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    |ui| {
                        ui.allocate_ui_with_layout(
                            egui::vec2(380.0, CONTROLS_H),
                            egui::Layout::left_to_right(egui::Align::Max),
                            |ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;
                                ui.label(mono(identity::REPO, 18.0, palette.fg));
                                ui.label(mono(VERSION_TAG, 10.0, palette.dim));
                            },
                        );
                        let right = egui::Rect::from_min_size(
                            egui::pos2(controls.right() - 310.0, controls.top()),
                            egui::vec2(310.0, CONTROLS_H),
                        );
                        ui.allocate_new_ui(
                            egui::UiBuilder::new()
                                .max_rect(right)
                                .layout(egui::Layout::top_down(egui::Align::RIGHT)),
                            |ui| {
                                ui.horizontal(|ui| {
                                    if refresh_toggle(ui, self.auto_update, palette).clicked() {
                                        self.auto_update = !self.auto_update;
                                        let _ = update::set_auto_update(self.auto_update);
                                        if self.auto_update {
                                            let _ = self.tx.send(WorkerCmd::Check);
                                            self.toast_hold(CHECKING_UPDATES);
                                        }
                                    }
                                    ui.add_space(10.0);
                                    ui.scope(|ui| {
                                        ui.style_mut().interaction.interact_radius = 0.0;
                                        for (id, name) in [
                                            (ThemeId::Phosphor, "phosphor"),
                                            (ThemeId::Amber, "amber"),
                                            (ThemeId::Ice, "ice"),
                                        ] {
                                            if theme_swatch(
                                                ui,
                                                theme(id).fg,
                                                self.theme == id,
                                                name,
                                            )
                                            .clicked()
                                            {
                                                self.theme = id;
                                            }
                                        }
                                    });
                                    ui.add_space(6.0);
                                    ui.label(mono("theme", 12.0, palette.dim));
                                });
                            },
                        );
                    },
                );
                let (url_row, _) =
                    ui.allocate_exact_size(egui::vec2(width, URL_H), egui::Sense::hover());
                ui.allocate_new_ui(
                    egui::UiBuilder::new()
                        .max_rect(url_row)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    |ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        if mode_switch(ui, self.mode, palette).clicked() {
                            self.mode = self.mode.toggle();
                        }
                        let button_w = 80.0_f32;
                        let gap = 10.0_f32;
                        let glyphs_w = 42.0_f32;
                        let edit_margin = 16.0_f32;
                        ui.add_space(gap);
                        let field_w = (ui.available_width() - gap - glyphs_w - gap - button_w)
                            .clamp(200.0, 680.0);
                        let edit_w = (field_w - edit_margin).max(160.0);
                        let input_id = ui.make_persistent_id("download-url");
                        let focused = ui.memory(|memory| memory.has_focus(input_id));
                        let mut edit = TextEdit::singleline(&mut self.input)
                            .id(input_id)
                            .font(FontId::new(14.0, FontFamily::Monospace))
                            .desired_width(edit_w)
                            .margin(egui::Margin::symmetric(8, 5));
                        if !focused {
                            edit = edit.hint_text(match self.mode {
                                Mode::Magnet => "magnet:?xt=urn:btih:…",
                                Mode::Http => "https://example.com/file.iso",
                            });
                        }
                        let response = ui.allocate_ui_with_layout(
                            egui::vec2(field_w, URL_H),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                let dim = Stroke::new(1.0_f32, palette.dim);
                                let lit = Stroke::new(1.0_f32, palette.fg);
                                ui.visuals_mut().widgets.inactive.bg_stroke = dim;
                                ui.visuals_mut().widgets.hovered.bg_stroke = lit;
                                ui.visuals_mut().widgets.active.bg_stroke = lit;
                                ui.visuals_mut().widgets.open.bg_stroke = lit;
                                ui.visuals_mut().widgets.hovered.expansion = 0.0;
                                ui.visuals_mut().widgets.active.expansion = 0.0;
                                ui.visuals_mut().widgets.open.expansion = 0.0;
                                ui.add(edit)
                            },
                        );
                        ui.add_space(gap);
                        if let Some(seed) =
                            seed_toggle(ui, self.seed, palette, self.mode == Mode::Magnet)
                        {
                            self.seed = seed;
                            let _ = update::set_seed(seed);
                        }
                        ui.add_space(gap);
                        let start = ui.add_sized(
                            egui::vec2(button_w, URL_H),
                            egui::Button::new(mono("START", 14.0, palette.fg)),
                        );
                        if start.clicked()
                            || (response.inner.lost_focus()
                                && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                        {
                            self.queue_job();
                        }
                    },
                );
                ui.add_space(10.0);
                let (dest_row, _) =
                    ui.allocate_exact_size(egui::vec2(width, DEST_H), egui::Sense::hover());
                ui.allocate_new_ui(
                    egui::UiBuilder::new()
                        .max_rect(dest_row)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    |ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        let folder = dest_glyph(ui, palette, true);
                        let shown = truncate_middle(&self.dest.display().to_string(), 72);
                        let path = click_cursor(ui.add(
                            egui::Label::new(mono(shown, 12.0, palette.dim)).sense(Sense::click()),
                        ))
                        .on_hover_text(self.dest.display().to_string());
                        if folder.clicked() || path.clicked() {
                            if let Some(picked) = Self::pick_folder(&self.dest) {
                                self.apply_picked_dir(picked, false);
                            }
                        }
                    },
                );
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(palette.bg).inner_margin(12.0))
            .show(ctx, |ui| {
                if let Some(tag) = self.installed_tag.clone() {
                    let fill = Color32::from_rgba_unmultiplied(
                        palette.fg.r(),
                        palette.fg.g(),
                        palette.fg.b(),
                        18,
                    );
                    egui::Frame::NONE
                        .fill(fill)
                        .stroke(Stroke::new(1.0_f32, palette.dim))
                        .inner_margin(egui::Margin::symmetric(10, 6))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(mono(
                                    format!("{tag} is on disk  ·  restart to run it"),
                                    13.0,
                                    palette.fg,
                                ));
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.button(mono("later", 12.0, palette.dim)).clicked() {
                                            self.installed_tag = None;
                                        }
                                        if ui
                                            .button(mono("restart now", 12.0, palette.fg))
                                            .clicked()
                                        {
                                            let _ = update::relaunch();
                                        }
                                    },
                                );
                            });
                        });
                    ui.add_space(8.0);
                } else if let Some(available) = self.pending.clone() {
                    if !self.auto_update && !self.banner_dismissed {
                        let fill = Color32::from_rgba_unmultiplied(
                            palette.fg.r(),
                            palette.fg.g(),
                            palette.fg.b(),
                            18,
                        );
                        egui::Frame::NONE
                            .fill(fill)
                            .stroke(Stroke::new(1.0_f32, palette.dim))
                            .inner_margin(egui::Margin::symmetric(10, 6))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    let (icon_rect, _) = ui.allocate_exact_size(
                                        egui::vec2(16.0, 16.0),
                                        egui::Sense::hover(),
                                    );
                                    paint_refresh(
                                        ui.painter(),
                                        icon_rect.center(),
                                        5.5,
                                        palette.fg,
                                    );
                                    ui.label(mono(
                                        format!("a new version is available  ·  {}", available.tag),
                                        13.0,
                                        palette.fg,
                                    ));
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui
                                                .button(mono("dismiss", 12.0, palette.dim))
                                                .clicked()
                                            {
                                                self.banner_dismissed = true;
                                            }
                                            if ui
                                                .button(mono("install", 12.0, palette.fg))
                                                .clicked()
                                            {
                                                let _ = self
                                                    .tx
                                                    .send(WorkerCmd::Install(available.clone()));
                                                self.toast_hold("installing update…");
                                            }
                                        },
                                    );
                                });
                            });
                        ui.add_space(8.0);
                    }
                }
                if self.jobs.is_empty() {
                    ui.centered_and_justified(|ui| {
                        ui.label(mono(
                            "no jobs yet — paste a magnet or HTTP URL",
                            14.0,
                            palette.dim,
                        ));
                    });
                    return;
                }
                let mut clear_index = None;
                let mut open_restart = None;
                let mut reveal = None;
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (index, job) in self.jobs.iter_mut().enumerate() {
                        let now = Instant::now();
                        let downloaded = job
                            .snapshot
                            .downloaded
                            .load(std::sync::atomic::Ordering::Relaxed);
                        let uploaded = job
                            .snapshot
                            .uploaded
                            .load(std::sync::atomic::Ordering::Relaxed);
                        let engine_up = job
                            .snapshot
                            .upload_bps
                            .load(std::sync::atomic::Ordering::Relaxed)
                            as f64;
                        let elapsed = now.saturating_duration_since(job.last_check).as_secs_f64();
                        if elapsed >= 0.4 {
                            job.speed =
                                (downloaded.saturating_sub(job.last_bytes) as f64) / elapsed;
                            job.up_speed = if engine_up > 0.0 {
                                engine_up
                            } else {
                                (uploaded.saturating_sub(job.last_uploaded) as f64) / elapsed
                            };
                            job.last_bytes = downloaded;
                            job.last_uploaded = uploaded;
                            job.last_check = now;
                        } else if engine_up > 0.0 {
                            job.up_speed = engine_up;
                        }
                        let total = job
                            .snapshot
                            .total
                            .load(std::sync::atomic::Ordering::Relaxed);
                        let peers = job
                            .snapshot
                            .peers
                            .load(std::sync::atomic::Ordering::Relaxed);
                        let status = job
                            .snapshot
                            .error()
                            .unwrap_or_else(|| job.snapshot.status());
                        let fill = if index % 2 == 0 {
                            palette.row
                        } else {
                            palette.row_alt
                        };
                        let settled = job.snapshot.is_done()
                            || (total > 0
                                && downloaded >= total
                                && job.snapshot.error().is_none()
                                && !job.snapshot.is_stopped());
                        let accent = row_accent(palette, settled);
                        egui::Frame::NONE
                            .fill(fill)
                            .inner_margin(10.0)
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(mono(job.kind.label(), 13.0, palette.dim));
                                    if job.kind == Mode::Magnet {
                                        ui.add_space(6.0);
                                        seed_glyph(ui, job.seed, true, false, false, palette);
                                    }
                                    ui.add_space(8.0);
                                    let title = job.snapshot.title();
                                    let shown = if title.is_empty() {
                                        truncate(&job.source, 64)
                                    } else {
                                        truncate(&title, 64)
                                    };
                                    if click_cursor(
                                        ui.add(
                                            egui::Label::new(mono(shown, 13.0, accent))
                                                .sense(Sense::click()),
                                        ),
                                    )
                                    .on_hover_text(&job.source)
                                    .clicked()
                                    {
                                        reveal = Some(index);
                                    }
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            ui.label(mono(&job.added, 13.0, palette.dim));
                                            if job.snapshot.is_active() {
                                                if ui
                                                    .button(mono("stop", 12.0, palette.fail))
                                                    .clicked()
                                                {
                                                    job.snapshot.request_stop();
                                                }
                                                let pause_label = if job.snapshot.is_paused() {
                                                    "resume"
                                                } else {
                                                    "pause"
                                                };
                                                if ui
                                                    .button(mono(pause_label, 12.0, accent))
                                                    .clicked()
                                                {
                                                    job.snapshot.toggle_pause();
                                                }
                                            } else {
                                                if ui
                                                    .button(mono("clear", 12.0, palette.dim))
                                                    .clicked()
                                                {
                                                    clear_index = Some(index);
                                                }
                                                if (job.kind == Mode::Magnet
                                                    || job.snapshot.is_stopped())
                                                    && ui
                                                        .button(mono("restart", 12.0, palette.fg))
                                                        .clicked()
                                                {
                                                    open_restart = Some(index);
                                                }
                                            }
                                        },
                                    );
                                });
                                let fraction = if total > 0 {
                                    downloaded as f32 / total as f32
                                } else {
                                    0.0
                                };
                                ui.scope(|ui| {
                                    ui.visuals_mut().selection.bg_fill = accent;
                                    ui.add(
                                        egui::ProgressBar::new(fraction.clamp(0.0, 1.0))
                                            .desired_height(8.0)
                                            .fill(accent),
                                    );
                                });
                                let eta = if !job.snapshot.is_paused()
                                    && job.speed > 1.0
                                    && total > downloaded
                                {
                                    format_eta(Some(Duration::from_secs_f64(
                                        (total - downloaded) as f64 / job.speed,
                                    )))
                                } else {
                                    "N/A".into()
                                };
                                let sharing = job.kind == Mode::Magnet && job.seed;
                                let seeding = sharing
                                    && job.snapshot.is_active()
                                    && total > 0
                                    && downloaded >= total;
                                let line = if sharing {
                                    if seeding {
                                        format!(
                                            "{status}  peers {peers}  D {}  U {} @ {}  {}",
                                            format_size(downloaded),
                                            format_size(uploaded),
                                            format_speed(job.up_speed),
                                            format_size(total),
                                        )
                                    } else {
                                        format!(
                                            "{status}  peers {peers}  D {} @ {}  U {} @ {}  {}  eta {eta}",
                                            format_size(downloaded),
                                            format_speed(job.speed),
                                            format_size(uploaded),
                                            format_speed(job.up_speed),
                                            format_size(total),
                                        )
                                    }
                                } else {
                                    format!(
                                        "{status}  peers {peers}  D {} @ {}  {}  eta {eta}",
                                        format_size(downloaded),
                                        format_speed(job.speed),
                                        format_size(total),
                                    )
                                };
                                let color = if job.snapshot.error().is_some() {
                                    palette.fail
                                } else {
                                    palette.dim
                                };
                                ui.label(mono(line, 12.0, color));
                            });
                    }
                });
                if let Some(index) = reveal {
                    let reveal_job = self.jobs.get(index).map(|job| {
                        (job.kind, job.source.clone(), job.dest.clone())
                    });
                    if let Some((kind, source, dest)) = reveal_job {
                        self.reveal_path(kind, &source, &dest);
                    }
                }
                if let Some(index) = open_restart {
                    if let Some(job) = self.jobs.get(index) {
                        self.restart = Some(RestartDraft {
                            id: job.id,
                            kind: job.kind,
                            seed: job.seed,
                            dest: job.dest.clone(),
                        });
                    }
                } else if let Some(index) = clear_index {
                    if self.restart.as_ref().is_some_and(|draft| {
                        self.jobs.get(index).is_some_and(|job| job.id == draft.id)
                    }) {
                        self.restart = None;
                    }
                    self.jobs.remove(index);
                }
            });

        self.show_restart_popup(ctx, palette);
        self.show_toasts(ctx, palette);
    }
}

impl GuiApp {
    fn show_restart_popup(&mut self, ctx: &egui::Context, palette: Theme) {
        let Some(draft) = self.restart.as_ref() else {
            return;
        };
        if !self.jobs.iter().any(|job| job.id == draft.id) {
            self.restart = None;
            return;
        }
        let mut open = true;
        let mut picked = None;
        let mut start = false;
        let mut seed = draft.seed;
        let dest = draft.dest.clone();
        let kind = draft.kind;
        egui::Window::new("restart")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.set_min_width(420.0);
                if kind == Mode::Magnet {
                    ui.horizontal(|ui| {
                        ui.label(mono("mode", 12.0, palette.dim));
                        if let Some(value) = seed_toggle(ui, seed, palette, true) {
                            seed = value;
                        }
                    });
                    ui.add_space(8.0);
                }
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let folder = dest_glyph(ui, palette, true);
                    let shown = truncate_middle(&dest.display().to_string(), 48);
                    let path = click_cursor(ui.add(
                        egui::Label::new(mono(shown, 12.0, palette.dim)).sense(Sense::click()),
                    ))
                    .on_hover_text(dest.display().to_string());
                    if folder.clicked() || path.clicked() {
                        picked = Self::pick_folder(&dest);
                    }
                });
                ui.add_space(12.0);
                if ui
                    .add_sized(
                        egui::vec2(80.0, 28.0),
                        egui::Button::new(mono("START", 14.0, palette.fg)),
                    )
                    .clicked()
                {
                    start = true;
                }
            });
        if let Some(draft) = self.restart.as_mut() {
            draft.seed = seed;
        }
        if let Some(path) = picked {
            self.apply_picked_dir(path, true);
        }
        if start {
            let Some(draft) = self.restart.as_ref() else {
                return;
            };
            let Some(index) = self.jobs.iter().position(|job| job.id == draft.id) else {
                self.restart = None;
                return;
            };
            let seed = draft.seed;
            let dest = draft.dest.clone();
            if self.restart_job(index, seed, dest) {
                self.restart = None;
            }
        } else if !open {
            self.restart = None;
        }
    }

    fn show_toasts(&mut self, ctx: &egui::Context, palette: Theme) {
        if self.toasts.is_empty() {
            return;
        }
        let mut clicked = None;
        egui::Area::new(egui::Id::new("toasts"))
            .anchor(egui::Align2::RIGHT_BOTTOM, [-16.0, -16.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.set_max_width(360.0);
                for toast in self.toasts.iter().rev().take(5) {
                    let fill = Color32::from_rgba_unmultiplied(
                        palette.row.r(),
                        palette.row.g(),
                        palette.row.b(),
                        230,
                    );
                    let color = if toast.fail {
                        palette.fail
                    } else {
                        palette.dim
                    };
                    let response = egui::Frame::NONE
                        .fill(fill)
                        .stroke(Stroke::new(1.0_f32, color))
                        .inner_margin(egui::Margin::symmetric(10, 8))
                        .show(ui, |ui| {
                            ui.label(mono(&toast.text, 12.0, color));
                        })
                        .response
                        .on_hover_cursor(egui::CursorIcon::PointingHand);
                    if response.clicked() {
                        clicked = Some(toast.id);
                    }
                    ui.add_space(6.0);
                }
            });
        if let Some(id) = clicked {
            self.toasts.retain(|toast| toast.id != id);
        }
    }
}

fn seed_toggle(ui: &mut egui::Ui, seed: bool, palette: Theme, enabled: bool) -> Option<bool> {
    ui.scope(|ui| {
        ui.style_mut().interaction.interact_radius = 0.0;
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.allocate_ui_with_layout(
            egui::vec2(38.0, 16.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                let share = seed_glyph(ui, true, seed && enabled, enabled, !enabled, palette);
                let leech = seed_glyph(ui, false, !seed && enabled, enabled, !enabled, palette);
                if enabled && share.clicked() {
                    Some(true)
                } else if enabled && leech.clicked() {
                    Some(false)
                } else {
                    None
                }
            },
        )
        .inner
    })
    .inner
}

fn seed_glyph(
    ui: &mut egui::Ui,
    share: bool,
    selected: bool,
    clickable: bool,
    faded: bool,
    palette: Theme,
) -> egui::Response {
    let size = egui::vec2(16.0, 16.0);
    let sense = if clickable {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    let color = if faded {
        Color32::from_rgba_unmultiplied(palette.dim.r(), palette.dim.g(), palette.dim.b(), 70)
    } else if selected {
        palette.fg
    } else {
        palette.dim
    };
    if share {
        paint_share(ui.painter(), rect, color);
    } else {
        paint_leech(ui.painter(), rect, color);
    }
    if selected && !faded {
        ui.painter().rect_stroke(
            rect.expand(2.0),
            2.0,
            Stroke::new(1.2_f32, color),
            egui::StrokeKind::Outside,
        );
    }
    let response = if clickable {
        click_cursor(response)
    } else {
        response.on_hover_cursor(egui::CursorIcon::Default)
    };
    response.on_hover_text(if faded {
        "share and leech apply to magnet downloads"
    } else if share {
        "share: download and keep uploading until you stop"
    } else {
        "leech: download only, never upload"
    })
}

fn paint_share(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let c = rect.center();
    let stroke = Stroke::new(1.2_f32, color);
    painter.circle_stroke(egui::pos2(c.x, c.y + 1.5), 4.4, stroke);
    let mut halo = Vec::with_capacity(11);
    for i in 0..=10 {
        let x = -5.0 + i as f32;
        let y = -5.4 + (x * x) / 16.0;
        halo.push(egui::pos2(c.x + x * 0.85, c.y + y));
    }
    painter.add(egui::Shape::line(halo, stroke));
}

fn paint_leech(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let c = rect.center();
    let stroke = Stroke::new(1.2_f32, color);
    painter.circle_stroke(egui::pos2(c.x, c.y + 1.8), 4.4, stroke);
    painter.line_segment(
        [
            egui::pos2(c.x - 2.4, c.y - 1.6),
            egui::pos2(c.x - 4.6, c.y - 6.4),
        ],
        stroke,
    );
    painter.line_segment(
        [
            egui::pos2(c.x + 2.4, c.y - 1.6),
            egui::pos2(c.x + 4.6, c.y - 6.4),
        ],
        stroke,
    );
}

fn theme_swatch(ui: &mut egui::Ui, fill: Color32, selected: bool, name: &str) -> egui::Response {
    let size = egui::vec2(16.0, 16.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, fill);
    if selected {
        painter.rect_stroke(
            rect.expand(2.0),
            2.0,
            Stroke::new(1.2_f32, fill),
            egui::StrokeKind::Outside,
        );
    }
    click_cursor(response).on_hover_text(format!("theme: {name}"))
}

fn mode_switch(ui: &mut egui::Ui, mode: Mode, palette: Theme) -> egui::Response {
    let magnet = ui.painter().layout_no_wrap(
        Mode::Magnet.label().into(),
        FontId::new(13.0, FontFamily::Monospace),
        if mode == Mode::Magnet {
            palette.fg
        } else {
            palette.dim
        },
    );
    let http = ui.painter().layout_no_wrap(
        Mode::Http.label().into(),
        FontId::new(13.0, FontFamily::Monospace),
        if mode == Mode::Http {
            palette.fg
        } else {
            palette.dim
        },
    );
    let arrow_w = 22.0_f32;
    let pad_x = 10.0_f32;
    let gap = 6.0_f32;
    let size = egui::vec2(
        pad_x * 2.0 + magnet.size().x + gap + arrow_w + gap + http.size().x,
        28.0,
    );
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let painter = ui.painter();
    let stroke = Stroke::new(
        1.0_f32,
        if response.hovered() {
            palette.fg
        } else {
            palette.dim
        },
    );
    painter.rect_filled(rect, 2.0, palette.row);
    painter.rect_stroke(rect, 2.0, stroke, egui::StrokeKind::Inside);
    let magnet_pos = egui::pos2(rect.left() + pad_x, rect.center().y - magnet.size().y / 2.0);
    let arrow_rect = egui::Rect::from_center_size(
        egui::pos2(
            magnet_pos.x + magnet.size().x + gap + arrow_w / 2.0,
            rect.center().y,
        ),
        egui::vec2(arrow_w, 16.0),
    );
    let http_pos = egui::pos2(
        arrow_rect.right() + gap,
        rect.center().y - http.size().y / 2.0,
    );
    painter.galley(magnet_pos, magnet, palette.fg);
    paint_swap_arrows(painter, arrow_rect, palette.fg);
    painter.galley(http_pos, http, palette.fg);
    click_cursor(response).on_hover_text("switch between magnet and HTTP")
}

fn paint_swap_arrows(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let stroke = Stroke::new(1.6_f32, color);
    let c = rect.center();
    let w = 7.0_f32;
    let head = 3.2_f32;
    let top_y = c.y - 3.6;
    let bot_y = c.y + 3.6;
    let left = c.x - w;
    let right = c.x + w;
    painter.line_segment([egui::pos2(left, top_y), egui::pos2(right, top_y)], stroke);
    painter.line_segment(
        [
            egui::pos2(right - head, top_y - head),
            egui::pos2(right, top_y),
        ],
        stroke,
    );
    painter.line_segment(
        [
            egui::pos2(right - head, top_y + head),
            egui::pos2(right, top_y),
        ],
        stroke,
    );
    painter.line_segment([egui::pos2(right, bot_y), egui::pos2(left, bot_y)], stroke);
    painter.line_segment(
        [
            egui::pos2(left + head, bot_y - head),
            egui::pos2(left, bot_y),
        ],
        stroke,
    );
    painter.line_segment(
        [
            egui::pos2(left + head, bot_y + head),
            egui::pos2(left, bot_y),
        ],
        stroke,
    );
}

fn refresh_toggle(ui: &mut egui::Ui, on: bool, palette: Theme) -> egui::Response {
    let color = if on { palette.fg } else { palette.dim };
    let galley = ui.painter().layout_no_wrap(
        "auto-update".into(),
        FontId::new(13.0, FontFamily::Monospace),
        color,
    );
    let icon = 18.0_f32;
    let pad = 8.0_f32;
    let size = egui::vec2(icon + 8.0 + galley.size().x + pad * 2.0, 26.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let painter = ui.painter();
    let fill = if on {
        Color32::from_rgba_unmultiplied(palette.fg.r(), palette.fg.g(), palette.fg.b(), 28)
    } else {
        palette.row
    };
    let stroke = Stroke::new(1.0_f32, if on { palette.fg } else { palette.dim });
    painter.rect_filled(rect, 2.0, fill);
    painter.rect_stroke(rect, 2.0, stroke, egui::StrokeKind::Inside);
    let icon_center = egui::pos2(rect.left() + pad + icon / 2.0, rect.center().y);
    paint_refresh(
        painter,
        icon_center,
        6.5,
        if on { palette.fg } else { palette.dim },
    );
    painter.galley(
        egui::pos2(
            rect.left() + pad + icon + 8.0,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        if on { palette.fg } else { palette.dim },
    );
    click_cursor(response)
        .on_hover_text("check GitHub Releases and replace this app when a newer version exists")
}

fn click_cursor(response: egui::Response) -> egui::Response {
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn note_after_up_to_date(current: &str) -> Option<&'static str> {
    (current == CHECKING_UPDATES).then_some(NO_UPDATES)
}

fn paint_refresh(painter: &egui::Painter, center: egui::Pos2, radius: f32, color: Color32) {
    let stroke = Stroke::new(1.7_f32, color);
    let arc = |start_deg: f32| {
        let mut points = Vec::with_capacity(16);
        for i in 0..=12 {
            let deg = start_deg + 200.0 * (i as f32 / 12.0);
            let rad = deg.to_radians();
            points.push(center + egui::vec2(rad.cos(), rad.sin()) * radius);
        }
        painter.add(egui::Shape::line(points, stroke));
        let end = (start_deg + 200.0).to_radians();
        let tip = center + egui::vec2(end.cos(), end.sin()) * radius;
        let tangent = egui::vec2(-end.sin(), end.cos()) * 6.0;
        painter.arrow(tip, tangent, stroke);
    };
    arc(35.0);
    arc(215.0);
}

fn mono(text: impl Into<String>, size: f32, color: Color32) -> RichText {
    RichText::new(text.into())
        .font(FontId::new(size, FontFamily::Monospace))
        .color(color)
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        let clipped: String = value.chars().take(max.saturating_sub(1)).collect();
        format!("{clipped}…")
    }
}

fn truncate_middle(value: &str, max: usize) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= max {
        return value.to_string();
    }
    if max <= 1 {
        return "…".into();
    }
    let keep = max - 1;
    let left = keep / 2;
    let right = keep - left;
    format!(
        "{}…{}",
        chars[..left].iter().collect::<String>(),
        chars[chars.len() - right..].iter().collect::<String>()
    )
}

fn dest_glyph(ui: &mut egui::Ui, palette: Theme, clickable: bool) -> egui::Response {
    let size = egui::vec2(16.0, 16.0);
    let sense = if clickable {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    paint_folder(ui.painter(), rect, palette.fg);
    let response = if clickable {
        click_cursor(response)
    } else {
        response.on_hover_cursor(egui::CursorIcon::Default)
    };
    response.on_hover_text("choose download folder")
}

fn paint_folder(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let stroke = Stroke::new(1.2_f32, color);
    let tab = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 1.5, rect.top() + 3.0),
        egui::pos2(rect.left() + 7.0, rect.top() + 6.5),
    );
    let body = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 1.5, rect.top() + 6.0),
        egui::pos2(rect.right() - 1.5, rect.bottom() - 2.0),
    );
    painter.rect_stroke(tab, 1.0, stroke, egui::StrokeKind::Inside);
    painter.rect_stroke(body, 1.2, stroke, egui::StrokeKind::Inside);
}

fn worker_loop(rx: Receiver<WorkerCmd>, tx: Sender<WorkerEvent>) {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let handle = runtime.handle().clone();
    let limiter = Arc::new(tokio::sync::Semaphore::new(MAX_ACTIVE_DOWNLOADS));
    let _keep_alive = thread::spawn(move || {
        runtime.block_on(std::future::pending::<()>());
    });
    while let Ok(cmd) = rx.recv() {
        let tx = tx.clone();
        let limiter = Arc::clone(&limiter);
        handle.spawn(async move {
            match cmd {
                WorkerCmd::Download {
                    id,
                    mode,
                    source,
                    snapshot,
                    seed,
                    dest,
                } => {
                    snapshot.set_status("queued");
                    let Ok(_permit) = limiter.acquire_owned().await else {
                        return;
                    };
                    if snapshot.should_stop() {
                        snapshot.mark_stopped();
                        return;
                    }
                    snapshot.set_status("starting");
                    let result = match mode {
                        Mode::Magnet => {
                            start_download_reported(&source, snapshot, seed, dest).await
                        }
                        Mode::Http => start_url_download_reported(&source, snapshot, dest).await,
                    };
                    if let Err(error) = result {
                        if !is_stopped(&error) {
                            let _ = tx.send(WorkerEvent::DownloadFailed {
                                id,
                                error: format!("{error:#}"),
                            });
                        }
                    }
                }
                WorkerCmd::Check => match update::check_latest(AppKind::Gui).await {
                    Ok(Some(update)) => {
                        let _ = tx.send(WorkerEvent::UpdateAvailable(update));
                    }
                    Ok(None) => {
                        let _ = tx.send(WorkerEvent::UpToDate);
                    }
                    Err(error) => {
                        let _ = tx.send(WorkerEvent::UpdateFailed(format!("{error:#}")));
                    }
                },
                WorkerCmd::Install(update) => match update::install_update(&update).await {
                    Ok(_) => {
                        let _ = tx.send(WorkerEvent::Installed(update.tag));
                    }
                    Err(error) => {
                        let _ = tx.send(WorkerEvent::UpdateFailed(format!("{error:#}")));
                    }
                },
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn themes_are_distinct() {
        let a = theme(ThemeId::Phosphor).fg;
        let b = theme(ThemeId::Amber).fg;
        let c = theme(ThemeId::Ice).fg;
        assert_ne!(a, b);
        assert_ne!(b, c);
    }

    #[test]
    fn date_format_is_iso() {
        let value = today();
        assert_eq!(value.len(), 10);
        assert_eq!(&value[4..5], "-");
        assert_eq!(&value[7..8], "-");
    }

    #[test]
    fn caps_active_downloads() {
        assert_eq!(MAX_ACTIVE_DOWNLOADS, 4);
    }

    #[test]
    fn mode_toggle_swaps_magnet_and_http() {
        assert_eq!(Mode::Magnet.toggle(), Mode::Http);
        assert_eq!(Mode::Http.toggle(), Mode::Magnet);
        assert_eq!(Mode::Magnet.label(), "MAGNET");
        assert_eq!(Mode::Http.label(), "HTTP");
    }

    #[test]
    fn in_progress_accent_is_duller_than_settled() {
        for id in [ThemeId::Phosphor, ThemeId::Amber, ThemeId::Ice] {
            let palette = theme(id);
            let live = row_accent(palette, false);
            let done = row_accent(palette, true);
            assert_eq!(done, palette.fg);
            assert_ne!(live, done);
            let dist = |c: Color32| c.r() as i16 + c.g() as i16 + c.b() as i16;
            assert!(dist(live) < dist(done), "{id:?} live should be duller");
        }
    }

    #[test]
    fn up_to_date_follows_a_manual_check() {
        assert_eq!(note_after_up_to_date(CHECKING_UPDATES), Some(NO_UPDATES));
        assert_eq!(note_after_up_to_date(""), None);
        assert_eq!(note_after_up_to_date("installing v3.0.3…"), None);
        assert_eq!(STATUS_HOLD, Duration::from_millis(1500));
    }

    #[test]
    fn middle_truncation_keeps_ends() {
        let value = truncate_middle("/Users/me/Downloads/torrenter/very-long-name", 16);
        assert!(value.contains('…'));
        assert!(value.starts_with("/U"));
        assert!(value.ends_with("me"));
        assert_eq!(value.chars().count(), 16);
    }
}
