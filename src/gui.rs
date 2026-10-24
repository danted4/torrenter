use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, FontFamily, FontId, RichText, Stroke, TextEdit, Vec2};

use crate::http::{filename_from_url, start_url_download_reported};
use crate::identity::{self, AppKind, VERSION_TAG};
use crate::progress::{format_eta, format_size, format_speed, is_stopped, JobSnapshot};
use crate::torrent::{magnet_display_name, magnet_info_hash, start_download_reported};
use crate::update::{self, AvailableUpdate};
use crate::validate::{is_valid_download_url, is_valid_magnet_url};

#[derive(Clone, Copy, PartialEq, Eq)]
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

#[derive(Clone, Copy, PartialEq, Eq)]
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
}

const MAX_ACTIVE_DOWNLOADS: usize = 4;

enum WorkerCmd {
    Download {
        id: u64,
        mode: Mode,
        source: String,
        snapshot: Arc<JobSnapshot>,
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
    last_check: Instant,
    speed: f64,
}

struct GuiApp {
    theme: ThemeId,
    applied_theme: ThemeId,
    mode: Mode,
    input: String,
    jobs: Vec<Job>,
    next_id: u64,
    tx: Sender<WorkerCmd>,
    events: Receiver<WorkerEvent>,
    auto_update: bool,
    update_note: String,
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
        let auto_update = update::load_config().auto_update;
        let _ = cmd_tx.send(WorkerCmd::Check);
        let app = Self {
            theme: ThemeId::Phosphor,
            applied_theme: ThemeId::Phosphor,
            mode: Mode::Magnet,
            input: String::new(),
            jobs: Vec::new(),
            next_id: 1,
            tx: cmd_tx,
            events: event_rx,
            auto_update,
            update_note: String::new(),
            pending: None,
            banner_dismissed: false,
            installed_tag: None,
        };
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
        ctx.set_visuals(visuals);
        ctx.style_mut(|style| {
            style.spacing.item_spacing = Vec2::new(10.0, 8.0);
        });
    }

    fn queue_job(&mut self) {
        let source = self.input.trim().to_string();
        if source.is_empty() {
            self.update_note = "paste a magnet or HTTP URL first".into();
            return;
        }
        let valid = match self.mode {
            Mode::Magnet => is_valid_magnet_url(&source),
            Mode::Http => is_valid_download_url(&source),
        };
        if !valid {
            self.update_note = format!("invalid {} URL", self.mode.label().to_lowercase());
            return;
        }
        if let Some(note) = self.duplicate_active_note(self.mode, &source) {
            self.update_note = note;
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
        let job = Job {
            id,
            kind: self.mode,
            source: source.clone(),
            snapshot: Arc::clone(&snapshot),
            added: today(),
            last_bytes: 0,
            last_check: Instant::now(),
            speed: 0.0,
        };
        let _ = self.tx.send(WorkerCmd::Download {
            id,
            mode: self.mode,
            source,
            snapshot,
        });
        self.jobs.push(job);
        self.input.clear();
    }

    fn restart_job(&mut self, index: usize) {
        let Some(job) = self.jobs.get(index) else {
            return;
        };
        let kind = job.kind;
        let source = job.source.clone();
        if let Some(note) = self.duplicate_active_note(kind, &source) {
            self.update_note = note;
            return;
        }
        let title = job.snapshot.title();
        let snapshot = Arc::new(JobSnapshot::default());
        snapshot.set_status("queued");
        if !title.is_empty() {
            snapshot.set_title(title);
        }
        let id = self.next_id;
        self.next_id += 1;
        let _ = self.tx.send(WorkerCmd::Download {
            id,
            mode: kind,
            source: source.clone(),
            snapshot: Arc::clone(&snapshot),
        });
        self.jobs[index] = Job {
            id,
            kind,
            source,
            snapshot,
            added: today(),
            last_bytes: 0,
            last_check: Instant::now(),
            speed: 0.0,
        };
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

    fn drain_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                WorkerEvent::UpdateAvailable(update) => {
                    self.banner_dismissed = false;
                    if self.auto_update {
                        let _ = self.tx.send(WorkerCmd::Install(update.clone()));
                        self.update_note = format!("installing {}…", update.tag);
                    } else {
                        self.update_note.clear();
                    }
                    self.pending = Some(update);
                }
                WorkerEvent::UpToDate => {
                    self.pending = None;
                    self.update_note.clear();
                }
                WorkerEvent::UpdateFailed(error) => {
                    if self.auto_update {
                        self.update_note = error;
                    }
                }
                WorkerEvent::Installed(tag) => {
                    self.pending = None;
                    self.update_note.clear();
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
        if self.applied_theme != self.theme {
            self.apply_theme(ctx);
            self.applied_theme = self.theme;
        }
        let palette = theme(self.theme);
        ctx.request_repaint_after(Duration::from_millis(200));

        const HEADER_H: f32 = 142.0;
        const CONTROLS_H: f32 = 70.0;
        const URL_H: f32 = 36.0;
        egui::TopBottomPanel::top("header")
            .exact_height(HEADER_H)
            .resizable(false)
            .frame(egui::Frame::NONE.fill(palette.bg).inner_margin(16.0))
            .show(ctx, |ui| {
                let width = ui.available_width();
                let (controls, _) =
                    ui.allocate_exact_size(egui::vec2(width, CONTROLS_H), egui::Sense::hover());
                ui.allocate_new_ui(
                    egui::UiBuilder::new()
                        .max_rect(controls)
                        .layout(egui::Layout::left_to_right(egui::Align::Min)),
                    |ui| {
                        ui.set_clip_rect(controls);
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(mono(identity::REPO, 18.0, palette.fg));
                                ui.add_space(8.0);
                                ui.label(mono(VERSION_TAG, 12.0, palette.dim));
                            });
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                for mode in [Mode::Magnet, Mode::Http] {
                                    let selected = self.mode == mode;
                                    let text = mono(
                                        mode.label(),
                                        13.0,
                                        if selected { palette.fg } else { palette.dim },
                                    );
                                    if ui.selectable_label(selected, text).clicked() {
                                        self.mode = mode;
                                    }
                                }
                            });
                        });
                        let right = egui::Rect::from_min_size(
                            egui::pos2(controls.right() - 250.0, controls.top()),
                            egui::vec2(250.0, CONTROLS_H),
                        );
                        ui.allocate_new_ui(
                            egui::UiBuilder::new()
                                .max_rect(right)
                                .layout(egui::Layout::top_down(egui::Align::RIGHT)),
                            |ui| {
                                ui.set_clip_rect(right);
                                ui.horizontal(|ui| {
                                    ui.label(mono("theme", 12.0, palette.dim));
                                    ui.add_space(8.0);
                                    for (id, name) in [
                                        (ThemeId::Phosphor, "phosphor"),
                                        (ThemeId::Amber, "amber"),
                                        (ThemeId::Ice, "ice"),
                                    ] {
                                        if theme_swatch(ui, theme(id).fg, self.theme == id, name)
                                            .clicked()
                                        {
                                            self.theme = id;
                                        }
                                    }
                                });
                                ui.add_space(4.0);
                                if refresh_toggle(ui, self.auto_update, palette).clicked() {
                                    self.auto_update = !self.auto_update;
                                    let _ = update::set_auto_update(self.auto_update);
                                    if self.auto_update {
                                        let _ = self.tx.send(WorkerCmd::Check);
                                        self.update_note = "checking for updates…".into();
                                    } else {
                                        self.update_note.clear();
                                    }
                                }
                                if !self.update_note.is_empty() {
                                    ui.painter().text(
                                        egui::pos2(right.right(), right.bottom() - 2.0),
                                        egui::Align2::RIGHT_BOTTOM,
                                        &self.update_note,
                                        FontId::new(11.0, FontFamily::Monospace),
                                        palette.dim,
                                    );
                                }
                            },
                        );
                    },
                );
                ui.add_space(8.0);
                let (url_row, _) =
                    ui.allocate_exact_size(egui::vec2(width, URL_H), egui::Sense::hover());
                let button_w = 72.0_f32;
                let gap = 8.0_f32;
                let edit_w = (url_row.width() * 0.58).clamp(380.0, 620.0);
                let group_w = edit_w + gap + button_w;
                let group = egui::Rect::from_min_size(
                    egui::pos2(url_row.center().x - group_w / 2.0, url_row.top()),
                    egui::vec2(group_w, url_row.height()),
                );
                ui.allocate_new_ui(
                    egui::UiBuilder::new()
                        .max_rect(group)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    |ui| {
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
                        let response = ui.scope(|ui| {
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
                        });
                        ui.add_space(gap);
                        if ui.button(mono("START", 14.0, palette.fg)).clicked()
                            || (response.inner.lost_focus()
                                && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                        {
                            self.queue_job();
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
                                                self.update_note = "installing update…".into();
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
                let mut restart_index = None;
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (index, job) in self.jobs.iter_mut().enumerate() {
                        let now = Instant::now();
                        let downloaded = job
                            .snapshot
                            .downloaded
                            .load(std::sync::atomic::Ordering::Relaxed);
                        let elapsed = now.saturating_duration_since(job.last_check).as_secs_f64();
                        if elapsed >= 0.4 {
                            job.speed =
                                (downloaded.saturating_sub(job.last_bytes) as f64) / elapsed;
                            job.last_bytes = downloaded;
                            job.last_check = now;
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
                        egui::Frame::NONE
                            .fill(fill)
                            .inner_margin(10.0)
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(mono(job.kind.label(), 13.0, palette.dim));
                                    ui.add_space(8.0);
                                    let title = job.snapshot.title();
                                    let shown = if title.is_empty() {
                                        truncate(&job.source, 64)
                                    } else {
                                        truncate(&title, 64)
                                    };
                                    ui.label(mono(shown, 13.0, palette.fg))
                                        .on_hover_text(&job.source);
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
                                                    .button(mono(pause_label, 12.0, palette.fg))
                                                    .clicked()
                                                {
                                                    job.snapshot.toggle_pause();
                                                }
                                            } else if job.snapshot.is_stopped() {
                                                if ui
                                                    .button(mono("clear", 12.0, palette.dim))
                                                    .clicked()
                                                {
                                                    clear_index = Some(index);
                                                }
                                                if ui
                                                    .button(mono("restart", 12.0, palette.fg))
                                                    .clicked()
                                                {
                                                    restart_index = Some(index);
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
                                    ui.visuals_mut().selection.bg_fill = palette.fg;
                                    ui.add(
                                        egui::ProgressBar::new(fraction.clamp(0.0, 1.0))
                                            .desired_height(8.0)
                                            .fill(palette.fg),
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
                                let line = format!(
                                    "{status}  peers {peers}  {} @ {}  {}  eta {eta}",
                                    format_size(downloaded),
                                    format_speed(job.speed),
                                    format_size(total),
                                );
                                let color = if job.snapshot.error().is_some() {
                                    palette.fail
                                } else {
                                    palette.dim
                                };
                                ui.label(mono(line, 12.0, color));
                            });
                    }
                });
                if let Some(index) = restart_index {
                    self.restart_job(index);
                } else if let Some(index) = clear_index {
                    self.jobs.remove(index);
                }
            });
    }
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
    response.on_hover_text(format!("theme: {name}"))
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
    response.on_hover_text("check GitHub Releases and replace this app when a newer version exists")
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
                        Mode::Magnet => start_download_reported(&source, snapshot).await,
                        Mode::Http => start_url_download_reported(&source, snapshot).await,
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
}
