use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, FontFamily, FontId, RichText, Sense, Stroke, TextEdit, Vec2};

use crate::credentials::{self, KeySource};
use crate::debrid::{check_account, Secret, Service};
use crate::identity::{self, AppKind, VERSION_TAG};
use crate::jobs::{self, JobKind};
use crate::paths;
use crate::progress::{format_eta, format_size, format_speed, JobSnapshot};
use crate::session::{self, Job, Route, Session, SessionEvent};
use crate::torrent::{magnet_info_hash, magnet_sub_folder};
use crate::update::{self, AvailableUpdate};

const WINDOW_INNER: [f32; 2] = [960.0, 580.0];
const WINDOW_MIN: [f32; 2] = [760.0, 440.0];
const INDETERMINATE_SPAN: f32 = 0.28;
const INDETERMINATE_HZ: f32 = 0.4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ThemeId {
    Phosphor,
    Amber,
    Ice,
    Scarlet,
    Violet,
}

impl ThemeId {
    fn from_config(name: &str) -> Self {
        match update::normalize_theme(name) {
            "amber" => Self::Amber,
            "ice" => Self::Ice,
            "scarlet" => Self::Scarlet,
            "violet" => Self::Violet,
            _ => Self::Phosphor,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Phosphor => "phosphor",
            Self::Amber => "amber",
            Self::Ice => "ice",
            Self::Scarlet => "scarlet",
            Self::Violet => "violet",
        }
    }
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

fn progress_is_indeterminate(status: &str) -> bool {
    matches!(status, "queued" | "starting") || status.contains("Initializing")
}

fn job_bar_fraction(status: &str, downloaded: u64, total: u64) -> Option<f32> {
    if progress_is_indeterminate(status) {
        return None;
    }
    if total <= 1 && downloaded > total {
        return None;
    }
    if total == 0 {
        return Some(0.0);
    }
    Some((downloaded as f32 / total as f32).clamp(0.0, 1.0))
}

fn indeterminate_bar_offset(time: f32, span: f32) -> f32 {
    let travel = (1.0 - span).max(0.0);
    let cycle = (time * INDETERMINATE_HZ).rem_euclid(1.0);
    let ping = 0.5 - 0.5 * (cycle * std::f32::consts::TAU).cos();
    ping * travel
}

fn paint_job_progress(
    ui: &mut egui::Ui,
    accent: Color32,
    track: Color32,
    fraction: Option<f32>,
    width: Option<f32>,
) {
    let track_w = width.unwrap_or_else(|| ui.available_width()).max(1.0);
    let desired = egui::vec2(track_w, 8.0);
    let (rect, _) = ui.allocate_exact_size(desired, Sense::hover());
    ui.painter().rect_filled(rect, 2.0, track);
    let fill = if let Some(fraction) = fraction {
        let mut fill = rect;
        fill.set_width(rect.width() * fraction);
        fill
    } else {
        ui.ctx().request_repaint_after(SLIDER_REPAINT);
        let t = ui.input(|input| input.time) as f32;
        let span = (32.0 / rect.width()).clamp(0.2, INDETERMINATE_SPAN);
        let start = indeterminate_bar_offset(t, span);
        let mut fill = rect;
        fill.min.x += rect.width() * start;
        fill.set_width(rect.width() * span);
        fill
    };
    if fill.width() > 0.5 {
        ui.painter().rect_filled(fill, 2.0, accent);
    }
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
        ThemeId::Scarlet => Theme {
            bg: Color32::from_rgb(10, 6, 8),
            row: Color32::from_rgb(20, 10, 14),
            row_alt: Color32::from_rgb(28, 14, 18),
            fg: Color32::from_rgb(255, 88, 102),
            dim: Color32::from_rgb(176, 52, 64),
            fail: Color32::from_rgb(255, 168, 72),
        },
        ThemeId::Violet => Theme {
            bg: Color32::from_rgb(10, 8, 14),
            row: Color32::from_rgb(18, 14, 26),
            row_alt: Color32::from_rgb(26, 20, 36),
            fg: Color32::from_rgb(210, 186, 255),
            dim: Color32::from_rgb(148, 124, 196),
            fail: Color32::from_rgb(255, 140, 96),
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

fn mode_from_kind(kind: jobs::JobKind) -> Mode {
    match kind {
        jobs::JobKind::Magnet | jobs::JobKind::Debrid => Mode::Magnet,
        jobs::JobKind::Http => Mode::Http,
    }
}

const CHECKING_UPDATES: &str = "checking for updates…";
const NO_UPDATES: &str = "no updates found";
const STATUS_HOLD: Duration = Duration::from_millis(1500);
const FAIL_HOLD: Duration = Duration::from_secs(5);
const SLIDER_REPAINT: Duration = Duration::from_millis(16);

enum WorkerCmd {
    Check,
    Install {
        update: AvailableUpdate,
        snapshot: Arc<JobSnapshot>,
    },
}

enum WorkerEvent {
    UpdateAvailable(AvailableUpdate),
    UpToDate,
    UpdateFailed(String),
    Installed(String),
}

struct Toast {
    id: u64,
    text: String,
    fail: bool,
    expires_at: Option<Instant>,
    progress: Option<Arc<JobSnapshot>>,
}

struct RestartDraft {
    id: u64,
    kind: Mode,
    seed: bool,
    dest: PathBuf,
    /// Magnet jobs: `None` from peers, or through a debrid service.
    route: Option<Service>,
}

struct GuiApp {
    theme: ThemeId,
    applied_theme: ThemeId,
    mode: Mode,
    input: String,
    dest: PathBuf,
    session: Session,
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
    job_query: String,
    glass_tint: u8,
    applied_glass_tint: u8,
    settings_open: bool,
    glass_applied: bool,
    viewport_fullscreen: bool,
    /// Remembered debrid route for magnet jobs (decision: remembered like seed).
    debrid_route: Option<Service>,
    debrid_remove_after: bool,
    connections: Option<u8>,
    /// Settings page: key being typed per service (never shown back).
    key_drafts: HashMap<Service, String>,
    /// Settings page: last account check per service.
    account_notes: HashMap<Service, String>,
    account_tx: Sender<(Service, String)>,
    account_rx: Receiver<(Service, String)>,
    /// A cleared debrid job that may still sit on the service: ask first.
    clear_confirm: Option<u64>,
}

pub fn run() -> eframe::Result<()> {
    update::cleanup_replaced_binary();
    let viewport = egui::ViewportBuilder::default()
        .with_app_id("torrenter")
        .with_title(identity::banner())
        .with_inner_size(WINDOW_INNER)
        .with_min_inner_size(WINDOW_MIN)
        .with_icon(crate::icon::app_icon());
    #[cfg(target_os = "macos")]
    let viewport = viewport.with_transparent(true);
    let options = eframe::NativeOptions {
        viewport,
        // Hyprland (and other Linux compositors) ping the GUI thread. vsync
        // can block that thread in swap buffers so the ping is missed and the
        // shell shows "Application Not Responding".
        vsync: !cfg!(target_os = "linux"),
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
        let theme = ThemeId::from_config(&config.theme);
        let glass_tint = {
            let loaded = update::normalize_glass_tint(config.glass_tint);
            #[cfg(target_os = "windows")]
            {
                update::snap_windows_glass_tint(loaded)
            }
            #[cfg(not(target_os = "windows"))]
            {
                loaded
            }
        };
        let dest = paths::effective_dir(&config);
        let launch_error = paths::ensure_dir(&dest)
            .err()
            .map(|error| format!("{error:#}"));
        let jobs_path = jobs::jobs_path().unwrap_or_else(|_| dest.join("jobs.json"));
        let session = Session::open(dest.clone(), jobs_path, seed);
        session.set_connections(config.connections);
        session.set_debrid_remove_after(config.debrid_remove_after);
        let debrid_route = config
            .debrid_route
            .as_deref()
            .and_then(Service::parse)
            .filter(|service| Service::shipped().contains(service));
        let (account_tx, account_rx) = mpsc::channel();
        let _ = cmd_tx.send(WorkerCmd::Check);
        let mut app = Self {
            theme,
            applied_theme: theme,
            mode: Mode::Magnet,
            input: String::new(),
            dest,
            session,
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
            job_query: String::new(),
            glass_tint,
            applied_glass_tint: glass_tint,
            settings_open: false,
            glass_applied: false,
            viewport_fullscreen: false,
            debrid_route,
            debrid_remove_after: config.debrid_remove_after,
            connections: config.connections,
            key_drafts: HashMap::new(),
            account_notes: HashMap::new(),
            account_tx,
            account_rx,
            clear_confirm: None,
        };
        if let Some(error) = launch_error {
            app.toast_fatal(error);
        }
        app.hydrate_jobs();
        app.apply_theme(&cc.egui_ctx);
        app
    }

    fn surface(&self, color: Color32) -> Color32 {
        #[cfg(target_os = "macos")]
        {
            crate::glass::tint(
                color,
                update::effective_glass_tint(self.glass_tint, self.viewport_fullscreen),
            )
        }
        #[cfg(not(target_os = "macos"))]
        {
            color
        }
    }

    fn card_fill(&self, color: Color32) -> Color32 {
        Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 236)
    }

    fn hydrate_jobs(&mut self) {
        match self.session.hydrate() {
            session::Hydrate::Empty | session::Hydrate::Loaded => {}
            session::Hydrate::Corrupt(error) => {
                self.toast_fail(format!("could not read saved jobs: {error}"));
            }
        }
    }

    fn persist_jobs(&mut self, force: bool) {
        self.session.persist(force);
    }

    fn apply_theme(&self, ctx: &egui::Context) {
        let palette = theme(self.theme);
        let mut visuals = egui::Visuals::dark();
        visuals.override_text_color = Some(palette.fg);
        visuals.panel_fill = self.surface(palette.bg);
        visuals.window_fill = self.surface(palette.bg);
        visuals.extreme_bg_color = self.surface(palette.row);
        visuals.faint_bg_color = self.surface(palette.row_alt);
        visuals.widgets.inactive.bg_fill = self.surface(palette.row);
        visuals.widgets.hovered.bg_fill = self.surface(palette.row_alt);
        visuals.widgets.active.bg_fill = self.surface(palette.row_alt);
        visuals.widgets.open.bg_fill = self.surface(palette.row_alt);
        visuals.selection.bg_fill =
            Color32::from_rgba_unmultiplied(palette.fg.r(), palette.fg.g(), palette.fg.b(), 40);
        let fg = Stroke::new(1.0_f32, palette.fg);
        let dim = Stroke::new(1.0_f32, palette.dim);
        visuals.widgets.noninteractive.fg_stroke = dim;
        visuals.widgets.inactive.fg_stroke = dim;
        visuals.widgets.hovered.fg_stroke = fg;
        visuals.widgets.active.fg_stroke = fg;
        visuals.widgets.open.fg_stroke = fg;
        visuals.window_stroke = Stroke::new(1.0_f32, palette.dim);
        visuals.interact_cursor = Some(egui::CursorIcon::PointingHand);
        ctx.set_visuals(visuals);
        ctx.style_mut(|style| {
            style.spacing.item_spacing = Vec2::new(10.0, 8.0);
        });
    }

    fn queue_job(&mut self) {
        let (kind, via) = match self.mode {
            Mode::Magnet => (JobKind::Magnet, self.debrid_route),
            Mode::Http => (JobKind::Http, None),
        };
        match self.session.queue_with(kind, &self.input, via) {
            Ok(_) => self.input.clear(),
            Err(error) => self.toast_fail(error),
        }
    }

    /// Services with a key, in settings order.
    fn ready_services(&self) -> Vec<Service> {
        Service::shipped()
            .iter()
            .copied()
            .filter(|service| self.session.has_debrid_key(*service))
            .collect()
    }

    /// Debrid control click: off → first service with a key → next → off.
    fn cycle_debrid_route(&mut self) {
        let ready = self.ready_services();
        if ready.is_empty() {
            self.toast_fail("add a debrid API key in settings first");
            self.settings_open = true;
            return;
        }
        let next = match self
            .debrid_route
            .and_then(|now| ready.iter().position(|service| *service == now))
        {
            None => ready.first().copied(),
            Some(index) => ready.get(index + 1).copied(),
        };
        self.debrid_route = next;
        let _ = update::set_debrid_route(next.map(Service::id));
    }

    fn start_account_check(&mut self, service: Service, key: Secret) {
        self.account_notes.insert(service, "checking…".into());
        let tx = self.account_tx.clone();
        thread::spawn(move || {
            let note = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => match runtime.block_on(check_account(service, &key)) {
                    Ok(account) if account.premium => format!(
                        "premium{}",
                        account
                            .expires
                            .map(|until| format!(" until {until}"))
                            .unwrap_or_default()
                    ),
                    Ok(_) => "key works, but the account is not premium".into(),
                    Err(error) => error.to_string(),
                },
                Err(error) => error.to_string(),
            };
            let _ = tx.send((service, note));
        });
    }

    fn restart_job(&mut self, id: u64, route: Option<Service>, seed: bool, dest: PathBuf) -> bool {
        let route = match route {
            Some(service) => Route::Via(service),
            None => Route::Peers,
        };
        let kind = self.session.job(id).map(|job| job.kind);
        let route = if kind == Some(JobKind::Http) {
            Route::Keep
        } else {
            route
        };
        match self.session.restart_with(id, route, seed, dest) {
            Ok(_) => true,
            Err(error) => {
                self.toast_fail(error);
                false
            }
        }
    }

    fn toast_info(&mut self, text: impl Into<String>) {
        self.push_toast(text.into(), false, Some(STATUS_HOLD), None);
    }

    fn toast_hold(&mut self, text: impl Into<String>) {
        self.push_toast(text.into(), false, None, None);
    }

    fn toast_hold_progress(&mut self, text: impl Into<String>, snapshot: Arc<JobSnapshot>) {
        self.push_toast(text.into(), false, None, Some(snapshot));
    }

    fn toast_fail(&mut self, text: impl Into<String>) {
        self.push_toast(text.into(), true, Some(FAIL_HOLD), None);
    }

    fn toast_fatal(&mut self, text: impl Into<String>) {
        self.push_toast(text.into(), true, None, None);
    }

    fn push_toast(
        &mut self,
        text: String,
        fail: bool,
        hold: Option<Duration>,
        progress: Option<Arc<JobSnapshot>>,
    ) {
        if text.is_empty() {
            return;
        }
        self.toasts.retain(|toast| toast.text != text);
        let id = self.next_toast;
        self.next_toast += 1;
        self.toasts.push(Toast {
            id,
            text,
            fail,
            expires_at: hold.map(|duration| Instant::now() + duration),
            progress,
        });
    }

    fn start_install(&mut self, update: AvailableUpdate, label: String) {
        let snapshot = Arc::new(JobSnapshot::default());
        snapshot.set_status("starting");
        let _ = self.tx.send(WorkerCmd::Install {
            update,
            snapshot: Arc::clone(&snapshot),
        });
        self.toast_hold_progress(label, snapshot);
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
                self.session.set_dest(dest.clone());
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
            if let Some(folder) = magnet_sub_folder(dest, source) {
                let nested = dest.join(folder);
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
                        self.start_install(update.clone(), format!("installing {}…", update.tag));
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
                    self.toasts
                        .retain(|toast| !toast.text.starts_with("installing "));
                    if self.auto_update {
                        self.toast_fatal(error);
                    } else {
                        self.toast_fail(error);
                    }
                }
                WorkerEvent::Installed(tag) => {
                    self.pending = None;
                    self.toasts
                        .retain(|toast| !toast.text.starts_with("installing "));
                    self.installed_tag = Some(tag);
                }
            }
        }
        for event in self.session.drain_events() {
            if let SessionEvent::RestoredMissing(missing) = event {
                self.toast_fail(if missing == 1 {
                    "1 saved download is missing from its folder".into()
                } else {
                    format!("{missing} saved downloads are missing from their folders")
                });
            }
        }
    }
}

impl eframe::App for GuiApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.session.persist(true);
        // Windowed value only; fullscreen never mutates `glass_tint`.
        let _ = update::set_glass_tint(self.glass_tint);
    }

    #[cfg(target_os = "macos")]
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        crate::glass::clear_color()
    }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.drain_events();
        self.expire_toasts();
        self.persist_jobs(false);
        if !self.glass_applied {
            crate::glass::apply(frame);
            self.glass_applied = true;
        }
        self.viewport_fullscreen = viewport_is_fullscreen(ctx);
        let paint_tint = update::effective_glass_tint(self.glass_tint, self.viewport_fullscreen);
        if self.applied_theme != self.theme || self.applied_glass_tint != paint_tint {
            self.apply_theme(ctx);
            self.applied_theme = self.theme;
            self.applied_glass_tint = paint_tint;
        }
        let palette = theme(self.theme);
        ctx.request_repaint_after(Duration::from_millis(200));

        const CONTROLS_H: f32 = 28.0;
        const URL_H: f32 = 28.0;
        const DEST_H: f32 = 22.0;
        const GUTTER: f32 = 12.0;
        while let Ok((service, note)) = self.account_rx.try_recv() {
            self.account_notes.insert(service, note);
        }
        let header_h = if self.settings_open {
            16.0 + CONTROLS_H + 16.0
        } else {
            16.0 + CONTROLS_H + GUTTER + URL_H + GUTTER + DEST_H + 16.0
        };
        egui::TopBottomPanel::top("header")
            .exact_height(header_h)
            .resizable(false)
            .frame(
                egui::Frame::NONE
                    .fill(self.surface(palette.bg))
                    .inner_margin(16.0),
            )
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
                                glass_pair(
                                    ui,
                                    identity::REPO,
                                    18.0,
                                    palette.fg,
                                    VERSION_TAG,
                                    10.0,
                                    palette.dim,
                                );
                            },
                        );
                        let right_w = 200.0_f32;
                        let right = egui::Rect::from_min_size(
                            egui::pos2(controls.right() - right_w, controls.top()),
                            egui::vec2(right_w, CONTROLS_H),
                        );
                        ui.allocate_new_ui(
                            egui::UiBuilder::new()
                                .max_rect(right)
                                .layout(egui::Layout::right_to_left(egui::Align::Center)),
                            |ui| {
                                ui.spacing_mut().item_spacing.x = 8.0;
                                if settings_cog(ui, palette, self.settings_open).clicked() {
                                    self.settings_open = !self.settings_open;
                                }
                                if auto_update_toggle(ui, self.auto_update, palette).clicked() {
                                    self.auto_update = !self.auto_update;
                                    let _ = update::set_auto_update(self.auto_update);
                                    if self.auto_update {
                                        let _ = self.tx.send(WorkerCmd::Check);
                                        self.toast_hold(CHECKING_UPDATES);
                                    }
                                }
                            },
                        );
                    },
                );
                if self.settings_open {
                    return;
                }
                ui.add_space(GUTTER);
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
                        let debrid_w = DEBRID_CONTROL_W;
                        let glyphs_w = 42.0_f32 + debrid_w + gap;
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
                        let magnet = self.mode == Mode::Magnet;
                        let via = self.debrid_route.filter(|_| magnet);
                        if debrid_control(ui, via, magnet, palette).clicked() && magnet {
                            self.cycle_debrid_route();
                        }
                        ui.add_space(gap);
                        if let Some(seed) =
                            seed_toggle(ui, self.seed, palette, magnet && via.is_none())
                        {
                            self.seed = seed;
                            self.session.set_seed(seed);
                            let _ = update::set_seed(seed);
                        }
                        ui.add_space(gap);
                        let start = ui.add_sized(
                            egui::vec2(button_w, URL_H),
                            egui::Button::new(ink("START", 14.0, palette.fg)),
                        );
                        if start.clicked()
                            || (response.inner.lost_focus()
                                && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                        {
                            self.queue_job();
                        }
                    },
                );
                ui.add_space(GUTTER);
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
                        let path = click_text(ui, shown, 12.0, palette.dim)
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
            .frame(
                egui::Frame::NONE
                    .fill(self.surface(palette.bg))
                    .inner_margin(12.0),
            )
            .show(ctx, |ui| {
                if self.settings_open {
                    self.show_settings_page(ui, palette);
                    return;
                }
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
                                glass_label(
                                    ui,
                                    format!("{tag} is on disk  ·  restart to run it"),
                                    13.0,
                                    palette.fg,
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.button(ink("later", 12.0, palette.dim)).clicked() {
                                            self.installed_tag = None;
                                        }
                                        if ui
                                            .button(ink("restart now", 12.0, palette.fg))
                                            .clicked()
                                        {
                                            if let Err(error) = update::relaunch() {
                                                self.toast_fail(format!("{error:#}"));
                                            }
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
                                    glass_label(
                                        ui,
                                        format!("a new version is available  ·  {}", available.tag),
                                        13.0,
                                        palette.fg,
                                    );
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui
                                                .button(ink("dismiss", 12.0, palette.dim))
                                                .clicked()
                                            {
                                                self.banner_dismissed = true;
                                            }
                                            if ui
                                                .button(ink("install", 12.0, palette.fg))
                                                .clicked()
                                            {
                                                self.start_install(
                                                    available.clone(),
                                                    "installing update…".into(),
                                                );
                                            }
                                        },
                                    );
                                });
                            });
                        ui.add_space(8.0);
                    }
                }
                if self.session.is_empty() {
                    ui.centered_and_justified(|ui| {
                        glass_label(
                            ui,
                            "no jobs yet — paste a magnet or HTTP URL",
                            14.0,
                            palette.dim,
                        );
                    });
                    return;
                }
                let jobs = self.session.jobs();
                const SEARCH_H: f32 = 28.0;
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), SEARCH_H),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        glass_label(ui, "search", 12.0, palette.dim);
                        let search_id = ui.make_persistent_id("job-search");
                        let focused = ui.memory(|memory| memory.has_focus(search_id));
                        let field_w = ui.available_width().max(160.0);
                        let mut edit = TextEdit::singleline(&mut self.job_query)
                            .id(search_id)
                            .font(FontId::new(14.0, FontFamily::Monospace))
                            .desired_width((field_w - 16.0).max(120.0))
                            .margin(egui::Margin::symmetric(8, 5));
                        if !focused {
                            edit = edit.hint_text("title, URL, folder…");
                        }
                        let response = ui.allocate_ui_with_layout(
                            egui::vec2(field_w, SEARCH_H),
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
                        if response.inner.has_focus()
                            && ui.input(|input| input.key_pressed(egui::Key::Escape))
                        {
                            self.job_query.clear();
                        }
                    },
                );
                ui.add_space(8.0);
                let visible: Vec<usize> = jobs
                    .iter()
                    .enumerate()
                    .filter(|(_, job)| job_matches(job, &self.job_query))
                    .map(|(index, _)| index)
                    .collect();
                if visible.is_empty() {
                    ui.centered_and_justified(|ui| {
                        glass_label(ui, "no jobs match that search", 14.0, palette.dim);
                    });
                    return;
                }
                let mut clear_index = None;
                let mut open_restart = None;
                let mut reveal = None;
                let mut copy_source = None;
                let mut as_torrent = None;
                let mut persist_now = false;
                let row_fill = self.surface(palette.row);
                let row_alt_fill = self.surface(palette.row_alt);
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (stripe, index) in visible.into_iter().enumerate() {
                        if stripe > 0 {
                            ui.add_space(8.0);
                        }
                        let job = &jobs[index];
                        let downloaded = job
                            .snapshot
                            .downloaded
                            .load(std::sync::atomic::Ordering::Relaxed);
                        let uploaded = job
                            .snapshot
                            .uploaded
                            .load(std::sync::atomic::Ordering::Relaxed);
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
                        let fill = if stripe % 2 == 0 {
                            row_fill
                        } else {
                            row_alt_fill
                        };
                        let settled = job.snapshot.is_done()
                            || (total > 0
                                && downloaded >= total
                                && job.snapshot.error().is_none()
                                && !job.snapshot.is_stopped());
                        let accent = row_accent(palette, settled);
                        let card = egui::Frame::NONE
                            .fill(fill)
                            .corner_radius(8)
                            .stroke(Stroke::new(1.0_f32, palette.dim))
                            .inner_margin(10.0);
                        card.show(ui, |ui| {
                                let debrid_view =
                                    job.debrid.as_ref().map(|debrid| (debrid.service, debrid.view()));
                                let fetching = debrid_view.is_some()
                                    && matches!(status.as_str(), "fetching" | "queued" | "starting");
                                ui.horizontal(|ui| {
                                    let badge = match &debrid_view {
                                        Some((service, _)) if fetching => {
                                            format!("MAGNET → {}", service.label())
                                        }
                                        Some((service, _)) => format!("HTTP · via {}", service.label()),
                                        None => job.kind.label().to_string(),
                                    };
                                    glass_label(ui, badge, 13.0, palette.dim);
                                    if job.kind == JobKind::Magnet {
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
                                    if click_text(ui, shown, 13.0, accent)
                                        .on_hover_text(&job.source)
                                        .clicked()
                                    {
                                        reveal = Some(index);
                                    }
                                    ui.add_space(6.0);
                                    if copy_url_icon(ui, palette, fill).clicked() {
                                        copy_source = Some(job.source.clone());
                                    }
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            glass_label(ui, &job.added, 13.0, palette.dim);
                                            if job.snapshot.is_active() {
                                                if ui
                                                    .button(ink("stop", 12.0, palette.fail))
                                                    .clicked()
                                                {
                                                    job.snapshot.mark_stopped();
                                                    persist_now = true;
                                                }
                                                let pause_label = if job.snapshot.is_paused() {
                                                    "resume"
                                                } else {
                                                    "pause"
                                                };
                                                if ui
                                                    .button(ink(pause_label, 12.0, accent))
                                                    .clicked()
                                                {
                                                    job.snapshot.toggle_pause();
                                                    persist_now = true;
                                                }
                                                if fetching
                                                    && ui
                                                        .button(ink("as torrent", 12.0, palette.dim))
                                                        .on_hover_text(
                                                            "stop waiting for the service and download from peers",
                                                        )
                                                        .clicked()
                                                {
                                                    as_torrent = Some(index);
                                                }
                                            } else {
                                                if ui
                                                    .button(ink("clear", 12.0, palette.dim))
                                                    .clicked()
                                                {
                                                    clear_index = Some(index);
                                                }
                                                if (job.kind.takes_magnet()
                                                    || job.snapshot.is_stopped())
                                                    && ui
                                                        .button(ink("restart", 12.0, palette.fg))
                                                        .clicked()
                                                {
                                                    open_restart = Some(index);
                                                }
                                            }
                                        },
                                    );
                                });
                                let fraction = match &debrid_view {
                                    Some((_, state)) if fetching && status == "fetching" => {
                                        Some(f32::from(state.remote_permille) / 1000.0)
                                    }
                                    _ => job_bar_fraction(&status, downloaded, total),
                                };
                                paint_job_progress(ui, accent, palette.row_alt, fraction, None);
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
                                let sharing = job.kind == JobKind::Magnet && job.seed;
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
                                } else if let Some((service, state)) = &debrid_view {
                                    if fetching {
                                        format!(
                                            "{status}  {} fetching {:.1}%",
                                            service.label(),
                                            f32::from(state.remote_permille) / 10.0
                                        )
                                    } else {
                                        let files = if state.files.is_empty() {
                                            String::new()
                                        } else {
                                            format!(
                                                "  file {}/{}",
                                                state.file_index.max(1),
                                                state.files.len()
                                            )
                                        };
                                        format!(
                                            "{status}{files}  D {} @ {}  {}  eta {eta}",
                                            format_size(downloaded),
                                            format_speed(job.speed),
                                            format_size(total),
                                        )
                                    }
                                } else if job.kind == JobKind::Http {
                                    format!(
                                        "{status}  D {} @ {}  {}  eta {eta}",
                                        format_size(downloaded),
                                        format_speed(job.speed),
                                        format_size(total),
                                    )
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
                                glass_label(ui, line, 12.0, color);
                            });
                    }
                });
                if let Some(index) = reveal {
                    if let Some(job) = jobs.get(index) {
                        self.reveal_path(mode_from_kind(job.kind), &job.source, &job.dest);
                    }
                }
                if let Some(source) = copy_source {
                    if source.is_empty() {
                        self.toast_fail("nothing to copy");
                    } else {
                        ctx.copy_text(source);
                        self.toast_info("copied");
                    }
                }
                if let Some(index) = as_torrent {
                    if let Some(job) = jobs.get(index) {
                        let (id, dest) = (job.id, job.dest.clone());
                        self.restart_job(id, None, self.seed, dest);
                    }
                }
                if let Some(index) = open_restart {
                    if let Some(job) = jobs.get(index) {
                        self.restart = Some(RestartDraft {
                            id: job.id,
                            kind: mode_from_kind(job.kind),
                            seed: job.seed,
                            dest: job.dest.clone(),
                            route: job.debrid.as_ref().map(|debrid| debrid.service),
                        });
                    }
                } else if let Some(index) = clear_index {
                    if let Some(job) = jobs.get(index) {
                        if self.restart.as_ref().is_some_and(|draft| draft.id == job.id) {
                            self.restart = None;
                        }
                        if self.session.clear_asks_about_remote(job.id) {
                            self.clear_confirm = Some(job.id);
                        } else {
                            self.session.remove(job.id);
                        }
                    }
                    persist_now = true;
                }
                if persist_now {
                    self.persist_jobs(true);
                }
            });

        self.show_clear_confirm(ctx, palette);
        self.show_restart_popup(ctx, palette);
        self.show_toasts(ctx, palette);
    }
}

impl GuiApp {
    /// Full settings page in place of the input row and jobs (the cog turns
    /// into a back arrow). Downloads keep running underneath.
    fn show_settings_page(&mut self, ui: &mut egui::Ui, palette: Theme) {
        #[cfg(target_os = "macos")]
        let mut persist_tint = false;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_max_width(640.0);
                ui.spacing_mut().item_spacing.y = 6.0;
                settings_heading(ui, "appearance", palette);
                ui.horizontal(|ui| {
                    glass_label(ui, "theme", 12.0, palette.dim);
                    ui.add_space(12.0);
                    ui.scope(|ui| {
                        ui.style_mut().interaction.interact_radius = 0.0;
                        for (id, name) in [
                            (ThemeId::Phosphor, "phosphor"),
                            (ThemeId::Amber, "amber"),
                            (ThemeId::Ice, "ice"),
                            (ThemeId::Scarlet, "scarlet"),
                            (ThemeId::Violet, "violet"),
                        ] {
                            if theme_swatch(ui, theme(id).fg, self.theme == id, name).clicked()
                                && self.theme != id
                            {
                                self.theme = id;
                                let _ = update::set_theme(id.as_str());
                            }
                        }
                    });
                });
                #[cfg(target_os = "macos")]
                {
                    let locked = self.viewport_fullscreen;
                    let shown = update::effective_glass_tint(self.glass_tint, locked);
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        glass_label(ui, "tint", 12.0, palette.dim);
                        ui.add_space(12.0);
                        glass_label(
                            ui,
                            format!("{}%", update::glass_tint_percent(shown)),
                            12.0,
                            palette.fg,
                        );
                    });
                    ui.allocate_ui(egui::vec2(260.0, 24.0), |ui| {
                        if locked {
                            let mut locked_tint = shown;
                            tint_slider(ui, &mut locked_tint, palette, true);
                        } else {
                            let response = tint_slider(ui, &mut self.glass_tint, palette, false);
                            if response.drag_stopped()
                                || (response.changed() && !response.dragged())
                            {
                                persist_tint = true;
                            }
                        }
                    });
                }

                settings_heading(ui, "updates", palette);
                if labeled_switch(ui, "auto-update", self.auto_update, palette)
                    .on_hover_text(
                        "check GitHub Releases and replace this app when a newer version exists",
                    )
                    .clicked()
                {
                    self.auto_update = !self.auto_update;
                    let _ = update::set_auto_update(self.auto_update);
                    if self.auto_update {
                        let _ = self.tx.send(WorkerCmd::Check);
                        self.toast_hold(CHECKING_UPDATES);
                    }
                }

                settings_heading(ui, "debrid accounts", palette);
                glass_label(
                    ui,
                    "Uses your own account. The provider's terms apply.",
                    12.0,
                    palette.dim,
                );
                ui.add_space(4.0);
                for service in Service::shipped().iter().copied() {
                    self.debrid_account_card(ui, service, palette);
                    ui.add_space(6.0);
                }

                settings_heading(ui, "downloads", palette);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    glass_label(ui, "connections per download", 12.0, palette.dim);
                    let auto = self.connections.is_none();
                    if route_chip(ui, "auto (4)", auto, palette).clicked() && !auto {
                        self.set_connections(None);
                    }
                    for parts in [1u8, 2, 4, 8, 16] {
                        let label = parts.to_string();
                        let on = self.connections == Some(parts);
                        if route_chip(ui, &label, on, palette).clicked() && !on {
                            self.set_connections(Some(parts));
                        }
                    }
                });
                glass_label(
                    ui,
                    "applies to new HTTP and debrid downloads; each service caps it (Premiumize 1)",
                    11.0,
                    palette.dim,
                );
                ui.add_space(4.0);
                if labeled_switch(
                    ui,
                    "remove from service after download",
                    self.debrid_remove_after,
                    palette,
                )
                .on_hover_text("only torrents torrenter added, and only once every file landed")
                .clicked()
                {
                    self.debrid_remove_after = !self.debrid_remove_after;
                    self.session
                        .set_debrid_remove_after(self.debrid_remove_after);
                    let _ = update::set_debrid_remove_after(self.debrid_remove_after);
                }
            });
        #[cfg(target_os = "macos")]
        if persist_tint {
            let _ = update::set_glass_tint(self.glass_tint);
        }
    }

    fn set_connections(&mut self, parts: Option<u8>) {
        self.connections = parts;
        self.session.set_connections(parts);
        let _ = update::set_connections(parts);
    }

    fn debrid_account_card(&mut self, ui: &mut egui::Ui, service: Service, palette: Theme) {
        let source = credentials::key_for(service).map(|(_, source)| source);
        let fill = self.card_fill(palette.row);
        egui::Frame::NONE
            .fill(fill)
            .corner_radius(8)
            .stroke(Stroke::new(1.0_f32, palette.dim))
            .inner_margin(10.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    glass_label(ui, service.label(), 14.0, palette.fg);
                    ui.add_space(8.0);
                    let (text, color) = match source {
                        Some(KeySource::Env) => {
                            (format!("key from {}", service.env_var()), palette.fg)
                        }
                        Some(KeySource::File) => ("key saved".to_string(), palette.fg),
                        None => ("no key".to_string(), palette.dim),
                    };
                    glass_label(ui, text, 12.0, color);
                });
                if let Some(note) = self.account_notes.get(&service) {
                    glass_label(ui, note, 12.0, palette.dim);
                }
                ui.add_space(4.0);
                let mut save = false;
                let mut test = false;
                let mut remove = false;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let draft = self.key_drafts.entry(service).or_default();
                    let id = ui.make_persistent_id(("debrid-key", service.id()));
                    let focused = ui.memory(|memory| memory.has_focus(id));
                    let mut edit = TextEdit::singleline(draft)
                        .id(id)
                        .password(true)
                        .font(FontId::new(13.0, FontFamily::Monospace))
                        .desired_width(260.0)
                        .margin(egui::Margin::symmetric(8, 5));
                    if !focused {
                        edit = edit.hint_text("paste API key");
                    }
                    let response = ui.add(edit);
                    let typed = !draft.trim().is_empty();
                    if typed
                        && (ui.button(ink("save", 12.0, palette.fg)).clicked()
                            || (response.lost_focus()
                                && ui.input(|input| input.key_pressed(egui::Key::Enter))))
                    {
                        save = true;
                    }
                    if (typed || source.is_some())
                        && ui.button(ink("test", 12.0, palette.fg)).clicked()
                    {
                        test = true;
                    }
                    if source == Some(KeySource::File)
                        && ui.button(ink("remove", 12.0, palette.fail)).clicked()
                    {
                        remove = true;
                    }
                });
                let draft = self.key_drafts.get(&service).cloned().unwrap_or_default();
                if save {
                    if let Some(key) = Secret::new(draft) {
                        match credentials::set_key(service, &key) {
                            Ok(()) => {
                                self.key_drafts.remove(&service);
                                self.toast_info(format!("{} key saved", service.label()));
                                self.start_account_check(service, key);
                            }
                            Err(error) => self.toast_fail(format!("{error:#}")),
                        }
                    }
                } else if test {
                    let key = Secret::new(draft)
                        .or_else(|| credentials::key_for(service).map(|(key, _)| key));
                    if let Some(key) = key {
                        self.start_account_check(service, key);
                    }
                } else if remove {
                    match credentials::remove_key(service) {
                        Ok(()) => {
                            self.account_notes.remove(&service);
                            if self.debrid_route == Some(service) {
                                self.debrid_route = None;
                                let _ = update::set_debrid_route(None);
                            }
                            self.toast_info(format!("{} key removed", service.label()));
                        }
                        Err(error) => self.toast_fail(format!("{error:#}")),
                    }
                }
            });
    }

    /// Clearing a debrid job torrenter added that never finished: ask whether
    /// it should also go from the service.
    fn show_clear_confirm(&mut self, ctx: &egui::Context, palette: Theme) {
        let Some(id) = self.clear_confirm else {
            return;
        };
        let Some(job) = self.session.job(id) else {
            self.clear_confirm = None;
            return;
        };
        let label = job
            .debrid
            .as_ref()
            .map(|debrid| debrid.service.label())
            .unwrap_or("the service");
        let mut open = true;
        let mut choice = None;
        egui::Window::new("clear")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .frame(
                egui::Frame::NONE
                    .fill(self.card_fill(palette.row))
                    .stroke(Stroke::new(1.0_f32, palette.dim))
                    .inner_margin(12.0)
                    .corner_radius(8),
            )
            .show(ctx, |ui| {
                ui.set_min_width(360.0);
                glass_label(
                    ui,
                    format!("Also remove it from {label}?"),
                    13.0,
                    palette.fg,
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui
                        .button(ink(format!("remove from {label}"), 12.0, palette.fail))
                        .clicked()
                    {
                        choice = Some(true);
                    }
                    if ui
                        .button(ink(format!("keep it on {label}"), 12.0, palette.fg))
                        .clicked()
                    {
                        choice = Some(false);
                    }
                });
            });
        match choice {
            Some(true) => {
                self.session.remove_with_remote(id);
                self.clear_confirm = None;
                self.persist_jobs(true);
            }
            Some(false) => {
                self.session.remove(id);
                self.clear_confirm = None;
                self.persist_jobs(true);
            }
            None if !open => self.clear_confirm = None,
            None => {}
        }
    }

    fn show_restart_popup(&mut self, ctx: &egui::Context, palette: Theme) {
        let Some(draft) = self.restart.as_ref() else {
            return;
        };
        if !self.session.has_job(draft.id) {
            self.restart = None;
            return;
        }
        let mut open = true;
        let mut picked = None;
        let mut start = false;
        let mut seed = draft.seed;
        let mut route = draft.route;
        let dest = draft.dest.clone();
        let kind = draft.kind;
        let ready = self.ready_services();
        egui::Window::new("restart")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .frame(
                egui::Frame::NONE
                    .fill(self.card_fill(palette.row))
                    .stroke(Stroke::new(1.0_f32, palette.dim))
                    .inner_margin(12.0)
                    .corner_radius(8),
            )
            .show(ctx, |ui| {
                ui.set_min_width(420.0);
                if kind == Mode::Magnet {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        glass_label(ui, "route", 12.0, palette.dim);
                        if route_chip(ui, "peers", route.is_none(), palette).clicked() {
                            route = None;
                        }
                        for service in &ready {
                            if route_chip(ui, service.label(), route == Some(*service), palette)
                                .clicked()
                            {
                                route = Some(*service);
                            }
                        }
                    });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        glass_label(ui, "mode", 12.0, palette.dim);
                        if let Some(value) = seed_toggle(ui, seed, palette, route.is_none()) {
                            seed = value;
                        }
                    });
                    ui.add_space(8.0);
                }
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let folder = dest_glyph(ui, palette, true);
                    let shown = truncate_middle(&dest.display().to_string(), 48);
                    let path = click_text(ui, shown, 12.0, palette.dim)
                        .on_hover_text(dest.display().to_string());
                    if folder.clicked() || path.clicked() {
                        picked = Self::pick_folder(&dest);
                    }
                });
                ui.add_space(12.0);
                if ui
                    .add_sized(
                        egui::vec2(80.0, 28.0),
                        egui::Button::new(ink("START", 14.0, palette.fg)),
                    )
                    .clicked()
                {
                    start = true;
                }
            });
        if let Some(draft) = self.restart.as_mut() {
            draft.seed = seed;
            draft.route = route;
        }
        if let Some(path) = picked {
            self.apply_picked_dir(path, true);
        }
        if start {
            let Some(draft) = self.restart.as_ref() else {
                return;
            };
            let seed = draft.seed;
            let dest = draft.dest.clone();
            let id = draft.id;
            let route = draft.route;
            if self.restart_job(id, route, seed, dest) {
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
                ui.with_layout(egui::Layout::top_down(egui::Align::RIGHT), |ui| {
                    ui.spacing_mut().item_spacing.y = 6.0;
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
                        let installing = toast.progress.is_some();
                        let response = egui::Frame::NONE
                            .fill(fill)
                            .stroke(Stroke::new(1.0_f32, color))
                            .inner_margin(egui::Margin::symmetric(10, 8))
                            .show(ui, |ui| {
                                ui.set_max_width(360.0);
                                let label = glass_label(ui, &toast.text, 12.0, color);
                                if let Some(snapshot) = &toast.progress {
                                    ui.add_space(6.0);
                                    let downloaded = snapshot
                                        .downloaded
                                        .load(std::sync::atomic::Ordering::Relaxed);
                                    let total =
                                        snapshot.total.load(std::sync::atomic::Ordering::Relaxed);
                                    let fraction =
                                        job_bar_fraction(&snapshot.status(), downloaded, total);
                                    paint_job_progress(
                                        ui,
                                        palette.fg,
                                        palette.row_alt,
                                        fraction,
                                        Some(label.rect.width()),
                                    );
                                }
                            })
                            .response
                            .on_hover_cursor(if installing {
                                egui::CursorIcon::Default
                            } else {
                                egui::CursorIcon::PointingHand
                            });
                        if !installing && response.clicked() {
                            clicked = Some(toast.id);
                        }
                    }
                });
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

fn viewport_is_fullscreen(ctx: &egui::Context) -> bool {
    ctx.input(|input| input.viewport().fullscreen == Some(true))
}

#[cfg(target_os = "macos")]
fn tint_slider(ui: &mut egui::Ui, value: &mut u8, palette: Theme, locked: bool) -> egui::Response {
    let min = update::GLASS_TINT_MIN;
    let max = update::GLASS_TINT_MAX;
    let knob_r = 5.0_f32;
    let width = ui.available_width().max(1.0);
    let sense = if locked {
        Sense::hover()
    } else {
        Sense::click_and_drag()
    };
    let (rect, mut response) = ui.allocate_exact_size(egui::vec2(width, 22.0), sense);
    let track = rect.shrink2(egui::vec2(knob_r, 0.0));
    let span = track.width().max(1.0);
    if !locked && (response.clicked() || response.dragged()) {
        if let Some(pointer) = response.interact_pointer_pos() {
            let t = ((pointer.x - track.left()) / span).clamp(0.0, 1.0);
            let next = if t <= 0.0 {
                min
            } else if t >= 1.0 {
                max
            } else {
                min + (t * f32::from(max - min)).round() as u8
            };
            if next != *value {
                *value = next;
                response.mark_changed();
            }
        }
    }
    let t = f32::from(*value - min) / f32::from(max - min);
    let y = track.center().y;
    let knob = egui::pos2(track.left() + t * span, y);
    let painter = ui.painter();
    painter.line_segment(
        [egui::pos2(track.left(), y), knob],
        Stroke::new(2.0_f32, palette.fg),
    );
    painter.line_segment(
        [knob, egui::pos2(track.right(), y)],
        Stroke::new(1.0_f32, palette.dim),
    );
    painter.circle_filled(knob, knob_r, if locked { palette.dim } else { palette.fg });
    if locked {
        response.on_hover_text("tint is locked at 100% in fullscreen")
    } else {
        click_cursor(response).on_hover_text("how heavy the theme wash sits on the blur")
    }
}

fn settings_cog(ui: &mut egui::Ui, palette: Theme, open: bool) -> egui::Response {
    let size = egui::vec2(28.0, 28.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let color = if open || response.hovered() {
        palette.fg
    } else {
        palette.dim
    };
    let painter = ui.painter();
    paint_brush_stroke(painter, rect.shrink(1.5));
    let stroke = Stroke::new(1.3_f32, color);
    let c = rect.center();
    if open {
        // Back arrow: settings replace the main screen until closed.
        painter.line_segment(
            [c + egui::vec2(6.0, 0.0), c + egui::vec2(-6.0, 0.0)],
            stroke,
        );
        painter.line_segment(
            [c + egui::vec2(-6.0, 0.0), c + egui::vec2(-1.5, -4.5)],
            stroke,
        );
        painter.line_segment(
            [c + egui::vec2(-6.0, 0.0), c + egui::vec2(-1.5, 4.5)],
            stroke,
        );
        return click_cursor(response).on_hover_text("back");
    }
    painter.circle_stroke(c, 4.8, stroke);
    painter.circle_filled(c, 1.6, color);
    for step in 0..6 {
        let rad = (step as f32 * 60.0).to_radians();
        let dir = egui::vec2(rad.cos(), rad.sin());
        painter.line_segment([c + dir * 6.2, c + dir * 8.4], stroke);
    }
    click_cursor(response).on_hover_text("settings")
}

fn settings_heading(ui: &mut egui::Ui, text: &str, palette: Theme) {
    ui.add_space(14.0);
    glass_label(ui, text, 13.0, palette.fg);
    let width = ui.available_width().min(640.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 1.0), Sense::hover());
    ui.painter().line_segment(
        [rect.left_center(), rect.right_center()],
        Stroke::new(1.0_f32, palette.dim),
    );
    ui.add_space(4.0);
}

/// Width the input row reserves for the debrid control.
const DEBRID_CONTROL_W: f32 = 120.0;

/// "debrid" (off) or "via TorBox" (on); faded outside MAGNET mode.
fn debrid_control(
    ui: &mut egui::Ui,
    via: Option<Service>,
    enabled: bool,
    palette: Theme,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(DEBRID_CONTROL_W, 22.0), Sense::click());
    let painter = ui.painter();
    let (text, color) = match via {
        Some(service) => (format!("via {}", service.label()), palette.fg),
        None => ("debrid".to_string(), palette.dim),
    };
    let color = if enabled {
        color
    } else {
        Color32::from_rgba_unmultiplied(palette.dim.r(), palette.dim.g(), palette.dim.b(), 70)
    };
    let galley = painter.layout_no_wrap(text, FontId::new(12.0, FontFamily::Monospace), color);
    let pos = egui::pos2(
        rect.center().x - galley.size().x / 2.0,
        rect.center().y - galley.size().y / 2.0,
    );
    painter.galley(pos, galley, color);
    if via.is_some() && enabled {
        painter.rect_stroke(
            rect.shrink(1.0),
            4.0,
            Stroke::new(1.0_f32, color),
            egui::StrokeKind::Inside,
        );
    }
    let response = if enabled {
        click_cursor(response)
    } else {
        response
    };
    response.on_hover_text(if !enabled {
        "debrid applies to magnet downloads"
    } else if via.is_some() {
        "download through your debrid service (click to switch or turn off)"
    } else {
        "download from peers; click to use your debrid service"
    })
}

fn route_chip(ui: &mut egui::Ui, text: &str, selected: bool, palette: Theme) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(
        text.into(),
        FontId::new(12.0, FontFamily::Monospace),
        if selected { palette.fg } else { palette.dim },
    );
    let size = galley.size() + egui::vec2(14.0, 8.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let color = if selected || response.hovered() {
        palette.fg
    } else {
        palette.dim
    };
    let painter = ui.painter();
    painter.galley(rect.min + egui::vec2(7.0, 4.0), galley, color);
    if selected {
        painter.rect_stroke(
            rect,
            4.0,
            Stroke::new(1.0_f32, color),
            egui::StrokeKind::Inside,
        );
    }
    click_cursor(response)
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

fn auto_update_toggle(ui: &mut egui::Ui, on: bool, palette: Theme) -> egui::Response {
    labeled_switch(ui, "auto-update", on, palette)
        .on_hover_text("check GitHub Releases and replace this app when a newer version exists")
}

fn labeled_switch(ui: &mut egui::Ui, label: &str, on: bool, palette: Theme) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(
        label.into(),
        FontId::new(12.0, FontFamily::Monospace),
        palette.dim,
    );
    let track_w = 32.0_f32;
    let track_h = 16.0_f32;
    let gap = 18.0_f32;
    let size = egui::vec2(galley.size().x + gap + track_w, 26.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let painter = ui.painter();
    let text_pos = egui::pos2(rect.left(), rect.center().y - galley.size().y / 2.0);
    paint_washed_galley(painter, text_pos, galley, palette.dim);
    let track = egui::Rect::from_center_size(
        egui::pos2(rect.right() - track_w / 2.0, rect.center().y),
        egui::vec2(track_w, track_h),
    );
    let rounding = track_h / 2.0;
    let stroke = Stroke::new(1.0_f32, if on { palette.fg } else { palette.dim });
    if on {
        painter.rect_filled(track, rounding, palette.fg);
    }
    painter.rect_stroke(track, rounding, stroke, egui::StrokeKind::Inside);
    let knob_r = 5.0_f32;
    let knob_x = if on {
        track.right() - 2.5 - knob_r
    } else {
        track.left() + 2.5 + knob_r
    };
    let knob_fill = if on { palette.bg } else { palette.dim };
    painter.circle_filled(egui::pos2(knob_x, track.center().y), knob_r, knob_fill);
    click_cursor(response)
}

fn copy_url_icon(ui: &mut egui::Ui, palette: Theme, page: Color32) -> egui::Response {
    let size = egui::vec2(16.0, 16.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    paint_copy(ui.painter(), rect, palette.dim, page);
    click_cursor(response).on_hover_text("copy URL")
}

fn paint_copy(painter: &egui::Painter, rect: egui::Rect, color: Color32, page: Color32) {
    let stroke = Stroke::new(1.2_f32, color);
    let back = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 5.0, rect.top() + 2.5),
        egui::pos2(rect.right() - 2.5, rect.bottom() - 5.0),
    );
    let front = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 2.5, rect.top() + 5.0),
        egui::pos2(rect.right() - 5.0, rect.bottom() - 2.5),
    );
    painter.rect_stroke(back, 1.5, stroke, egui::StrokeKind::Inside);
    painter.rect_filled(front, 1.5, page);
    painter.rect_stroke(front, 1.5, stroke, egui::StrokeKind::Inside);
}

fn click_cursor(response: egui::Response) -> egui::Response {
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn click_text(
    ui: &mut egui::Ui,
    text: impl Into<String>,
    size: f32,
    color: Color32,
) -> egui::Response {
    click_cursor(glass_text(ui, text, size, color, Sense::click()))
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

fn glass_text_wash() -> Color32 {
    Color32::from_rgba_unmultiplied(16, 14, 16, 110)
}

fn paint_brush_stroke(painter: &egui::Painter, rect: egui::Rect) {
    if !cfg!(target_os = "macos") {
        return;
    }
    if rect.width() < 0.5 || rect.height() < 0.5 {
        return;
    }
    let wash = glass_text_wash();
    // One highlighter smear: even thickness, round finger-pad caps (not giant end dots).
    let smear = rect.expand2(egui::vec2(5.0, 2.5));
    let radius = (smear.height() * 0.5).max(4.0);
    painter.rect_filled(smear, radius, wash);
}

fn paint_washed_galley(
    painter: &egui::Painter,
    pos: egui::Pos2,
    galley: std::sync::Arc<egui::Galley>,
    color: Color32,
) {
    paint_brush_stroke(
        painter,
        egui::Rect::from_min_size(pos, galley.size()).expand2(egui::vec2(4.0, 2.0)),
    );
    painter.galley(pos, galley, color);
}

fn glass_text(
    ui: &mut egui::Ui,
    text: impl Into<String>,
    size: f32,
    color: Color32,
    sense: Sense,
) -> egui::Response {
    layout_glass_text(ui, text, size, color, sense, false)
}

fn glass_label(
    ui: &mut egui::Ui,
    text: impl Into<String>,
    size: f32,
    color: Color32,
) -> egui::Response {
    layout_glass_text(ui, text, size, color, Sense::hover(), true)
}

fn layout_glass_text(
    ui: &mut egui::Ui,
    text: impl Into<String>,
    size: f32,
    color: Color32,
    sense: Sense,
    wrap: bool,
) -> egui::Response {
    let pad = if cfg!(target_os = "macos") {
        egui::vec2(6.0, 4.0)
    } else {
        egui::vec2(0.0, 0.0)
    };
    let font = FontId::new(size, FontFamily::Monospace);
    let galley = if wrap {
        let wrap_width = (ui.available_width() - pad.x).max(1.0);
        ui.painter().layout(text.into(), font, color, wrap_width)
    } else {
        ui.painter().layout_no_wrap(text.into(), font, color)
    };
    let (rect, response) = ui.allocate_exact_size(galley.size() + pad, sense);
    let pos = egui::pos2(
        rect.left() + pad.x * 0.5,
        rect.center().y - galley.size().y * 0.5,
    );
    paint_washed_galley(ui.painter(), pos, galley, color);
    response
}

fn glass_pair(
    ui: &mut egui::Ui,
    left: impl Into<String>,
    left_size: f32,
    left_color: Color32,
    right: impl Into<String>,
    right_size: f32,
    right_color: Color32,
) -> egui::Response {
    let painter = ui.painter();
    let left_galley = painter.layout_no_wrap(
        left.into(),
        FontId::new(left_size, FontFamily::Monospace),
        left_color,
    );
    let right_galley = painter.layout_no_wrap(
        right.into(),
        FontId::new(right_size, FontFamily::Monospace),
        right_color,
    );
    let gap = 7.0_f32;
    let pad = if cfg!(target_os = "macos") {
        egui::vec2(8.0, 4.0)
    } else {
        egui::vec2(0.0, 0.0)
    };
    let inner_h = left_galley.size().y.max(right_galley.size().y);
    let inner_w = left_galley.size().x + gap + right_galley.size().x;
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(inner_w, inner_h) + pad, Sense::hover());
    let bottom = rect.bottom() - pad.y * 0.5;
    let left_pos = egui::pos2(rect.left() + pad.x * 0.5, bottom - left_galley.size().y);
    let title_mid = left_pos.y + left_galley.size().y * 0.5;
    let right_pos = egui::pos2(
        left_pos.x + left_galley.size().x + gap,
        title_mid - right_galley.size().y * 0.5,
    );
    let smear = egui::Rect::from_min_max(
        egui::pos2(left_pos.x, bottom - inner_h),
        egui::pos2(right_pos.x + right_galley.size().x, bottom),
    );
    paint_brush_stroke(ui.painter(), smear);
    ui.painter().galley(left_pos, left_galley, left_color);
    ui.painter().galley(right_pos, right_galley, right_color);
    response
}

fn ink(text: impl Into<String>, size: f32, color: Color32) -> RichText {
    RichText::new(text.into())
        .font(FontId::new(size, FontFamily::Monospace))
        .color(color)
}

fn job_matches(job: &Job, query: &str) -> bool {
    let needle = query.trim();
    if needle.is_empty() {
        return true;
    }
    let needle = needle.to_lowercase();
    let mut hay = vec![
        job.kind.label().to_string(),
        job.source.clone(),
        job.snapshot.title(),
        job.dest.display().to_string(),
        job.added.clone(),
        job.snapshot.status(),
    ];
    if let Some(hash) = magnet_info_hash(&job.source) {
        hay.push(hash);
    }
    hay.iter()
        .any(|field| field.to_lowercase().contains(&needle))
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
    let _keep_alive = thread::spawn(move || {
        runtime.block_on(std::future::pending::<()>());
    });
    while let Ok(cmd) = rx.recv() {
        let tx = tx.clone();
        handle.spawn(async move {
            match cmd {
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
                WorkerCmd::Install { update, snapshot } => {
                    match update::install_update_reported(&update, snapshot).await {
                        Ok(_) => {
                            let _ = tx.send(WorkerEvent::Installed(update.tag));
                        }
                        Err(error) => {
                            let _ = tx.send(WorkerEvent::UpdateFailed(format!("{error:#}")));
                        }
                    }
                }
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
        let d = theme(ThemeId::Scarlet).fg;
        let e = theme(ThemeId::Violet).fg;
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
        assert_ne!(a, d);
        assert_ne!(b, d);
        assert_ne!(c, d);
        assert_ne!(a, e);
        assert_ne!(d, e);
    }

    #[test]
    fn glass_text_wash_is_dark_enough_for_light_frost() {
        let wash = glass_text_wash();
        assert!(wash.a() >= 90, "wash must sit over a light wallpaper");
        assert!(wash.r() < 32 && wash.g() < 32 && wash.b() < 32);
    }

    #[test]
    fn theme_id_maps_config_names() {
        assert_eq!(ThemeId::from_config("amber").as_str(), "amber");
        assert_eq!(ThemeId::from_config("ICE").as_str(), "ice");
        assert_eq!(ThemeId::from_config("scarlet").as_str(), "scarlet");
        assert_eq!(ThemeId::from_config("violet").as_str(), "violet");
        assert_eq!(ThemeId::from_config("blood").as_str(), "phosphor");
        assert_eq!(ThemeId::from_config("").as_str(), "phosphor");
        assert_eq!(ThemeId::from_config("unknown").as_str(), "phosphor");
    }

    #[test]
    fn date_format_is_iso() {
        let value = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert_eq!(value.len(), 10);
        assert_eq!(&value[4..5], "-");
        assert_eq!(&value[7..8], "-");
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
        for id in [
            ThemeId::Phosphor,
            ThemeId::Amber,
            ThemeId::Ice,
            ThemeId::Scarlet,
            ThemeId::Violet,
        ] {
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
        assert_eq!(FAIL_HOLD, Duration::from_secs(5));
    }

    #[test]
    fn middle_truncation_keeps_ends() {
        let value = truncate_middle("/Users/me/Downloads/torrenter/very-long-name", 16);
        assert!(value.contains('…'));
        assert!(value.starts_with("/U"));
        assert!(value.ends_with("me"));
        assert_eq!(value.chars().count(), 16);
    }

    fn sample_job(title: &str, source: &str, dest: &str) -> Job {
        let snapshot = Arc::new(JobSnapshot::default());
        snapshot.set_title(title);
        Job {
            id: 1,
            kind: JobKind::Http,
            source: source.into(),
            snapshot,
            added: "2026-10-27".into(),
            last_bytes: 0,
            last_uploaded: 0,
            last_check: Instant::now(),
            speed: 0.0,
            up_speed: 0.0,
            seed: false,
            dest: PathBuf::from(dest),
            part_count: 4,
            debrid: None,
        }
    }

    #[test]
    fn restoring_placeholder_total_is_indeterminate() {
        assert!(job_bar_fraction("starting", 25, 100).is_none());
        assert!(
            job_bar_fraction("Initializing torrent client & verifying data...", 25, 100).is_none()
        );
        assert_eq!(job_bar_fraction("verifying", 25, 100), Some(0.25));
        assert!(job_bar_fraction("downloading", 250, 1).is_none());
        assert_eq!(job_bar_fraction("downloading", 25, 100), Some(0.25));
        assert_eq!(job_bar_fraction("paused", 25, 100), Some(0.25));
        assert_eq!(job_bar_fraction("downloading", 0, 0), Some(0.0));
        assert_eq!(SLIDER_REPAINT, Duration::from_millis(16));
    }

    #[test]
    fn indeterminate_bar_ping_pongs_without_wrap_jump() {
        let span = INDETERMINATE_SPAN;
        let travel = 1.0 - span;
        let period = 1.0 / INDETERMINATE_HZ;
        assert!((indeterminate_bar_offset(0.0, span) - 0.0).abs() < 1e-5);
        assert!((indeterminate_bar_offset(period * 0.5, span) - travel).abs() < 1e-4);
        assert!((indeterminate_bar_offset(period, span) - 0.0).abs() < 1e-5);
        let before = indeterminate_bar_offset(period - 0.02, span);
        let after = indeterminate_bar_offset(period + 0.02, span);
        assert!(
            before < 0.04,
            "near wrap should stay at the left, got {before}"
        );
        assert!(
            after < 0.04,
            "after wrap should stay at the left, got {after}"
        );
        let mid_left = indeterminate_bar_offset(period * 0.25, span);
        let mid_right = indeterminate_bar_offset(period * 0.75, span);
        assert!((mid_left - mid_right).abs() < 0.02);
        assert!(mid_left > travel * 0.4 && mid_left < travel * 0.6);
    }

    #[test]
    fn empty_search_matches_every_job() {
        let job = sample_job("Sintel", "https://example.com/sintel.mkv", "/tmp/sintel");
        assert!(job_matches(&job, ""));
        assert!(job_matches(&job, "   "));
    }

    #[test]
    fn search_matches_title_url_and_folder() {
        let job = sample_job("Sintel", "https://example.com/sintel.mkv", "/tmp/films");
        assert!(job_matches(&job, "sintel"));
        assert!(job_matches(&job, "EXAMPLE.COM"));
        assert!(job_matches(&job, "films"));
        assert!(job_matches(&job, "http"));
        assert!(!job_matches(&job, "ubuntu"));
    }
}
