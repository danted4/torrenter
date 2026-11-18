use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, FontFamily, FontId, RichText, Sense, Stroke, TextEdit, Vec2};

use crate::credentials::{self, AccountInfo, KeySource};
use crate::debrid::{check_account, Secret, Service};
use crate::identity::{self, AppKind, VERSION_TAG};
use crate::jobs::{self, JobKind};
use crate::paths;
use crate::progress::{format_eta, format_size, format_speed, JobSnapshot};
use crate::session::{self, Job, Route, Session, SessionEvent};
use crate::torrent::{magnet_info_hash, magnet_sub_folder};
use crate::torrent_file;
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
    matches!(
        status,
        "queued"
            | "starting"
            | crate::progress::FETCHING_METADATA
            | crate::progress::CONNECTING_TO_PEERS
    ) || status.contains("Initializing")
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
    /// Magnet jobs: `None` from peers, or through a debrid account (id).
    route: Option<String>,
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
    /// Repaint on the busy cadence until then (see `BUSY_TAIL`).
    busy_until: Instant,
    /// For worker threads to wake the window.
    repaint: egui::Context,
    next_toast: u64,
    restart: Option<RestartDraft>,
    pending: Option<AvailableUpdate>,
    banner_dismissed: bool,
    installed_tag: Option<String>,
    job_query: String,
    glass_tint: u8,
    applied_glass_tint: u8,
    settings_open: bool,
    /// Which services have a key (credentials.json or env), read once rather
    /// than every frame. `None` = reload; cleared on save / remove and when
    /// settings opens.
    accounts_cache: Option<Vec<AccountInfo>>,
    glass_applied: bool,
    viewport_fullscreen: bool,
    /// Remembered debrid route for magnet jobs (decision: remembered like seed).
    debrid_route: Option<String>,
    debrid_remove_after: bool,
    connections: Option<u8>,
    /// Settings page: the category shown in the content pane.
    settings_tab: SettingsTab,
    /// Settings page: the add-account form, while open.
    add_form: Option<AddAccountForm>,
    /// Settings page: last account check per account id.
    account_notes: HashMap<String, String>,
    account_tx: Sender<(String, String)>,
    account_rx: Receiver<(String, String)>,
    /// Clearing or re-routing a debrid job whose torrent torrenter added on
    /// the service: ask "also remove it from …?" first.
    remote_confirm: Option<RemoteAsk>,
}

/// Settings categories, listed in the sidebar.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum SettingsTab {
    #[default]
    Appearance,
    Downloads,
    Accounts,
    Updates,
}

impl SettingsTab {
    const ALL: [SettingsTab; 4] = [
        SettingsTab::Appearance,
        SettingsTab::Downloads,
        SettingsTab::Accounts,
        SettingsTab::Updates,
    ];

    fn label(self) -> &'static str {
        match self {
            SettingsTab::Appearance => "appearance",
            SettingsTab::Downloads => "downloads",
            SettingsTab::Accounts => "debrid accounts",
            SettingsTab::Updates => "updates",
        }
    }
}

/// Settings: a new account being added (provider first, then name and key;
/// the key is never shown back).
#[derive(Default)]
struct AddAccountForm {
    service: Option<Service>,
    name: String,
    key: String,
}

/// "TorBox", or "work (TorBox)" when the name differs from the provider.
fn account_title(name: &str, service: Service) -> String {
    if name == service.label() {
        name.to_string()
    } else {
        format!("{name} ({})", service.label())
    }
}

/// What waits on the "also remove it from the service?" answer.
#[derive(Clone)]
enum RemoteAsk {
    Clear(u64),
    Reroute {
        id: u64,
        route: Route,
        seed: bool,
        dest: PathBuf,
    },
}

impl RemoteAsk {
    fn id(&self) -> u64 {
        match self {
            RemoteAsk::Clear(id) | RemoteAsk::Reroute { id, .. } => *id,
        }
    }
}

pub fn run() -> eframe::Result<()> {
    update::cleanup_replaced_binary();
    let viewport = egui::ViewportBuilder::default()
        .with_app_id("torrenter")
        .with_title(identity::banner())
        .with_inner_size(WINDOW_INNER)
        .with_min_inner_size(WINDOW_MIN)
        .with_icon(crate::icon::app_icon())
        // .torrent files dropped on the window start like the upload glyph.
        .with_drag_and_drop(true);
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
        let worker_ctx = cc.egui_ctx.clone();
        thread::spawn(move || worker_loop(cmd_rx, event_tx, worker_ctx));
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
        let debrid_route = config.debrid_route.clone().filter(|id| !id.is_empty());
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
            busy_until: Instant::now() + BUSY_TAIL,
            repaint: cc.egui_ctx.clone(),
            next_toast: 1,
            restart: None,
            pending: None,
            banner_dismissed: false,
            installed_tag: None,
            job_query: String::new(),
            glass_tint,
            applied_glass_tint: glass_tint,
            settings_open: false,
            accounts_cache: None,
            glass_applied: false,
            viewport_fullscreen: false,
            debrid_route,
            debrid_remove_after: config.debrid_remove_after,
            connections: config.connections,
            settings_tab: SettingsTab::default(),
            add_form: None,
            account_notes: HashMap::new(),
            account_tx,
            account_rx,
            remote_confirm: None,
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

    /// Queue a `.torrent` file's bytes: the magnet built from it, through the
    /// current route (peers or a debrid account), started at once.
    fn queue_torrent_bytes(&mut self, bytes: &[u8]) {
        let via = self.route_account().map(|account| account.id);
        if let Err(error) = self.session.queue_torrent(bytes, via.as_deref()) {
            self.toast_fail(error);
        }
    }

    /// The upload glyph: pick a `.torrent` and start it.
    fn pick_torrent(&mut self) {
        let picked = rfd::FileDialog::new()
            .set_title("open a .torrent file")
            .add_filter("torrent", &["torrent"])
            .pick_file();
        let Some(path) = picked else {
            return;
        };
        match torrent_file::read_capped(&path) {
            Ok(bytes) => self.queue_torrent_bytes(&bytes),
            Err(error) => self.toast_fail(format!("{error:#}")),
        }
    }

    /// Files dropped on the window: each `.torrent` starts; anything else is
    /// refused with a note.
    fn take_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|input| input.raw.dropped_files.clone());
        for file in dropped {
            let name = file
                .path
                .as_ref()
                .and_then(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| file.name.clone());
            if !name.to_ascii_lowercase().ends_with(".torrent") {
                self.toast_fail("only .torrent files can be dropped here");
                continue;
            }
            let bytes = match (&file.bytes, &file.path) {
                (Some(bytes), _) if bytes.len() <= torrent_file::MAX_TORRENT_FILE => {
                    Ok(bytes.to_vec())
                }
                (_, Some(path)) => torrent_file::read_capped(path),
                _ => Err(anyhow::anyhow!("could not read {name}")),
            };
            match bytes {
                Ok(bytes) => self.queue_torrent_bytes(&bytes),
                Err(error) => self.toast_fail(format!("{error:#}")),
            }
        }
    }

    fn queue_job(&mut self) {
        let (kind, via) = match self.mode {
            Mode::Magnet => (JobKind::Magnet, self.route_account().map(|a| a.id)),
            Mode::Http => (JobKind::Http, None),
        };
        match self.session.queue_with(kind, &self.input, via.as_deref()) {
            Ok(_) => self.input.clear(),
            Err(error) => self.toast_fail(error),
        }
    }

    /// Debrid accounts for offered providers (saved and env), read once and
    /// cached; cleared on save / remove and when settings opens.
    fn accounts(&mut self) -> Vec<AccountInfo> {
        self.accounts_cache
            .get_or_insert_with(|| {
                credentials::accounts()
                    .into_iter()
                    .filter(|account| Service::offered().contains(&account.service))
                    .collect()
            })
            .clone()
    }

    fn account(&mut self, id: &str) -> Option<AccountInfo> {
        self.accounts().into_iter().find(|account| account.id == id)
    }

    /// The remembered route, if its account still exists.
    fn route_account(&mut self) -> Option<AccountInfo> {
        let id = self.debrid_route.clone()?;
        self.account(&id)
    }

    /// Route menu pick: `None` = default (peers), or an account id.
    fn set_debrid_route(&mut self, route: Option<String>) {
        let _ = update::set_debrid_route(route.as_deref());
        self.debrid_route = route;
    }

    /// Open settings on a category (the dropdown opens debrid accounts).
    fn open_settings(&mut self, tab: SettingsTab) {
        self.settings_open = true;
        self.settings_tab = tab;
        self.accounts_cache = None;
    }

    /// The route menu under the select: default (none), each offered
    /// provider (a provider without a key opens settings), manage keys.
    fn route_menu(&mut self, ui: &mut egui::Ui, select: &egui::Response, palette: Theme) {
        let popup_id = ui.make_persistent_id("debrid-route-menu");
        if select.clicked() {
            ui.memory_mut(|memory| memory.toggle_popup(popup_id));
        }
        if !ui.memory(|memory| memory.is_popup_open(popup_id)) {
            return;
        }
        let accounts = self.accounts();
        let current = self.route_account().map(|account| account.id);
        let picked = solid_popup_below(ui, popup_id, select, palette, |ui| {
            ui.set_min_width(select.rect.width().max(240.0));
            ui.spacing_mut().item_spacing.y = 2.0;
            let mut picked = None;
            if menu_row(
                ui,
                "default (none)",
                "from peers",
                current.is_none(),
                palette,
            )
            .clicked()
            {
                picked = Some(RoutePick::Route(None));
            }
            for account in &accounts {
                if menu_row(
                    ui,
                    &account.name,
                    account.service.label(),
                    current.as_deref() == Some(account.id.as_str()),
                    palette,
                )
                .clicked()
                {
                    picked = Some(RoutePick::Route(Some(account.id.clone())));
                }
            }
            ui.separator();
            let manage = if accounts.is_empty() {
                "add a debrid account…"
            } else {
                "manage accounts…"
            };
            if menu_row_centered(ui, manage, None, false, palette).clicked() {
                picked = Some(RoutePick::Settings);
            }
            picked
        })
        .flatten();
        match picked {
            Some(RoutePick::Route(route)) => self.set_debrid_route(route),
            Some(RoutePick::Settings) => self.open_settings(SettingsTab::Accounts),
            None => {}
        }
    }

    fn start_account_check(&mut self, account: String, service: Service, key: Secret) {
        self.account_notes
            .insert(account.clone(), "checking…".into());
        let tx = self.account_tx.clone();
        let wake = RepaintOnDrop(self.repaint.clone());
        thread::spawn(move || {
            let _wake = wake;
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
            let _ = tx.send((account, note));
        });
    }

    fn restart_job(&mut self, id: u64, route: Option<String>, seed: bool, dest: PathBuf) -> bool {
        let route = match route {
            Some(account) => Route::Via(account),
            None => Route::Peers,
        };
        let kind = self.session.job(id).map(|job| job.kind);
        let route = if kind == Some(JobKind::Http) {
            Route::Keep
        } else {
            route
        };
        if self.session.reroute_asks_about_remote(id, &route) {
            // Restarts once answered; closing the question cancels it.
            self.remote_confirm = Some(RemoteAsk::Reroute {
                id,
                route,
                seed,
                dest,
            });
            return true;
        }
        self.restart_routed(id, route, seed, dest, false)
    }

    fn restart_routed(
        &mut self,
        id: u64,
        route: Route,
        seed: bool,
        dest: PathBuf,
        remove_from_service: bool,
    ) -> bool {
        match self
            .session
            .restart_with(id, route, seed, dest, remove_from_service)
        {
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
        self.take_dropped_files(ctx);
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
        paint_drop_hint(ctx, palette);
        // Redraw often only while something moves (downloads, toasts and the
        // update install they show, account checks); idle, once a second.
        // Input still repaints at once; worker replies wait at most that long.
        let moving = self.session.any_active()
            || !self.toasts.is_empty()
            || self.account_notes.values().any(|note| note == "checking…");
        let now = Instant::now();
        if moving {
            self.busy_until = now + BUSY_TAIL;
        }
        // The tail covers what arrives just after: restore probes at launch, a
        // failure notice for a job that just stopped.
        let busy = moving || now < self.busy_until;
        ctx.request_repaint_after(if busy { BUSY_REPAINT } else { IDLE_REPAINT });

        const CONTROLS_H: f32 = 28.0;
        const URL_H: f32 = 28.0;
        const DEST_H: f32 = 22.0;
        const GUTTER: f32 = 12.0;
        while let Ok((account, note)) = self.account_rx.try_recv() {
            self.account_notes.insert(account, note);
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
                                // The title doubles as "home": back to the
                                // main page from settings.
                                let title = glass_pair(
                                    ui,
                                    identity::REPO,
                                    18.0,
                                    palette.fg,
                                    VERSION_TAG,
                                    10.0,
                                    palette.dim,
                                );
                                if click_cursor(title).on_hover_text("main page").clicked() {
                                    self.settings_open = false;
                                }
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
                                    self.accounts_cache = None;
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
                        let magnet = self.mode == Mode::Magnet;
                        let picked = self.route_account();
                        let via = picked.as_ref().filter(|_| magnet);
                        let route_text = route_label(picked.as_ref());
                        // Fixed width (that of "default (none)"); longer
                        // account names are shortened with "…".
                        let route_w = route_select_width(ui, &route_label(None));
                        let glyphs_w = 42.0_f32 + route_w + gap;
                        // MAGNET: room at the right end of the box for the
                        // .torrent file glyph, so text never runs under it.
                        let glyph_room = if magnet { FILE_GLYPH_ROOM } else { 0.0 };
                        let edit_margin = 16.0_f32 + glyph_room;
                        ui.add_space(gap);
                        // The field takes whatever the controls leave.
                        let field_w =
                            (ui.available_width() - gap - glyphs_w - gap - button_w).max(200.0);
                        let edit_w = (field_w - edit_margin).max(160.0);
                        let input_id = ui.make_persistent_id("download-url");
                        let focused = ui.memory(|memory| memory.has_focus(input_id));
                        let mut edit = TextEdit::singleline(&mut self.input)
                            .id(input_id)
                            .font(FontId::new(14.0, FontFamily::Monospace))
                            .desired_width(edit_w)
                            .margin(egui::Margin {
                                left: 8,
                                right: 8 + glyph_room as i8,
                                top: 5,
                                bottom: 5,
                            });
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
                        // Open a .torrent: inside the box, MAGNET only (gone
                        // in HTTP), as on Android and iOS.
                        if magnet && upload_glyph(ui, palette, response.response.rect).clicked() {
                            self.pick_torrent();
                        }
                        ui.add_space(gap);
                        let select = route_select(
                            ui,
                            &route_text,
                            route_w,
                            URL_H,
                            via.is_some(),
                            magnet,
                            palette,
                        );
                        if magnet {
                            self.route_menu(ui, &select, palette);
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
                let account_names: HashMap<String, String> = self
                    .accounts()
                    .into_iter()
                    .map(|account| (account.id, account.name))
                    .collect();
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
                                // Shown by account name (the provider's name
                                // for a single account).
                                let debrid_view = job.debrid.as_ref().map(|debrid| {
                                    let name = account_names
                                        .get(&debrid.account)
                                        .cloned()
                                        .unwrap_or_else(|| debrid.service.label().to_string());
                                    (name, debrid.view())
                                });
                                let fetching = debrid_view.is_some()
                                    && matches!(status.as_str(), "fetching" | "queued" | "starting");
                                ui.horizontal(|ui| {
                                    let badge = match &debrid_view {
                                        Some((name, _)) if fetching => format!("MAGNET → {name}"),
                                        Some((name, _)) => format!("HTTP · via {name}"),
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
                                } else if let Some((name, state)) = &debrid_view {
                                    if fetching {
                                        format!(
                                            "{status}  {name} fetching {:.1}%",
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
                            route: job.debrid.as_ref().map(|debrid| debrid.account.clone()),
                        });
                    }
                } else if let Some(index) = clear_index {
                    if let Some(job) = jobs.get(index) {
                        if self.restart.as_ref().is_some_and(|draft| draft.id == job.id) {
                            self.restart = None;
                        }
                        if self.session.clear_asks_about_remote(job.id) {
                            self.remote_confirm = Some(RemoteAsk::Clear(job.id));
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

        self.show_remote_confirm(ctx, palette);
        self.show_restart_popup(ctx, palette);
        self.show_toasts(ctx, palette);
    }
}

impl GuiApp {
    /// Full settings page in place of the input row and jobs (the cog turns
    /// into a back arrow). Downloads keep running underneath.
    /// Settings: a category sidebar on the left (stays put) and the chosen
    /// category's setting cards on the right — title and description on the
    /// left of each card, its control flush right, like the download rows.
    fn show_settings_page(&mut self, ui: &mut egui::Ui, palette: Theme) {
        let height = ui.available_height();
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(SIDEBAR_W, height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_width(SIDEBAR_W);
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.add_space(4.0);
                    for tab in SettingsTab::ALL {
                        if menu_row(ui, tab.label(), "", self.settings_tab == tab, palette)
                            .clicked()
                        {
                            self.settings_tab = tab;
                        }
                    }
                },
            );
            let (line, _) = ui.allocate_exact_size(egui::vec2(1.0, height), Sense::hover());
            let edge = Color32::from_rgba_unmultiplied(
                palette.dim.r(),
                palette.dim.g(),
                palette.dim.b(),
                90,
            );
            ui.painter().line_segment(
                [line.center_top(), line.center_bottom()],
                Stroke::new(1.0_f32, edge),
            );
            ui.add_space(12.0);
            ui.vertical(|ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = CARD_GAP;
                        match self.settings_tab {
                            SettingsTab::Appearance => self.appearance_page(ui, palette),
                            SettingsTab::Downloads => self.downloads_page(ui, palette),
                            SettingsTab::Accounts => self.accounts_page(ui, palette),
                            SettingsTab::Updates => self.updates_page(ui, palette),
                        }
                    });
            });
        });
    }

    fn appearance_page(&mut self, ui: &mut egui::Ui, palette: Theme) {
        let fill = self.card_fill(palette.row);
        page_title(ui, "appearance", palette);
        // Swatches centred in the same box as the tint slider below.
        setting_card(
            ui,
            fill,
            palette,
            ("theme", "colour theme for every screen"),
            APPEARANCE_CONTROL_W,
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                let (swatch, gap) = (16.0, 10.0);
                ui.spacing_mut().item_spacing.x = gap;
                ui.add_space((APPEARANCE_CONTROL_W - (5.0 * swatch + 4.0 * gap)) / 2.0);
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
            },
        );
        #[cfg(target_os = "macos")]
        {
            let locked = self.viewport_fullscreen;
            let shown = update::effective_glass_tint(self.glass_tint, locked);
            let description = if locked {
                "locked at 100% in fullscreen"
            } else {
                "how strongly the theme colour sits on the window blur"
            };
            let mut persist = false;
            setting_card(
                ui,
                fill,
                palette,
                ("window tint", description),
                APPEARANCE_CONTROL_W,
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    glass_label(
                        ui,
                        format!("{}%", update::glass_tint_percent(shown)),
                        12.0,
                        palette.fg,
                    );
                    ui.allocate_ui(egui::vec2(ui.available_width(), 22.0), |ui| {
                        if locked {
                            let mut locked_tint = shown;
                            tint_slider(ui, &mut locked_tint, palette, true);
                        } else {
                            let response = tint_slider(ui, &mut self.glass_tint, palette, false);
                            if response.drag_stopped()
                                || (response.changed() && !response.dragged())
                            {
                                persist = true;
                            }
                        }
                    });
                },
            );
            if persist {
                let _ = update::set_glass_tint(self.glass_tint);
            }
        }
    }

    fn downloads_page(&mut self, ui: &mut egui::Ui, palette: Theme) {
        let fill = self.card_fill(palette.row);
        page_title(ui, "downloads", palette);
        setting_card(
            ui,
            fill,
            palette,
            (
                "connections per download",
                "parallel connections for new HTTP and debrid downloads; auto is 4, and each provider caps it",
            ),
            200.0,
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                // right-to-left: listed in reverse so they read auto → 16
                for parts in [16u8, 8, 4, 2, 1] {
                    let on = self.connections == Some(parts);
                    if route_chip(ui, &parts.to_string(), on, palette).clicked() && !on {
                        self.set_connections(Some(parts));
                    }
                }
                let auto = self.connections.is_none();
                if route_chip(ui, "auto", auto, palette).clicked() && !auto {
                    self.set_connections(None);
                }
            },
        );
        setting_card(
            ui,
            fill,
            palette,
            (
                "remove from debrid account after download",
                "once every file has arrived; only torrents torrenter added",
            ),
            SWITCH_CONTROL_W,
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                if toggle_switch(ui, self.debrid_remove_after, palette).clicked() {
                    self.debrid_remove_after = !self.debrid_remove_after;
                    self.session
                        .set_debrid_remove_after(self.debrid_remove_after);
                    let _ = update::set_debrid_remove_after(self.debrid_remove_after);
                }
            },
        );
    }

    fn updates_page(&mut self, ui: &mut egui::Ui, palette: Theme) {
        let fill = self.card_fill(palette.row);
        page_title(ui, "updates", palette);
        setting_card(
            ui,
            fill,
            palette,
            (
                "auto-update",
                "check GitHub Releases on launch and install newer versions (same switch as the header)",
            ),
            SWITCH_CONTROL_W,
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                if toggle_switch(ui, self.auto_update, palette).clicked() {
                    self.auto_update = !self.auto_update;
                    let _ = update::set_auto_update(self.auto_update);
                    if self.auto_update {
                        let _ = self.tx.send(WorkerCmd::Check);
                        self.toast_hold(CHECKING_UPDATES);
                    }
                }
            },
        );
    }

    /// One card per account (like a download row), "+ add account" beside
    /// the title, the add form as its own card. More providers or keys only
    /// add cards.
    fn accounts_page(&mut self, ui: &mut egui::Ui, palette: Theme) {
        let fill = self.card_fill(palette.row);
        let accounts = self.accounts();
        let mut open_form = false;
        ui.horizontal(|ui| {
            page_title(ui, "debrid accounts", palette);
            if self.add_form.is_none() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button(ink("+ add account", 12.0, palette.fg))
                        .on_hover_text("paste an API key from your provider's account page")
                        .clicked()
                    {
                        open_form = true;
                    }
                });
            }
        });
        if open_form {
            self.add_form = Some(AddAccountForm::default());
        }
        if self.add_form.is_some() {
            settings_card(ui, fill, palette, |ui| {
                self.add_account_form(ui, &accounts, palette);
            });
        }
        if accounts.is_empty() && self.add_form.is_none() {
            settings_card(ui, fill, palette, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(10.0);
                    glass_label(ui, "no accounts yet", 13.0, palette.fg);
                    glass_label(
                        ui,
                        "add one to download magnets through a debrid service instead of from peers",
                        12.0,
                        palette.dim,
                    );
                    ui.add_space(10.0);
                });
            });
        }
        for account in &accounts {
            settings_card(ui, fill, palette, |ui| {
                self.account_row(ui, account, palette)
            });
        }
        glass_label(
            ui,
            "uses your own account; the provider's terms apply",
            11.0,
            palette.dim,
        );
    }

    fn set_connections(&mut self, parts: Option<u8>) {
        self.connections = parts;
        self.session.set_connections(parts);
        let _ = update::set_connections(parts);
    }

    fn account_row(&mut self, ui: &mut egui::Ui, account: &AccountInfo, palette: Theme) {
        let provider = account.service.menu_label();
        let detail = match (self.account_notes.get(&account.id), account.source) {
            (Some(note), _) => format!("{provider} · {note}"),
            (None, KeySource::Env) => format!("{provider} · from {}", account.service.env_var()),
            (None, KeySource::File) => format!("{provider} · key saved"),
        };
        let mut test = false;
        let mut remove = false;
        setting_row(
            ui,
            palette,
            (&account.name, &detail),
            160.0,
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if account.source == KeySource::File
                    && ui
                        .button(ink("remove", 12.0, palette.fail))
                        .on_hover_text("forget this key on this computer")
                        .clicked()
                {
                    remove = true;
                }
                if ui
                    .button(ink("test", 12.0, palette.fg))
                    .on_hover_text("check the key and the account's plan")
                    .clicked()
                {
                    test = true;
                }
            },
        );
        if test {
            if let Some((service, key)) = credentials::account_key(&account.id) {
                self.start_account_check(account.id.clone(), service, key);
            }
        } else if remove {
            let removed = credentials::remove_account(&account.id);
            self.accounts_cache = None;
            match removed {
                Ok(()) => {
                    self.account_notes.remove(&account.id);
                    if self.debrid_route.as_deref() == Some(account.id.as_str()) {
                        self.set_debrid_route(None);
                    }
                    self.toast_info(format!("{} removed", account.name));
                }
                Err(error) => self.toast_fail(format!("{error:#}")),
            }
        }
    }

    /// Provider first, then a name (optional: defaults to the provider's
    /// name) and the key, as tile rows; cancel in the header, save at the
    /// bottom right. Saving runs the account check.
    fn add_account_form(&mut self, ui: &mut egui::Ui, accounts: &[AccountInfo], palette: Theme) {
        let Some(mut form) = self.add_form.take() else {
            return;
        };
        let mut keep = true;
        let mut save = false;
        // One width for the select and both fields, flush right: just wide
        // enough for the placeholder and the longest provider name.
        let field_w = Service::offered()
            .iter()
            .map(|service| route_select_width(ui, &service.menu_label()))
            .fold(route_select_width(ui, "choose a provider"), f32::max)
            + 8.0;
        // A typed name must be free (any provider, any case); checked as
        // you type, shown on the field, and save stays off meanwhile.
        let name_error = {
            let typed = form.name.trim();
            (!typed.is_empty()
                && accounts
                    .iter()
                    .any(|account| account.name.eq_ignore_ascii_case(typed)))
            .then(|| format!("\"{typed}\" is already one of your accounts"))
        };
        card_title(ui, "add account", palette);
        ui.label(ink(
            "paste an API key from your provider's account page; it is stored only on this computer",
            11.0,
            palette.dim,
        ));
        ui.add_space(4.0);
        // The fields sit in an inset panel the header owns.
        let inset =
            Color32::from_rgba_unmultiplied(palette.bg.r(), palette.bg.g(), palette.bg.b(), 170);
        egui::Frame::NONE
            .fill(inset)
            .corner_radius(6)
            .inner_margin(egui::Margin::symmetric(FORM_INSET, 6))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let provider_note = match form.service {
                    Some(service) if !service.verified() => {
                        "experimental: not yet tested with a real account; please report problems"
                    }
                    _ => "the debrid service the key belongs to",
                };
                setting_row(
                    ui,
                    palette,
                    ("provider", provider_note),
                    field_w,
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        let text = form
                            .service
                            .map(Service::menu_label)
                            .unwrap_or_else(|| "choose a provider".to_string());
                        let select = route_select(
                            ui,
                            &text,
                            field_w,
                            28.0,
                            form.service.is_some(),
                            true,
                            palette,
                        );
                        let popup_id = ui.make_persistent_id("add-account-provider");
                        if select.clicked() {
                            ui.memory_mut(|memory| memory.toggle_popup(popup_id));
                        }
                        solid_popup_below(ui, popup_id, &select, palette, |ui| {
                            // Exactly as wide as the select above it.
                            let margin = ui.style().spacing.menu_margin;
                            let inner = field_w - f32::from(margin.left) - f32::from(margin.right);
                            ui.set_width(inner);
                            for service in Service::offered() {
                                if menu_row_centered(
                                    ui,
                                    &service.menu_label(),
                                    None,
                                    form.service == Some(*service),
                                    palette,
                                )
                                .clicked()
                                {
                                    form.service = Some(*service);
                                }
                            }
                        });
                    },
                );
                if let Some(service) = form.service {
                    let default_name = credentials::default_name(service, accounts);
                    tile_divider(ui, palette);
                    let (name_note, name_tint) = match &name_error {
                        Some(error) => (error.as_str(), palette.fail),
                        None => ("shown in the download menu; optional", palette.dim),
                    };
                    setting_row_tinted(
                        ui,
                        palette,
                        ("name", name_note),
                        name_tint,
                        field_w,
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            // Placeholder centred like the select; typed
                            // text reads from the left.
                            let align = placeholder_align(&form.name);
                            let edit = TextEdit::singleline(&mut form.name)
                                .horizontal_align(align)
                                .hint_text(default_name.as_str())
                                .font(FontId::new(13.0, FontFamily::Monospace))
                                .desired_width(field_w - 16.0)
                                .margin(egui::Margin::symmetric(8, 5));
                            form_field_checked(ui, edit, palette, name_error.is_some());
                        },
                    );
                    tile_divider(ui, palette);
                    let description = format!("from your {} account settings", service.label());
                    setting_row(
                        ui,
                        palette,
                        ("api key", &description),
                        field_w,
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            let align = placeholder_align(&form.key);
                            let edit = TextEdit::singleline(&mut form.key)
                                .horizontal_align(align)
                                .password(true)
                                .hint_text("paste the key")
                                .font(FontId::new(13.0, FontFamily::Monospace))
                                .desired_width(field_w - 16.0)
                                .margin(egui::Margin::symmetric(8, 5));
                            let response = form_field(ui, edit, palette);
                            if response.lost_focus()
                                && ui.input(|input| input.key_pressed(egui::Key::Enter))
                            {
                                save = true;
                            }
                        },
                    );
                }
            });
        ui.add_space(2.0);
        // Both actions together, centred under the panel: cancel, then save.
        let width = ui.available_width();
        let font = FontId::new(12.0, FontFamily::Monospace);
        let pad = ui.spacing().button_padding.x * 2.0;
        let gap = 8.0;
        let text_w = |text: &str| {
            ui.fonts(|fonts| {
                fonts
                    .layout_no_wrap(text.to_string(), font.clone(), Color32::WHITE)
                    .size()
                    .x
            })
        };
        let group_w = text_w("cancel") + text_w("save account") + 2.0 * pad + gap;
        ui.allocate_ui_with_layout(
            egui::vec2(width, 28.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.spacing_mut().item_spacing.x = gap;
                ui.add_space(((width - group_w) / 2.0).max(0.0));
                if ui.button(ink("cancel", 12.0, palette.fail)).clicked() {
                    keep = false;
                }
                let ready =
                    form.service.is_some() && !form.key.trim().is_empty() && name_error.is_none();
                let mut primary = egui::Button::new(ink(
                    "save account",
                    12.0,
                    if ready { palette.fg } else { palette.dim },
                ));
                if ready {
                    primary = primary.stroke(Stroke::new(1.0_f32, palette.fg));
                }
                if ui.add_enabled(ready, primary).clicked() {
                    save = true;
                }
            },
        );
        if save && name_error.is_none() {
            if let (Some(service), Some(key)) = (form.service, Secret::new(form.key.trim())) {
                let added = credentials::add_account(service, &form.name, &key);
                self.accounts_cache = None;
                match added {
                    Ok(account) => {
                        self.toast_info(format!("{} saved", account.name));
                        self.start_account_check(account.id, service, key);
                        keep = false;
                    }
                    Err(error) => self.toast_fail(format!("{error:#}")),
                }
            }
        }
        if keep {
            self.add_form = Some(form);
        }
    }

    /// Clearing an unfinished debrid job torrenter added, or moving it off
    /// its service ("as torrent", another service): ask whether the torrent
    /// should also go from the service.
    fn show_remote_confirm(&mut self, ctx: &egui::Context, palette: Theme) {
        let Some(ask) = self.remote_confirm.clone() else {
            return;
        };
        let id = ask.id();
        let Some(job) = self.session.job(id) else {
            self.remote_confirm = None;
            return;
        };
        // The account's name, with its provider when the name differs.
        let label = match job.debrid.as_ref() {
            Some(debrid) => match self.account(&debrid.account) {
                Some(account) => account_title(&account.name, account.service),
                None => debrid.service.label().to_string(),
            },
            None => "the service".to_string(),
        };
        let mut open = true;
        let mut choice = None;
        let title = match ask {
            RemoteAsk::Clear(_) => "clear",
            RemoteAsk::Reroute { .. } => "restart",
        };
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .title_bar(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(
                egui::Frame::NONE
                    .fill(self.card_fill(palette.row))
                    .stroke(Stroke::new(1.0_f32, palette.dim))
                    .inner_margin(12.0)
                    .corner_radius(8),
            )
            .show(ctx, |ui| {
                ui.set_min_width(360.0);
                if popup_header(ui, title, palette) {
                    open = false;
                }
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
        let Some(remove_remote) = choice else {
            if !open {
                // Closed without an answer: neither clear nor re-route.
                self.remote_confirm = None;
            }
            return;
        };
        self.remote_confirm = None;
        match ask {
            RemoteAsk::Clear(_) if remove_remote => self.session.remove_with_remote(id),
            RemoteAsk::Clear(_) => self.session.remove(id),
            RemoteAsk::Reroute {
                route, seed, dest, ..
            } => {
                self.restart_routed(id, route, seed, dest, remove_remote);
            }
        }
        self.persist_jobs(true);
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
        let mut route = draft.route.clone();
        let dest = draft.dest.clone();
        let kind = draft.kind;
        let ready = self.accounts();
        egui::Window::new("restart")
            .collapsible(false)
            .resizable(false)
            .title_bar(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(
                egui::Frame::NONE
                    .fill(self.card_fill(palette.row))
                    .stroke(Stroke::new(1.0_f32, palette.dim))
                    .inner_margin(12.0)
                    .corner_radius(8),
            )
            .show(ctx, |ui| {
                ui.set_min_width(420.0);
                if popup_header(ui, "restart", palette) {
                    open = false;
                }
                if kind == Mode::Magnet {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        glass_label(ui, "route", 12.0, palette.dim);
                        if route_chip(ui, "peers", route.is_none(), palette).clicked() {
                            route = None;
                        }
                        for account in &ready {
                            let on = route.as_deref() == Some(account.id.as_str());
                            if route_chip(ui, &account.name, on, palette)
                                .on_hover_text(account.service.label())
                                .clicked()
                            {
                                route = Some(account.id.clone());
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
            let route = draft.route.clone();
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

/// Repaint cadence while something moves, and when idle.
const BUSY_REPAINT: Duration = Duration::from_millis(200);
const IDLE_REPAINT: Duration = Duration::from_secs(1);
/// Stay on the busy cadence this long after launch and after the last motion.
const BUSY_TAIL: Duration = Duration::from_secs(3);

/// Requests a repaint when dropped: a worker holds one while it works, so the
/// window wakes as soon as the reply is posted.
struct RepaintOnDrop(egui::Context);

impl Drop for RepaintOnDrop {
    fn drop(&mut self) {
        self.0.request_repaint();
    }
}

/// Settings layout: sidebar width and the gap between cards.
const SIDEBAR_W: f32 = 190.0;
/// Control boxes on the right of a setting tile: theme and tint share one so
/// they line up; a switch needs little.
const APPEARANCE_CONTROL_W: f32 = 260.0;
const SWITCH_CONTROL_W: f32 = 40.0;
/// How far the add-account fields sit inside its header.
const FORM_INSET: i8 = 12;
const CARD_GAP: f32 = 10.0;

/// The content pane's heading (the chosen category).
fn page_title(ui: &mut egui::Ui, text: &str, palette: Theme) {
    ui.add_space(2.0);
    glass_label(ui, text, 15.0, palette.fg);
}

/// One setting as a full-width tile: title and description on the left,
/// the control in a box of `control_w` on the right. Both are centred on the
/// tile's middle line; the tile is only as tall as its content.
fn setting_card(
    ui: &mut egui::Ui,
    fill: Color32,
    palette: Theme,
    text: (&str, &str),
    control_w: f32,
    layout: egui::Layout,
    control: impl FnOnce(&mut egui::Ui),
) {
    settings_card(ui, fill, palette, |ui| {
        setting_row(ui, palette, text, control_w, layout, control);
    });
}

/// A tile's content without the frame (rows inside one card share it).
fn setting_row(
    ui: &mut egui::Ui,
    palette: Theme,
    text: (&str, &str),
    control_w: f32,
    layout: egui::Layout,
    control: impl FnOnce(&mut egui::Ui),
) {
    setting_row_tinted(ui, palette, text, palette.dim, control_w, layout, control);
}

/// `setting_row` with the description in `tint` (an inline error uses
/// `palette.fail`).
fn setting_row_tinted(
    ui: &mut egui::Ui,
    palette: Theme,
    text: (&str, &str),
    tint: Color32,
    control_w: f32,
    layout: egui::Layout,
    control: impl FnOnce(&mut egui::Ui),
) {
    let (title, description) = text;
    let width = ui.available_width();
    let text_w = (width - control_w - 24.0).max(80.0);
    let painter = ui.painter();
    let title = painter.layout_no_wrap(
        title.to_string(),
        FontId::new(13.0, FontFamily::Monospace),
        palette.fg,
    );
    let description = painter.layout(
        description.to_string(),
        FontId::new(11.0, FontFamily::Monospace),
        tint,
        text_w,
    );
    let gap = 3.0;
    let text_h = title.size().y + gap + description.size().y;
    let row_h = text_h.max(30.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, row_h), Sense::hover());
    let top = rect.center().y - text_h / 2.0;
    let title_h = title.size().y;
    let painter = ui.painter();
    painter.galley(egui::pos2(rect.left(), top), title, palette.fg);
    painter.galley(
        egui::pos2(rect.left(), top + title_h + gap),
        description,
        tint,
    );
    let control_rect =
        egui::Rect::from_min_max(egui::pos2(rect.right() - control_w, rect.top()), rect.max);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(control_rect).layout(layout));
    control(&mut child);
}

/// A thin line between rows inside one card.
fn tile_divider(ui: &mut egui::Ui, palette: Theme) {
    let color =
        Color32::from_rgba_unmultiplied(palette.dim.r(), palette.dim.g(), palette.dim.b(), 50);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().line_segment(
        [rect.left_center(), rect.right_center()],
        Stroke::new(1.0_f32, color),
    );
}

/// A settings card: the same frame as a download row.
fn settings_card(
    ui: &mut egui::Ui,
    fill: Color32,
    palette: Theme,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    egui::Frame::NONE
        .fill(fill)
        .corner_radius(8)
        .stroke(Stroke::new(1.0_f32, palette.dim))
        .inner_margin(10.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 6.0;
            add_contents(ui);
        });
}

/// A card's title, styled like a download row's badge.
fn card_title(ui: &mut egui::Ui, text: &str, palette: Theme) {
    // Inside a card the fill already keeps text readable: no glass wash.
    ui.label(ink(text, 13.0, palette.fg));
}

/// Centre a field's placeholder (matching the dropdowns) but keep typed text
/// left-aligned, so the cursor does not jump and a long key shows its start.
fn placeholder_align(text: &str) -> egui::Align {
    if text.is_empty() {
        egui::Align::Center
    } else {
        egui::Align::Min
    }
}

/// A text field styled like the URL field (dim border, bright on hover/focus).
fn form_field(ui: &mut egui::Ui, edit: TextEdit<'_>, palette: Theme) -> egui::Response {
    form_field_checked(ui, edit, palette, false)
}

/// `form_field` with a fail-coloured border while `error` is set.
fn form_field_checked(
    ui: &mut egui::Ui,
    edit: TextEdit<'_>,
    palette: Theme,
    error: bool,
) -> egui::Response {
    ui.scope(|ui| {
        let (dim, lit) = if error {
            (
                Stroke::new(1.0_f32, palette.fail),
                Stroke::new(1.0_f32, palette.fail),
            )
        } else {
            (
                Stroke::new(1.0_f32, palette.dim),
                Stroke::new(1.0_f32, palette.fg),
            )
        };
        ui.visuals_mut().extreme_bg_color = palette.bg;
        ui.visuals_mut().widgets.inactive.bg_stroke = dim;
        ui.visuals_mut().widgets.hovered.bg_stroke = lit;
        ui.visuals_mut().widgets.active.bg_stroke = lit;
        ui.add(edit)
    })
    .inner
}

/// A bare on/off switch (no label), the same track as the header toggle.
fn toggle_switch(ui: &mut egui::Ui, on: bool, palette: Theme) -> egui::Response {
    let track_w = 34.0_f32;
    let track_h = 18.0_f32;
    let (track, response) = ui.allocate_exact_size(egui::vec2(track_w, track_h), Sense::click());
    let painter = ui.painter();
    let rounding = track_h / 2.0;
    if on {
        painter.rect_filled(track, rounding, palette.fg);
    }
    painter.rect_stroke(
        track,
        rounding,
        Stroke::new(1.0_f32, if on { palette.fg } else { palette.dim }),
        egui::StrokeKind::Inside,
    );
    let knob_r = 5.5_f32;
    let knob_x = if on {
        track.right() - 3.0 - knob_r
    } else {
        track.left() + 3.0 + knob_r
    };
    painter.circle_filled(
        egui::pos2(knob_x, track.center().y),
        knob_r,
        if on { palette.bg } else { palette.dim },
    );
    click_cursor(response)
}

/// A popup's own title row: the title, and a close cross with the pointer
/// cursor (egui's built-in title-bar close button cannot set one). True when
/// the cross was clicked.
fn popup_header(ui: &mut egui::Ui, title: &str, palette: Theme) -> bool {
    let mut close = false;
    ui.horizontal(|ui| {
        ui.label(ink(title, 13.0, palette.fg));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let (rect, response) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), Sense::click());
            let color = if response.hovered() {
                palette.fg
            } else {
                palette.dim
            };
            let cross = rect.shrink(3.5);
            let stroke = Stroke::new(1.4_f32, color);
            let painter = ui.painter();
            painter.line_segment([cross.left_top(), cross.right_bottom()], stroke);
            painter.line_segment([cross.right_top(), cross.left_bottom()], stroke);
            if click_cursor(response).on_hover_text("close").clicked() {
                close = true;
            }
        });
    });
    ui.add_space(6.0);
    close
}

/// What the route menu picked.
enum RoutePick {
    Route(Option<String>),
    Settings,
}

/// The input row's route select text: the account name, or "default (none)".
fn route_label(route: Option<&AccountInfo>) -> String {
    match route {
        Some(account) => account.name.clone(),
        None => "default (none)".to_string(),
    }
}

const ROUTE_PAD: f32 = 10.0;
const ROUTE_ARROW: f32 = 14.0;

fn route_select_width(ui: &egui::Ui, text: &str) -> f32 {
    let width = ui.fonts(|fonts| {
        fonts
            .layout_no_wrap(
                text.to_string(),
                FontId::new(13.0, FontFamily::Monospace),
                Color32::WHITE,
            )
            .size()
            .x
    });
    ROUTE_PAD + width + 8.0 + ROUTE_ARROW + ROUTE_PAD
}

/// A select styled like the URL field: the route text and a ▾ drawn as a
/// triangle. Bright when a provider is picked; faded outside MAGNET mode.
fn route_select(
    ui: &mut egui::Ui,
    text: &str,
    width: f32,
    height: f32,
    on: bool,
    enabled: bool,
    palette: Theme,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), Sense::click());
    let faded =
        Color32::from_rgba_unmultiplied(palette.dim.r(), palette.dim.g(), palette.dim.b(), 70);
    let hovered = enabled && response.hovered();
    let color = match (enabled, on || hovered) {
        (false, _) => faded,
        (true, true) => palette.fg,
        (true, false) => palette.dim,
    };
    let painter = ui.painter();
    painter.rect_stroke(
        rect,
        2.0,
        Stroke::new(1.0_f32, color),
        egui::StrokeKind::Inside,
    );
    let font = FontId::new(13.0, FontFamily::Monospace);
    let room = (width - ROUTE_PAD - 8.0 - ROUTE_ARROW - ROUTE_PAD).max(0.0);
    let shown = fit_text(ui, text, &font, room);
    let shortened = shown != text;
    let galley = painter.layout_no_wrap(shown, font, color);
    // Centred on the whole box, like the fields below it; a label too long
    // for that is centred in the space left of the arrow instead.
    let text_left = rect.left() + ROUTE_PAD;
    let arrow_left = rect.right() - ROUTE_PAD - ROUTE_ARROW;
    let centred = rect.center().x - galley.size().x / 2.0;
    let text_x = if centred >= text_left && centred + galley.size().x <= arrow_left - 4.0 {
        centred
    } else {
        text_left + ((room - galley.size().x) / 2.0).max(0.0)
    };
    let text_pos = egui::pos2(text_x, rect.center().y - galley.size().y / 2.0);
    painter.galley(text_pos, galley, color);
    let arrow = egui::pos2(
        rect.right() - ROUTE_PAD - ROUTE_ARROW / 2.0,
        rect.center().y,
    );
    painter.add(egui::Shape::convex_polygon(
        vec![
            arrow + egui::vec2(-4.5, -2.5),
            arrow + egui::vec2(4.5, -2.5),
            arrow + egui::vec2(0.0, 3.0),
        ],
        color,
        Stroke::NONE,
    ));
    let response = if enabled {
        click_cursor(response)
    } else {
        response
    };
    let hint = if !enabled {
        "debrid applies to magnet downloads".to_string()
    } else if on {
        "magnets download through your debrid account; click to change".to_string()
    } else {
        "magnets download from peers; click to use a debrid account".to_string()
    };
    response.on_hover_text(if shortened {
        format!("{text}\n{hint}")
    } else {
        hint
    })
}

/// `text`, or as much of it as fits in `width` followed by "…".
fn fit_text(ui: &egui::Ui, text: &str, font: &FontId, width: f32) -> String {
    let measure = |candidate: &str| {
        ui.fonts(|fonts| {
            fonts
                .layout_no_wrap(candidate.to_string(), font.clone(), Color32::WHITE)
                .size()
                .x
        })
    };
    if measure(text) <= width {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    (0..chars.len())
        .rev()
        .map(|keep| format!("{}…", chars[..keep].iter().collect::<String>().trim_end()))
        .find(|candidate| measure(candidate) <= width)
        .unwrap_or_else(|| "…".to_string())
}

/// A dropdown list under `select` with a solid fill, so lines behind it do
/// not show through the window's glass. Closes on any click.
fn solid_popup_below<R>(
    ui: &mut egui::Ui,
    popup_id: egui::Id,
    select: &egui::Response,
    palette: Theme,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> Option<R> {
    let mut solid = ui.new_child(egui::UiBuilder::new());
    let fill = Color32::from_rgb(palette.row.r(), palette.row.g(), palette.row.b());
    solid.visuals_mut().window_fill = fill;
    solid.visuals_mut().panel_fill = fill;
    egui::popup::popup_below_widget(
        &solid,
        popup_id,
        select,
        egui::PopupCloseBehavior::CloseOnClick,
        add_contents,
    )
}

/// A list row with its text centred (and an optional dim detail after it),
/// for a dropdown whose own label is centred.
fn menu_row_centered(
    ui: &mut egui::Ui,
    title: &str,
    detail: Option<&str>,
    selected: bool,
    palette: Theme,
) -> egui::Response {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 26.0), Sense::click());
    let painter = ui.painter();
    if response.hovered() {
        painter.rect_filled(rect, 3.0, palette.row_alt);
    }
    let color = if selected || response.hovered() {
        palette.fg
    } else {
        palette.dim
    };
    if selected {
        painter.circle_filled(
            egui::pos2(rect.left() + 9.0, rect.center().y),
            3.0,
            palette.fg,
        );
    }
    let title = painter.layout_no_wrap(
        title.to_string(),
        FontId::new(13.0, FontFamily::Monospace),
        color,
    );
    let detail = detail.map(|text| {
        painter.layout_no_wrap(
            format!("  {text}"),
            FontId::new(11.0, FontFamily::Monospace),
            palette.dim,
        )
    });
    let total = title.size().x + detail.as_ref().map_or(0.0, |d| d.size().x);
    let left = rect.center().x - total / 2.0;
    let title_w = title.size().x;
    painter.galley(
        egui::pos2(left, rect.center().y - title.size().y / 2.0),
        title,
        color,
    );
    if let Some(detail) = detail {
        let y = rect.center().y - detail.size().y / 2.0;
        painter.galley(egui::pos2(left + title_w, y), detail, palette.dim);
    }
    click_cursor(response)
}

/// Space a list row keeps on both sides: the current pick's ● and its gap.
const MENU_INSET: f32 = 20.0;

/// One route menu row: a marker for the current pick, the title, and a dim
/// detail at the far right.
fn menu_row(
    ui: &mut egui::Ui,
    title: &str,
    detail: &str,
    selected: bool,
    palette: Theme,
) -> egui::Response {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 26.0), Sense::click());
    let painter = ui.painter();
    if response.hovered() {
        painter.rect_filled(rect, 3.0, palette.row_alt);
    }
    let color = if selected || response.hovered() {
        palette.fg
    } else {
        palette.dim
    };
    if selected {
        painter.circle_filled(
            egui::pos2(rect.left() + 9.0, rect.center().y),
            3.0,
            palette.fg,
        );
    }
    let font = FontId::new(13.0, FontFamily::Monospace);
    let title = painter.layout_no_wrap(title.to_string(), font, color);
    painter.galley(
        egui::pos2(
            rect.left() + MENU_INSET,
            rect.center().y - title.size().y / 2.0,
        ),
        title,
        color,
    );
    if !detail.is_empty() {
        let detail = painter.layout_no_wrap(
            detail.to_string(),
            FontId::new(11.0, FontFamily::Monospace),
            palette.dim,
        );
        painter.galley(
            egui::pos2(
                rect.right() - MENU_INSET - detail.size().x,
                rect.center().y - detail.size().y / 2.0,
            ),
            detail,
            palette.dim,
        );
    }
    click_cursor(response)
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
        ui.allocate_exact_size(egui::vec2(inner_w, inner_h) + pad, Sense::click());
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

/// Room the file glyph takes at the right end of the URL box (beyond the
/// usual 8 px text margin), so typed text stops short of it.
const FILE_GLYPH_ROOM: f32 = 16.0;
/// The page the file glyph draws: about the height of the URL text.
const FILE_PAGE: egui::Vec2 = egui::vec2(8.5, 12.0);

/// Inside the URL box, at its right end (MAGNET only): open a `.torrent` file.
fn upload_glyph(ui: &mut egui::Ui, palette: Theme, field: egui::Rect) -> egui::Response {
    // The page's right edge sits 8 px inside the border.
    let center = egui::pos2(field.right() - 8.0 - FILE_PAGE.x / 2.0, field.center().y);
    let hit = egui::Rect::from_center_size(center, egui::vec2(18.0, 18.0));
    let response = ui.interact(hit, ui.id().with("torrent-file-glyph"), Sense::click());
    let color = if response.hovered() {
        palette.fg
    } else {
        palette.dim
    };
    paint_file(ui.painter(), center, color);
    click_cursor(response).on_hover_text("open a .torrent file (or drop one on the window)")
}

/// A page with a folded corner, centred on `center`: open a file (unlike the
/// download-folder glyph on the row below).
fn paint_file(painter: &egui::Painter, center: egui::Pos2, color: Color32) {
    let stroke = Stroke::new(1.2_f32, color);
    let page = egui::Rect::from_center_size(center, FILE_PAGE);
    let fold = 3.0_f32;
    let (left, right, top, bottom) = (page.left(), page.right(), page.top(), page.bottom());
    let p = egui::pos2;
    painter.add(egui::Shape::closed_line(
        vec![
            p(left, top),
            p(right - fold, top),
            p(right, top + fold),
            p(right, bottom),
            p(left, bottom),
        ],
        stroke,
    ));
    painter.add(egui::Shape::line(
        vec![
            p(right - fold, top),
            p(right - fold, top + fold),
            p(right, top + fold),
        ],
        stroke,
    ));
}

/// While files are dragged over the window: say what a drop does.
fn paint_drop_hint(ctx: &egui::Context, palette: Theme) {
    if ctx.input(|input| input.raw.hovered_files.is_empty()) {
        return;
    }
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("torrent-drop"),
    ));
    let screen = ctx.screen_rect();
    painter.rect_filled(
        screen,
        0.0,
        Color32::from_rgba_unmultiplied(palette.bg.r(), palette.bg.g(), palette.bg.b(), 210),
    );
    painter.rect_stroke(
        screen.shrink(12.0),
        6.0,
        Stroke::new(1.5_f32, palette.fg),
        egui::StrokeKind::Inside,
    );
    painter.text(
        screen.center(),
        egui::Align2::CENTER_CENTER,
        "drop .torrent to start",
        FontId::new(18.0, FontFamily::Monospace),
        palette.fg,
    );
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

fn worker_loop(rx: Receiver<WorkerCmd>, tx: Sender<WorkerEvent>, ctx: egui::Context) {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let handle = runtime.handle().clone();
    let _keep_alive = thread::spawn(move || {
        runtime.block_on(std::future::pending::<()>());
    });
    while let Ok(cmd) = rx.recv() {
        let tx = tx.clone();
        let ctx = ctx.clone();
        handle.spawn(async move {
            // Wake the window when the reply is posted (idle repaints are slow).
            let _wake = RepaintOnDrop(ctx);
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
            private_torrent: false,
        }
    }

    #[test]
    fn restoring_placeholder_total_is_indeterminate() {
        assert!(job_bar_fraction("starting", 25, 100).is_none());
        assert!(
            job_bar_fraction("Initializing torrent client & verifying data...", 25, 100).is_none()
        );
        assert_eq!(job_bar_fraction("verifying", 25, 100), Some(0.25));
        assert!(job_bar_fraction("fetching metadata", 0, 0).is_none());
        assert!(job_bar_fraction("connecting to peers", 0, 100).is_none());
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
