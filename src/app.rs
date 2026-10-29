// SPDX-License-Identifier: GPL-3.0-or-later
//! The GUI: start screen, scan progress and the treemap view.

mod panel;
mod prefs;
mod tools;

use crate::filter::Filter;
use crate::layout::{self, Item, ItemKind, Params};
use crate::scan::{self, Kind, Progress, ScanOutput, F_DENIED, F_HARDLINK_DUP, F_MOUNT, F_SNAPSHOTS};
use crate::theme::{self, *};
use crate::tree::{DateKind, SizeMode, Tree, Values, NONE};
use crate::util::{self, fmt_count, fmt_size, Mount};
use eframe::egui::{
    self, epaint::RectShape, epaint::TessellationOptions, epaint::Tessellator, pos2, vec2, Align, Align2, Color32,
    CornerRadius, FontId, Galley, Key, Layout, Margin, Mesh, PointerButton, Pos2, Rect, Response, RichText, Sense,
    Shape, Stroke, StrokeKind, Ui,
};
use egui_phosphor::regular as icon;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{self, Receiver};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const HEADER_H: f32 = 21.0;
const DEPTH_UNLIMITED: u32 = 16;
const DETAIL_LEVELS: [(f32, &str); 5] = [
    (36.0, "Coarse"),
    (20.0, "Low"),
    (11.0, "Medium"),
    (6.0, "High"),
    (3.0, "Max"),
];

// ------------------------------------------------------------------ jobs

struct ScanJob {
    path: PathBuf,
    /// None = full scan, Some(id) = rescan of that folder
    target: Option<u32>,
    progress: Arc<Progress>,
    rx: Receiver<ScanOutput>,
    started: Instant,
    elevated: bool,
    /// used bytes of the filesystem, when scanning a whole filesystem
    fs_used: Option<u64>,
}

fn start_job(path: PathBuf, cross: bool, elevated: bool, target: Option<u32>, ctx: &egui::Context) -> ScanJob {
    let progress = Arc::new(Progress::default());
    let (tx, rx) = mpsc::channel();
    let p2 = progress.clone();
    let path2 = path.clone();
    let ctx2 = ctx.clone();
    std::thread::Builder::new()
        .name("scan".into())
        .spawn(move || {
            let out = if elevated { scan::scan_elevated(&path2, cross, p2) } else { scan::scan(&path2, cross, p2) };
            let _ = tx.send(out);
            ctx2.request_repaint();
        })
        .expect("spawn scan thread");
    let is_mount_root = util::mount_of(&util::read_mounts(), &path).map(|m| m.point == path).unwrap_or(false);
    let fs_used = if is_mount_root { util::fs_space(&path).map(|s| s.used) } else { None };
    ScanJob { path, target, progress, rx, started: Instant::now(), elevated, fs_used }
}

// ------------------------------------------------------------------ settings

#[derive(Clone, Copy, PartialEq, Eq)]
enum ColorMode {
    Sections,
    Mono,
    ByType,
    ByAge,
}

struct Settings {
    size_mode: SizeMode,
    depth: u32,
    detail: usize,
    show_free: bool,
    show_unknown: bool,
    color: ColorMode,
    /// which date the age colors use
    age_date: DateKind,
    cross_fs: bool,
    full_access: bool,
    /// macOS: show the Full Disk Access window at launch when access is missing
    ask_full_disk_access: bool,
}

// ------------------------------------------------------------------ cache

#[derive(PartialEq, Clone, Copy)]
struct CacheKey {
    root: u32,
    rect: [i32; 4],
    depth: u32,
    detail: usize,
    color: ColorMode,
    age_date: DateKind,
    ver: u64,
    expanded: u64,
    ppp: u32,
}

struct Label {
    pos: Pos2,
    galley: Arc<Galley>,
}

struct LayoutCache {
    key: CacheKey,
    items: Vec<Item>,
    mesh: Arc<Mesh>,
    labels: Vec<Label>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Target {
    Node(u32),
    /// group of small items inside folder
    Group(u32, u32),
}

impl Target {
    fn node(self) -> u32 {
        match self {
            Target::Node(n) | Target::Group(n, _) => n,
        }
    }
}

// ------------------------------------------------------------------ app

pub struct App {
    tree: Option<Tree>,
    /// "Drives" was clicked; go back to the start screen at the next frame
    /// (the rest of the current frame still draws the old scan)
    to_drives: bool,
    /// folders opened one level further by a click (below the level limit);
    /// reset when the view changes
    expanded: HashSet<u32>,
    expanded_root: u32,
    expanded_ver: u64,
    /// what the first click of a double-click hit
    last_click: Option<Target>,
    vals: Values,
    ver: u64,
    job: Option<ScanJob>,
    last_scan: Option<(Duration, u64, bool)>,
    /// the current tree was scanned with full access
    scanned_elevated: bool,
    error: Option<String>,

    view_root: u32,
    back: Vec<u32>,
    fwd: Vec<u32>,
    selected: Option<Target>,
    menu_target: Option<Target>,
    scroll_acc: f32,

    settings: Settings,
    filter_text: String,
    filter_applied: String,
    filter_edited: Option<Instant>,
    /// how long the last filter recalculation took
    filter_cost: Duration,
    filter: Filter,

    path_input: String,
    mounts: Vec<Mount>,
    /// filesystem sizes, filled in by background threads (statvfs on an
    /// unreachable network share can block for a long time)
    mount_space: Arc<Mutex<HashMap<PathBuf, util::Space>>>,
    denied: usize,
    last_title: String,
    toast: Option<(String, Instant)>,
    cache: Option<LayoutCache>,
    panel: panel::PanelState,
    dupes: tools::DupState,
    changes: tools::ChangesState,
    /// unfiltered on-disk sizes, for comparing with earlier scans
    history_vals: Values,
    saved_prefs: String,
    logo: egui::TextureHandle,
    menu_from_list: bool,
    list_menu_response: Option<Response>,
    /// macOS: whether Full Disk Access was granted (checked at launch)
    full_disk_access: bool,
    /// macOS: the Full Disk Access window is open
    access_prompt: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, start: Option<PathBuf>) -> Self {
        theme::install_fonts(&cc.egui_ctx);
        theme::apply_style(&cc.egui_ctx);
        let mut app = App {
            tree: None,
            to_drives: false,
            expanded: HashSet::new(),
            expanded_root: NONE,
            expanded_ver: 0,
            last_click: None,
            vals: Values { size: Vec::new(), files: Vec::new() },
            ver: 0,
            job: None,
            last_scan: None,
            scanned_elevated: false,
            error: None,
            view_root: 0,
            back: Vec::new(),
            fwd: Vec::new(),
            selected: None,
            menu_target: None,
            scroll_acc: 0.0,
            settings: Settings {
                size_mode: SizeMode::OnDisk,
                depth: DEPTH_UNLIMITED,
                detail: 2,
                show_free: true,
                show_unknown: true,
                color: ColorMode::Sections,
                age_date: DateKind::Added,
                cross_fs: false,
                full_access: false,
                ask_full_disk_access: true,
            },
            filter_text: String::new(),
            filter_applied: String::new(),
            filter_edited: None,
            filter_cost: Duration::ZERO,
            filter: Filter::default(),
            path_input: util::home_dir().to_string_lossy().into_owned(),
            mounts: Vec::new(),
            mount_space: Arc::new(Mutex::new(HashMap::new())),
            denied: 0,
            last_title: String::new(),
            toast: None,
            cache: None,
            panel: panel::PanelState::default(),
            dupes: tools::DupState::default(),
            changes: tools::ChangesState::default(),
            history_vals: Values { size: Vec::new(), files: Vec::new() },
            saved_prefs: String::new(),
            logo: cc.egui_ctx.load_texture(
                "logo",
                egui::ColorImage::from_rgba_unmultiplied([128, 128], crate::ICON_RGBA),
                egui::TextureOptions::LINEAR,
            ),
            menu_from_list: false,
            list_menu_response: None,
            full_disk_access: true,
            access_prompt: false,
        };
        app.load_prefs();
        app.full_disk_access = util::has_full_disk_access();
        app.access_prompt = !app.full_disk_access && app.settings.ask_full_disk_access;
        app.refresh_mounts(&cc.egui_ctx);
        if let Some(p) = start {
            app.path_input = p.to_string_lossy().into_owned();
            app.begin_scan(p, &cc.egui_ctx);
        }
        app
    }

    fn refresh_mounts(&mut self, ctx: &egui::Context) {
        self.mounts = list_mounts();
        for m in &self.mounts {
            let point = m.point.clone();
            let map = self.mount_space.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                if let Some(s) = util::fs_space(&point) {
                    if let Ok(mut m) = map.lock() {
                        m.insert(point, s);
                    }
                    ctx.request_repaint();
                }
            });
        }
    }

    /// Whether scans run with administrator rights. On Linux each full-access
    /// scan asks for the password through pkexec; on Windows the whole program
    /// runs as administrator after "Restart as administrator". macOS asks
    /// through sudo like pkexec; protected folders also need Full Disk Access.
    fn elevated(&self) -> bool {
        if cfg!(windows) { util::is_admin() } else { self.settings.full_access }
    }

    /// Windows: start an administrator copy (UAC prompt) and close this one.
    fn restart_as_admin(&mut self, path: Option<PathBuf>, ctx: &egui::Context) {
        if util::relaunch_as_admin(path.as_deref()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else {
            self.toast("Could not restart as administrator".to_string());
        }
    }

    /// macOS: explain Full Disk Access once per launch while it is missing.
    /// macOS has no system prompt for it; the user switches it on in
    /// System Settings, where Spacemapper.app can be dragged into the list.
    fn access_window(&mut self, ctx: &egui::Context) {
        let mut close = false;
        let r = egui::Modal::new(egui::Id::new("full_disk_access")).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.spacing_mut().item_spacing.y = 6.0;
            ui.label(RichText::new(format!("{}  Allow Full Disk Access", icon::SHIELD_CHECK)).color(TEXT).font(font_bold(16.0)));
            ui.label(
                RichText::new(
                    "To show everything on your disk, including Mail, Messages and other protected folders, \
                     Spacemapper needs Full Disk Access. Spacemapper only reads files; it does not modify or delete them.",
                )
                .size(12.5)
                .color(TEXT_DIM),
            );
            ui.label(
                RichText::new("In System Settings, switch on Spacemapper. If it is not in the list, drag Spacemapper.app from Finder into it. Then restart Spacemapper.")
                    .size(12.5)
                    .color(TEXT_DIM),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let open = egui::Button::new(RichText::new("Open System Settings").size(12.5).color(ACCENT))
                    .fill(SURFACE2)
                    .corner_radius(CornerRadius::same(8));
                if ui.add(open).clicked() {
                    util::open_full_disk_access_settings();
                    close = true;
                }
                if ui.button("Show Spacemapper.app in Finder").clicked() {
                    util::reveal_app();
                }
                if ui.button("Not now").clicked() {
                    close = true;
                }
                if ui.button("Don't ask again").clicked() {
                    self.settings.ask_full_disk_access = false;
                    close = true;
                }
            });
        });
        if close || r.should_close() {
            self.access_prompt = false;
        }
    }

    fn begin_scan(&mut self, path: PathBuf, ctx: &egui::Context) {
        if let Some(job) = &self.job {
            job.progress.cancel.store(true, Relaxed);
        }
        self.error = None;
        self.job = Some(start_job(path, self.settings.cross_fs, self.elevated(), None, ctx));
    }

    fn rescan_folder(&mut self, id: u32, ctx: &egui::Context) {
        if self.job.is_some() {
            return;
        }
        let Some(tree) = &self.tree else { return };
        if id == tree.root {
            let p = tree.root_path.clone();
            self.begin_scan(p, ctx);
            return;
        }
        let path = tree.path(id);
        self.job = Some(start_job(path, self.settings.cross_fs, self.elevated(), Some(id), ctx));
    }

    fn poll_job(&mut self) {
        let Some(job) = &self.job else { return };
        let Ok(out) = job.rx.try_recv() else { return };
        let job = self.job.take().unwrap();
        let elapsed = job.started.elapsed();
        if let Some(err) = out.error {
            if job.target.is_none() {
                self.error = Some(err.clone());
            }
            self.toast(err);
            return;
        }
        let Some(root) = out.root else { return };
        self.reset_expanded();
        match job.target {
            None => {
                let tree = Tree::from_scan(root, &job.path);
                self.view_root = tree.root;
                self.tree = Some(tree);
                self.back.clear();
                self.fwd.clear();
                self.selected = None;
                self.last_scan = Some((elapsed, job.progress.files.load(Relaxed), out.cancelled));
                self.scanned_elevated = job.elevated;
                self.dupes = tools::DupState { min_idx: self.dupes.min_idx, ..Default::default() };
                self.update_history_vals();
                if !out.cancelled {
                    self.record_history();
                } else {
                    self.changes = tools::ChangesState::default();
                }
            }
            Some(target) => {
                let Some(tree) = &mut self.tree else { return };
                let new = tree.replace_subtree(target, root);
                if tree.is_within(self.view_root, target) {
                    self.view_root = new;
                }
                self.back.clear();
                self.fwd.clear();
                self.selected = Some(Target::Node(new));
                let name = tree.nodes[new as usize].name.to_string();
                self.toast(format!("Rescanned {name} in {}", util::fmt_duration(elapsed)));
                self.update_history_vals();
            }
        }
        self.recompute();
    }

    fn update_history_vals(&mut self) {
        if let Some(tree) = &self.tree {
            self.history_vals = tree.compute_values(SizeMode::OnDisk, &Filter::default(), false, false);
        }
    }

    fn recompute(&mut self) {
        if let Some(tree) = &self.tree {
            self.denied = tree.denied_count();
            self.vals = tree.compute_values(
                self.settings.size_mode,
                &self.filter,
                self.settings.show_free,
                self.settings.show_unknown,
            );
            self.ver += 1;
        }
    }

    fn apply_filter(&mut self) {
        let t0 = Instant::now();
        self.filter = Filter::parse(&self.filter_text, util::now_secs());
        self.filter_applied = self.filter_text.clone();
        self.filter_edited = None;
        self.recompute();
        self.filter_cost = t0.elapsed();
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    // ---------------------------------------------------------- navigation

    fn go(&mut self, id: u32) {
        if id != self.view_root {
            self.back.push(self.view_root);
            self.fwd.clear();
            self.view_root = id;
        }
    }
    fn go_back(&mut self) {
        if let Some(id) = self.back.pop() {
            self.fwd.push(self.view_root);
            self.view_root = id;
        }
    }
    fn go_fwd(&mut self) {
        if let Some(id) = self.fwd.pop() {
            self.back.push(self.view_root);
            self.view_root = id;
        }
    }
    fn go_up(&mut self) {
        if let Some(tree) = &self.tree {
            let p = tree.nodes[self.view_root as usize].parent;
            if p != NONE {
                self.go(p);
            }
        }
    }
    fn go_home(&mut self) {
        if let Some(tree) = &self.tree {
            let r = tree.root;
            self.go(r);
        }
    }
    fn can_go_up(&self) -> bool {
        self.tree.as_ref().map(|t| t.nodes[self.view_root as usize].parent != NONE).unwrap_or(false)
    }

    /// Folder to zoom into when the user activates `t`.
    fn zoom_target(&self, t: Target) -> Option<u32> {
        let tree = self.tree.as_ref()?;
        match t {
            Target::Group(dir, _) => Some(dir),
            Target::Node(n) => {
                let node = &tree.nodes[n as usize];
                if node.is_dir() {
                    Some(n)
                } else {
                    let p = node.parent;
                    (p != NONE).then_some(p)
                }
            }
        }
    }

    /// One level deeper from the current view towards `t` (mouse wheel).
    fn step_towards(&self, t: Target) -> Option<u32> {
        let tree = self.tree.as_ref()?;
        let chain = tree.chain(t.node());
        let pos = chain.iter().position(|&c| c == self.view_root)?;
        let next = *chain.get(pos + 1)?;
        tree.nodes[next as usize].is_dir().then_some(next)
    }

    fn more_detail(&mut self) {
        self.settings.detail = (self.settings.detail + 1).min(DETAIL_LEVELS.len() - 1);
    }
    fn less_detail(&mut self) {
        self.settings.detail = self.settings.detail.saturating_sub(1);
    }
    fn more_depth(&mut self) {
        self.settings.depth = (self.settings.depth + 1).min(DEPTH_UNLIMITED);
    }
    fn less_depth(&mut self) {
        if self.settings.depth >= DEPTH_UNLIMITED {
            // from "all" step down to something visible
            self.settings.depth = 6;
        } else {
            self.settings.depth = self.settings.depth.saturating_sub(1).max(1);
        }
    }

    // ---------------------------------------------------------- input

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let typing = ctx.wants_keyboard_input();
        let (back, fwd, up, home, plus, minus, enter, f5, ctrl_f, toggle_list, dplus, dminus) = ctx.input(|i| {
            let m = i.modifiers;
            (
                i.pointer.button_pressed(PointerButton::Extra1)
                    || (!typing && i.key_pressed(Key::Backspace))
                    || (m.alt && i.key_pressed(Key::ArrowLeft)),
                i.pointer.button_pressed(PointerButton::Extra2) || (m.alt && i.key_pressed(Key::ArrowRight)),
                (m.alt && i.key_pressed(Key::ArrowUp)) || (!typing && i.key_pressed(Key::U)),
                !typing && i.key_pressed(Key::Home),
                !typing && !m.command && (i.key_pressed(Key::Plus) || i.key_pressed(Key::Equals)),
                !typing && !m.command && i.key_pressed(Key::Minus),
                !typing && i.key_pressed(Key::Enter),
                i.key_pressed(Key::F5),
                m.command && i.key_pressed(Key::F),
                !typing && i.key_pressed(Key::L),
                !typing && i.key_pressed(Key::CloseBracket),
                !typing && i.key_pressed(Key::OpenBracket),
            )
        });
        if self.tree.is_none() {
            return;
        }
        if back {
            self.go_back();
        }
        if fwd {
            self.go_fwd();
        }
        if up {
            self.go_up();
        }
        if home {
            self.go_home();
        }
        if plus {
            self.more_depth();
        }
        if minus {
            self.less_depth();
        }
        if toggle_list {
            self.panel.open = !self.panel.open;
        }
        if dplus {
            self.more_detail();
        }
        if dminus {
            self.less_detail();
        }
        if enter {
            if let Some(t) = self.selected {
                if let Some(z) = self.zoom_target(t) {
                    self.go(z);
                }
            }
        }
        if f5 {
            let id = self.view_root;
            self.rescan_folder(id, ctx);
        }
        if ctrl_f {
            ctx.memory_mut(|m| m.request_focus(egui::Id::new("filter_box")));
        }
    }

    fn handle_drops(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> =
            ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        if let Some(p) = dropped.into_iter().next() {
            // resolve links like a typed path (/Volumes/Macintosh HD is /)
            let p = util::absolute(&p);
            let dir = if p.is_dir() { p } else { p.parent().map(Path::to_path_buf).unwrap_or(p) };
            self.path_input = dir.to_string_lossy().into_owned();
            self.begin_scan(dir, ctx);
        }
    }
}

fn list_mounts() -> Vec<Mount> {
    let mut v: Vec<Mount> = util::read_mounts().into_iter().filter(util::is_listable_fs).collect();
    let mut seen = std::collections::HashSet::new();
    v.reverse();
    v.retain(|m| seen.insert(m.point.clone()));
    v.reverse();
    v.sort_by(|a, b| a.point.cmp(&b.point));
    v
}

// ------------------------------------------------------------------ colours

fn ext_hue(name: &str) -> Option<f32> {
    let ext = name.rsplit_once('.').map(|(_, e)| e)?;
    if ext.is_empty() || ext.len() > 8 {
        return None;
    }
    let mut h: u32 = 2166136261;
    for b in ext.to_ascii_lowercase().bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    Some((h % 360) as f32)
}

/// green = changed recently, red = untouched for years
fn age_color(mtime: i64, now: i64) -> Color32 {
    let days = ((now - mtime).max(0) as f32) / 86400.0;
    let t = ((days + 1.0).ln() / (1100.0f32).ln()).clamp(0.0, 1.0);
    hsl(130.0 * (1.0 - t), 0.50, 0.62)
}

struct Fills {
    body: Color32,
    header: Color32,
}

fn level(depth: u16) -> f32 {
    (depth.saturating_sub(1)).min(5) as f32
}

fn item_fills(item: &Item, tree: &Tree, mode: ColorMode, age: DateKind, now: i64) -> Fills {
    let (h, s0) = SECTIONS[item.section as usize % SECTIONS.len()];
    let s = if mode == ColorMode::Sections { s0 } else { 0.0 };
    let lv = level(item.depth);
    let same = |c| Fills { body: c, header: c };
    match item.kind {
        ItemKind::DirOpen if item.depth == 0 => Fills { body: SURFACE, header: SURFACE2 },
        ItemKind::DirOpen => {
            let l = 0.17 + 0.045 * lv;
            Fills { body: hsl(h, s * 0.40, l), header: hsl(h, s * 0.48, l + 0.075) }
        }
        ItemKind::DirClosed => {
            if mode == ColorMode::ByAge {
                same(age_color(tree.nodes[item.node as usize].date(age), now))
            } else {
                same(hsl(h, s * 0.45, 0.34 + 0.035 * lv))
            }
        }
        ItemKind::Group => same(hsl(h, s * 0.18, 0.27 + 0.035 * lv)),
        ItemKind::Free => same(Color32::from_rgb(26, 30, 30)),
        ItemKind::Unknown => same(Color32::from_rgb(58, 58, 66)),
        ItemKind::File => {
            let n = &tree.nodes[item.node as usize];
            let l = if item.depth % 2 == 0 { 0.70 } else { 0.64 };
            let c = match mode {
                ColorMode::Sections => {
                    if n.kind == Kind::File {
                        hsl(h, s0 * 0.55, l)
                    } else {
                        hsl(h, s0 * 0.25, l - 0.08)
                    }
                }
                ColorMode::Mono => hsl(0.0, 0.0, l),
                ColorMode::ByType => match ext_hue(&n.name) {
                    Some(hh) => hsl(hh, 0.50, 0.64),
                    None => hsl(0.0, 0.0, 0.58),
                },
                ColorMode::ByAge => age_color(n.date(age), now),
            };
            same(c)
        }
    }
}

fn radius_for(r: Rect) -> u8 {
    (r.width().min(r.height()) * 0.22).clamp(0.0, 6.0) as u8
}

/// Lay out `text` no wider than `max_w`, shortening with "…" if needed.
fn fit_text(painter: &egui::Painter, text: &str, font: &FontId, color: Color32, max_w: f32) -> Option<Arc<Galley>> {
    if max_w < 10.0 {
        return None;
    }
    let g = painter.layout_no_wrap(text.to_owned(), font.clone(), color);
    if g.size().x <= max_w {
        return Some(g);
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (1usize, chars.len());
    let mut best = None;
    while lo < hi {
        let mid = (lo + hi) / 2;
        let s: String = chars[..mid].iter().collect::<String>() + "…";
        let g = painter.layout_no_wrap(s, font.clone(), color);
        if g.size().x <= max_w {
            best = Some(g);
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    best
}

// ------------------------------------------------------------------ treemap

impl App {
    fn build_cache(&self, key: CacheKey, rect: Rect, painter: &egui::Painter, ppp: f32) -> LayoutCache {
        let tree = self.tree.as_ref().unwrap();
        let params = Params {
            max_depth: self.max_depth(),
            expanded: &self.expanded,
            min_box: DETAIL_LEVELS[self.settings.detail].0,
            header_h: HEADER_H,
            pad: 4.0,
            gap: 3.0,
        };
        let items = layout::layout(tree, &self.vals, self.view_root, rect, &params);
        let now = util::now_secs();
        let mut shapes: Vec<Shape> = Vec::with_capacity(items.len() * 2);
        let mut labels = Vec::new();
        let root_total = self.vals.size[self.view_root as usize].max(1);

        for item in &items {
            let r = item.rect;
            if r.width() < 1.0 || r.height() < 1.0 {
                continue;
            }
            let fills = item_fills(item, tree, self.settings.color, self.settings.age_date, now);
            let rad = radius_for(r);
            shapes.push(Shape::Rect(RectShape::filled(r, CornerRadius::same(rad), fills.body)));
            if item.header {
                let hr = Rect::from_min_max(r.min, pos2(r.max.x, r.min.y + HEADER_H));
                let cr = CornerRadius { nw: rad, ne: rad, sw: 0, se: 0 };
                shapes.push(Shape::Rect(RectShape::filled(hr, cr, fills.header)));
            }
            // subtle top highlight on solid tiles gives a soft, modern depth
            if !item.header && r.height() > 10.0 && r.width() > 10.0 && item.kind != ItemKind::Free {
                let hl = Rect::from_min_max(r.min, pos2(r.max.x, r.min.y + 1.0));
                shapes.push(Shape::Rect(RectShape::filled(
                    hl.shrink2(vec2(rad as f32, 0.0)),
                    CornerRadius::ZERO,
                    Color32::from_white_alpha(22),
                )));
            }
            if item.kind == ItemKind::Free {
                shapes.push(Shape::Rect(RectShape::stroke(
                    r,
                    CornerRadius::same(rad),
                    Stroke::new(1.0_f32, Color32::from_rgb(60, 72, 70)),
                    StrokeKind::Inside,
                )));
            }

            // ---- labels
            let node = &tree.nodes[item.node as usize];
            let is_root = item.node == self.view_root && item.kind != ItemKind::Group;
            let name: String = match item.kind {
                ItemKind::Group => format!("{} small items", fmt_count(item.count as u64)),
                ItemKind::Free => "Free space".into(),
                ItemKind::Unknown => "Unknown".into(),
                _ if is_root => tree.path(item.node).to_string_lossy().into_owned(),
                _ if item.chain != item.node => {
                    let mut parts = Vec::new();
                    let mut c = item.node;
                    loop {
                        parts.push(&*tree.nodes[c as usize].name);
                        if c == item.chain {
                            break;
                        }
                        c = tree.nodes[c as usize].parent;
                    }
                    parts.reverse();
                    parts.join(" / ")
                }
                _ => node.name.to_string(),
            };
            // shorter form for merged chains: "… / last"
            let short_name = (item.chain != item.node && item.kind != ItemKind::Group).then(|| format!("… / {}", node.name));
            let mut size_txt = fmt_size(item.value);
            if is_root {
                size_txt = format!("{size_txt}  ·  {}", util::count_of(self.vals.files[item.node as usize], "file"));
            } else if item.depth == 1 || matches!(item.kind, ItemKind::Free | ItemKind::Unknown) {
                size_txt = format!("{size_txt}  ·  {:.0}%", item.value as f64 * 100.0 / root_total as f64);
            }
            let (fg, fg_dim) = text_on(if item.header { fills.header } else { fills.body });

            if item.header {
                let big = item.depth <= 1;
                let hfont = font_bold(if big { 13.0 } else { 12.0 });
                let sfont = font(if big { 12.0 } else { 11.5 });
                let avail = r.width() - 14.0;
                let sg = painter.layout_no_wrap(size_txt.clone(), sfont, fg_dim);
                let mut name_w = avail;
                if avail > sg.size().x + 50.0 {
                    name_w = avail - sg.size().x - 12.0;
                    let y = r.min.y + (HEADER_H - sg.size().y) / 2.0;
                    labels.push(Label { pos: pos2(r.max.x - 7.0 - sg.size().x, y), galley: sg });
                }
                let ico = if is_root { icon::HARD_DRIVE } else { icon::FOLDER_SIMPLE };
                let mut text = format!("{ico}  {name}");
                if let Some(short) = &short_name {
                    if painter.layout_no_wrap(text.clone(), hfont.clone(), fg).size().x > name_w {
                        text = format!("{ico}  {short}");
                    }
                }
                if let Some(g) = fit_text(painter, &text, &hfont, fg, name_w) {
                    let y = r.min.y + (HEADER_H - g.size().y) / 2.0;
                    labels.push(Label { pos: pos2(r.min.x + 7.0, y), galley: g });
                }
            } else if item.kind != ItemKind::DirOpen && r.width() >= 30.0 && r.height() >= 16.0 {
                let max_w = r.width() - 10.0;
                let nfont_probe = font_bold(11.5);
                let name = match &short_name {
                    Some(short) if painter.layout_no_wrap(format!("{}  {name}", icon::FOLDER_SIMPLE), nfont_probe, fg).size().x > max_w => short.clone(),
                    _ => name,
                };
                let shown = match item.kind {
                    ItemKind::DirClosed => format!("{}  {name}", icon::FOLDER_SIMPLE),
                    ItemKind::Group => format!("{}  {name}", icon::DOTS_THREE),
                    ItemKind::Free => format!("{}  {name}", icon::CIRCLE_DASHED),
                    ItemKind::Unknown => format!("{}  {name}", icon::QUESTION),
                    _ => name,
                };
                let big = r.width() > 140.0 && r.height() > 60.0;
                let nfont = if big { font_bold(13.0) } else { font_bold(11.5) };
                let sfont = font(if big { 12.0 } else { 11.0 });
                let (nfg, sfg) = if item.kind == ItemKind::Free { (TEXT_DIM, TEXT_FAINT) } else { (fg, fg_dim) };
                let ng = fit_text(painter, &shown, &nfont, nfg, max_w);
                let sg = if r.height() >= 34.0 { fit_text(painter, &size_txt, &sfont, sfg, max_w) } else { None };
                let lines: Vec<Arc<Galley>> = [ng, sg].into_iter().flatten().collect();
                let gap = 2.0;
                let total_h: f32 = lines.iter().map(|g| g.size().y).sum::<f32>() + gap * (lines.len().max(1) - 1) as f32;
                let mut y = r.center().y - total_h / 2.0;
                for g in lines {
                    let x = r.center().x - g.size().x / 2.0;
                    let h = g.size().y;
                    labels.push(Label { pos: pos2(x, y), galley: g });
                    y += h + gap;
                }
            }
        }

        // tessellate once; afterwards drawing the whole map is a single mesh
        let mut tess = Tessellator::new(ppp, TessellationOptions::default(), [1, 1], vec![]);
        let mut mesh = Mesh::default();
        for s in shapes {
            tess.tessellate_shape(s, &mut mesh);
        }
        LayoutCache { key, items, mesh: Arc::new(mesh), labels }
    }

    fn hit(&self, pos: Pos2) -> Option<(Target, Item)> {
        let cache = self.cache.as_ref()?;
        let it = cache.items.iter().rev().find(|it| it.rect.contains(pos))?;
        let t = match it.kind {
            ItemKind::Group => Target::Group(it.node, it.count),
            _ => Target::Node(it.node),
        };
        Some((t, *it))
    }

    fn find_item(&self, t: Target) -> Option<Item> {
        let cache = self.cache.as_ref()?;
        cache
            .items
            .iter()
            .find(|it| match t {
                Target::Group(n, _) => it.kind == ItemKind::Group && it.node == n,
                Target::Node(n) => it.kind != ItemKind::Group && (it.node == n || it.chain == n),
            })
            .copied()
    }

    fn max_depth(&self) -> u32 {
        if self.settings.depth >= DEPTH_UNLIMITED { u32::MAX } else { self.settings.depth }
    }

    /// Folders opened by clicking only stay open in the view they were
    /// opened in (and for the same scan).
    fn reset_expanded(&mut self) {
        if !self.expanded.is_empty() {
            self.expanded.clear();
            self.expanded_ver += 1;
        }
    }

    /// Click on a folder cut off by the level limit: show one more level
    /// inside it. Click on such an opened folder's header: close it again.
    fn toggle_expanded(&mut self, t: Target, it: &Item) {
        let Target::Node(n) = t else { return };
        if it.kind == ItemKind::DirClosed && it.depth as u32 >= self.max_depth() {
            self.expanded.insert(n);
            self.expanded_ver += 1;
        } else if it.kind == ItemKind::DirOpen && self.expanded.remove(&n) {
            self.expanded_ver += 1;
        }
    }

    fn treemap(&mut self, ui: &mut Ui) {
        if self.expanded_root != self.view_root {
            self.expanded_root = self.view_root;
            self.reset_expanded();
        }
        let full = ui.available_rect_before_wrap();
        let response = ui.allocate_rect(full, Sense::click());
        let painter = ui.painter_at(full);
        painter.rect_filled(full, CornerRadius::ZERO, BG);
        let rect = full.shrink(8.0);

        let ppp = ui.ctx().pixels_per_point();
        let key = CacheKey {
            root: self.view_root,
            rect: [rect.min.x as i32, rect.min.y as i32, rect.max.x as i32, rect.max.y as i32],
            depth: self.settings.depth,
            detail: self.settings.detail,
            color: self.settings.color,
            age_date: self.settings.age_date,
            ver: self.ver,
            expanded: self.expanded_ver,
            ppp: ppp.to_bits(),
        };
        if self.cache.as_ref().map(|c| c.key != key).unwrap_or(true) {
            self.cache = Some(self.build_cache(key, rect, &painter, ppp));
        }
        let cache = self.cache.as_ref().unwrap();
        if cache.items.len() <= 1 {
            let msg = if self.filter.is_active() {
                format!("{}  No matching files", icon::FUNNEL)
            } else {
                format!("{}  Empty folder", icon::FOLDER_OPEN)
            };
            painter.text(rect.center(), Align2::CENTER_CENTER, msg, font(16.0), TEXT_DIM);
        }
        painter.add(Shape::Mesh(cache.mesh.clone()));
        for l in &cache.labels {
            painter.galley(l.pos, l.galley.clone(), TEXT);
        }

        // hatching on free space
        for it in cache.items.iter().filter(|i| i.kind == ItemKind::Free) {
            let p = painter.with_clip_rect(it.rect.shrink(2.0));
            let step = 14.0;
            let h = it.rect.height();
            let mut x = it.rect.min.x - h;
            while x < it.rect.max.x {
                p.line_segment(
                    [pos2(x, it.rect.max.y), pos2(x + h, it.rect.min.y)],
                    Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(120, 150, 145, 22)),
                );
                x += step;
            }
        }

        // hover / selection highlight
        let hover = response.hover_pos().and_then(|p| self.hit(p));
        if let Some(sel) = self.selected.and_then(|t| self.find_item(t)) {
            let cr = CornerRadius::same(radius_for(sel.rect));
            painter.rect_stroke(sel.rect.expand(1.0), cr, Stroke::new(2.0_f32, ACCENT), StrokeKind::Outside);
        }
        for it in self.panel.highlight.iter().filter_map(|&n| self.find_item(Target::Node(n))) {
            let cr = CornerRadius::same(radius_for(it.rect));
            painter.rect_filled(it.rect, cr, Color32::from_rgba_unmultiplied(122, 162, 255, 60));
            painter.rect_stroke(it.rect, cr, Stroke::new(2.0_f32, ACCENT), StrokeKind::Inside);
        }
        if let Some(it) = self.panel.hovered.and_then(|n| self.find_item(Target::Node(n))) {
            let cr = CornerRadius::same(radius_for(it.rect));
            painter.rect_filled(it.rect, cr, Color32::from_white_alpha(28));
            painter.rect_stroke(it.rect, cr, Stroke::new(2.0_f32, Color32::WHITE), StrokeKind::Inside);
        }
        if let Some((_, it)) = hover {
            let cr = CornerRadius::same(radius_for(it.rect));
            let area = if it.header { Rect::from_min_max(it.rect.min, pos2(it.rect.max.x, it.rect.min.y + HEADER_H)) } else { it.rect };
            painter.rect_filled(area, cr, Color32::from_white_alpha(16));
            painter.rect_stroke(it.rect, cr, Stroke::new(1.5_f32, Color32::from_white_alpha(200)), StrokeKind::Inside);
        }

        // clicks
        if response.double_clicked() {
            // the first click may have opened the folder, so use what it hit
            // rather than what is under the pointer now
            if let Some(t) = self.last_click.or(hover.map(|h| h.0)) {
                if let Some(z) = self.zoom_target(t) {
                    self.go(z);
                }
            }
        } else if response.clicked() {
            self.selected = hover.map(|h| h.0);
            self.last_click = self.selected;
            if let Some((t, it)) = hover {
                self.toggle_expanded(t, &it);
            }
        }
        if response.middle_clicked() {
            if let (Some((t, _)), Some(tree)) = (hover, &self.tree) {
                util::show_in_file_manager(&tree.path(t.node()));
            }
        }
        if response.secondary_clicked() {
            self.menu_from_list = false;
            self.menu_target = hover.map(|h| h.0);
            if let Some(t) = self.menu_target {
                self.selected = Some(t);
            }
        }

        // mouse wheel: step in / out
        if response.hovered() {
            let dy = ui.ctx().input(|i| if i.modifiers.command { 0.0 } else { i.raw_scroll_delta.y });
            self.scroll_acc += dy;
            if self.scroll_acc > 60.0 {
                self.scroll_acc = 0.0;
                if let Some(next) = hover.and_then(|(t, _)| self.step_towards(t)) {
                    self.go(next);
                }
            } else if self.scroll_acc < -60.0 {
                self.scroll_acc = 0.0;
                self.go_up();
            }
        }

        // tooltip
        if let Some((t, it)) = hover {
            if !response.context_menu_opened() {
                let info = self.describe(t, &it);
                response.clone().on_hover_ui_at_pointer(|ui| info.show(ui));
            }
        }

        if !self.menu_from_list {
            self.context_menu(&response);
        }
    }

    fn describe(&self, t: Target, it: &Item) -> Info {
        let Some(tree) = &self.tree else { return Info::default() };
        let total = self.vals.size[self.view_root as usize].max(1);
        let pct = it.value as f64 * 100.0 / total as f64;
        let mut info = Info::default();
        match t {
            Target::Group(dir, count) => {
                info.icon = icon::DOTS_THREE;
                info.title = format!("{} small items", fmt_count(count as u64));
                info.path = Some(tree.path(dir).display().to_string());
                info.rows.push(("Size", format!("{}  ({pct:.1}% of view)", fmt_size(it.value))));
                info.hint = Some("Double-click to zoom into this folder".into());
            }
            Target::Node(n) => {
                let node = &tree.nodes[n as usize];
                match node.kind {
                    Kind::Free => {
                        info.icon = icon::CIRCLE_DASHED;
                        info.title = "Free space".into();
                        info.rows.push(("Size", format!("{}  ({pct:.1}%)", fmt_size(it.value))));
                        info.hint = Some("Available space on this filesystem".into());
                        return info;
                    }
                    Kind::Unknown => {
                        info.icon = icon::QUESTION;
                        info.title = "Unknown space".into();
                        info.rows.push(("Size", format!("{}  ({pct:.1}%)", fmt_size(it.value))));
                        info.hint = Some("Used space that the scan could not read".into());
                        return info;
                    }
                    Kind::Dir => info.icon = icon::FOLDER_SIMPLE,
                    Kind::Link => info.icon = icon::LINK,
                    _ => info.icon = icon::FILE,
                }
                info.title = node.name.to_string();
                info.path = Some(tree.path(n).display().to_string());
                info.rows.push(("Size", format!("{}  ({pct:.1}% of view)", fmt_size(it.value))));
                if node.is_dir() {
                    info.rows.push(("Files", fmt_count(self.vals.files[n as usize])));
                    if it.kind == ItemKind::DirClosed && it.depth as u32 >= self.max_depth() {
                        info.hint = Some("Click to show what is inside, double-click to zoom in".into());
                    } else if it.kind == ItemKind::DirOpen && self.expanded.contains(&n) {
                        info.hint = Some("Click to close again, double-click to zoom in".into());
                    }
                }
                if self.settings.size_mode == SizeMode::OnDisk && node.kind == Kind::File && fmt_size(node.apparent) != fmt_size(node.alloc) {
                    info.rows.push(("Apparent", fmt_size(node.apparent)));
                }
                info.rows.push(("Modified", util::fmt_date(node.mtime)));
                if !node.is_dir() {
                    // only show when it tells something new (> 1 minute apart)
                    if node.added() - node.mtime > 60 {
                        info.rows.push(("Added", util::fmt_date(node.added())));
                    }
                    if !tree.no_atime() && node.atime - node.mtime.max(node.ctime) > 60 {
                        info.rows.push(("Opened", util::fmt_date(node.atime)));
                    }
                }
                if node.kind == Kind::Link {
                    info.notes.push("Symbolic link (not followed)".into());
                }
                if node.flags & F_DENIED != 0 {
                    info.notes.push(format!("{}  Permission denied", icon::WARNING));
                }
                if node.flags & F_MOUNT != 0 {
                    info.notes.push("Mount point of another filesystem (not scanned)".into());
                }
                if node.flags & F_SNAPSHOTS != 0 {
                    info.notes.push("Btrfs snapshots (not scanned)".into());
                }
                if node.flags & F_HARDLINK_DUP != 0 {
                    info.notes.push("Hard link, counted once".into());
                }
            }
        }
        info
    }

    fn context_menu(&mut self, response: &Response) {
        let Some(t) = self.menu_target else { return };
        let mut action: Option<Action> = None;
        response.context_menu(|ui| {
            let Some(tree) = &self.tree else { return };
            let node = &tree.nodes[t.node() as usize];
            let pseudo = node.is_pseudo();
            let is_dir = matches!(t, Target::Group(..)) || node.is_dir();
            ui.set_min_width(230.0);
            ui.label(
                RichText::new(match t {
                    Target::Group(_, c) => format!("{} small items", c),
                    _ => node.name.to_string(),
                })
                .font(font_bold(13.0))
                .color(TEXT),
            );
            ui.separator();
            let item = |ui: &mut Ui, ic: &str, text: &str| ui.button(format!("{ic}   {text}")).clicked();
            if !pseudo {
                if item(ui, icon::FOLDER_OPEN, if cfg!(windows) { "Show in Explorer" } else if cfg!(target_os = "macos") { "Show in Finder" } else { "Show in file manager" }) {
                    action = Some(Action::Reveal);
                }
                if item(ui, icon::ARROW_SQUARE_OUT, if is_dir { "Open folder" } else { "Open file" }) {
                    action = Some(Action::Open);
                }
                ui.separator();
                if item(ui, icon::MAGNIFYING_GLASS_PLUS, if is_dir { "Zoom into folder" } else { "Zoom into containing folder" }) {
                    action = Some(Action::Zoom);
                }
            }
            if item(ui, icon::MAGNIFYING_GLASS_MINUS, "Zoom out") {
                action = Some(Action::Up);
            }
            if !pseudo {
                ui.separator();
                if item(ui, icon::COPY, "Copy path") {
                    action = Some(Action::CopyPath);
                }
                if item(ui, icon::TEXT_AA, "Copy name") {
                    action = Some(Action::CopyName);
                }
                if is_dir {
                    ui.separator();
                    if ui
                        .add_enabled(self.job.is_none(), egui::Button::new(format!("{}   Rescan this folder", icon::ARROWS_CLOCKWISE)))
                        .clicked()
                    {
                        action = Some(Action::Rescan);
                    }
                }
            }
        });
        let Some(a) = action else { return };
        let ctx = response.ctx.clone();
        let Some(tree) = &self.tree else { return };
        let path = tree.path(t.node());
        match a {
            Action::Reveal => util::show_in_file_manager(&path),
            Action::Open => util::open_path(&path),
            Action::Zoom => {
                if let Some(z) = self.zoom_target(t) {
                    self.go(z);
                }
            }
            Action::Up => self.go_up(),
            Action::CopyPath => {
                ctx.copy_text(path.to_string_lossy().into_owned());
                self.toast("Path copied");
            }
            Action::CopyName => {
                let name = tree.nodes[t.node() as usize].name.to_string();
                ctx.copy_text(name);
                self.toast("Name copied");
            }
            Action::Rescan => {
                let id = self.zoom_target(t).unwrap_or(t.node());
                self.rescan_folder(id, &ctx);
            }
        }
    }
}

#[derive(Default)]
struct Info {
    icon: &'static str,
    title: String,
    path: Option<String>,
    rows: Vec<(&'static str, String)>,
    notes: Vec<String>,
    hint: Option<String>,
}

impl Info {
    fn show(&self, ui: &mut Ui) {
        ui.set_max_width(460.0);
        ui.label(RichText::new(format!("{}  {}", self.icon, self.title)).font(font_bold(14.0)).color(TEXT));
        if let Some(p) = &self.path {
            ui.label(RichText::new(p).size(11.5).color(TEXT_FAINT));
        }
        ui.add_space(2.0);
        egui::Grid::new("tip").num_columns(2).spacing(vec2(14.0, 3.0)).show(ui, |ui| {
            for (k, v) in &self.rows {
                ui.label(RichText::new(*k).color(TEXT_DIM));
                ui.label(RichText::new(v).color(TEXT));
                ui.end_row();
            }
        });
        for n in &self.notes {
            ui.label(RichText::new(n).color(WARN));
        }
        if let Some(h) = &self.hint {
            ui.label(RichText::new(h).color(TEXT_DIM).size(12.0));
        }
    }
}

#[derive(Clone, Copy)]
enum Action {
    Reveal,
    Open,
    Zoom,
    Up,
    CopyPath,
    CopyName,
    Rescan,
}

// ------------------------------------------------------------------ widgets

/// A rounded "pill" holding a row of compact controls.
fn group<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(SURFACE2)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(3))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let w = &mut ui.visuals_mut().widgets;
            w.inactive.weak_bg_fill = Color32::TRANSPARENT;
            w.inactive.bg_fill = Color32::TRANSPARENT;
            ui.horizontal(|ui| add(ui)).inner
        })
        .inner
}

fn icon_btn(ui: &mut Ui, ic: &str, tip: &str, enabled: bool, selected: bool) -> Response {
    let col = if !enabled {
        TEXT_FAINT
    } else if selected {
        ACCENT
    } else {
        TEXT
    };
    let b = egui::Button::new(RichText::new(ic).size(17.0).color(col))
        .min_size(vec2(32.0, 28.0))
        .selected(selected);
    ui.add_enabled(enabled, b).on_hover_text(tip)
}

fn text_btn(ui: &mut Ui, ic: &str, text: &str, tip: &str, selected: bool) -> Response {
    let col = if selected { ACCENT } else { TEXT };
    let b = egui::Button::new(RichText::new(format!("{ic}  {text}")).color(col))
        .min_size(vec2(0.0, 28.0))
        .selected(selected);
    ui.add(b).on_hover_text(tip)
}

fn caption(ui: &mut Ui, ic: &str, tip: &str) {
    ui.add_space(4.0);
    ui.label(RichText::new(ic).size(15.0).color(TEXT_FAINT)).on_hover_text(tip);
    ui.add_space(2.0);
}

/// Row of dots showing a level; clicking a dot sets it.
fn level_dots(ui: &mut Ui, value: &mut usize, count: usize) -> bool {
    let size = vec2(count as f32 * 11.0 + 6.0, 28.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let mut changed = false;
    if let Some(p) = resp.interact_pointer_pos() {
        if resp.clicked() {
            let i = ((p.x - rect.min.x - 3.0) / 11.0).floor().clamp(0.0, (count - 1) as f32) as usize;
            changed = i != *value;
            *value = i;
        }
    }
    let painter = ui.painter();
    for i in 0..count {
        let c = pos2(rect.min.x + 3.0 + 5.5 + i as f32 * 11.0, rect.center().y);
        let on = i <= *value;
        let r = if i == *value { 4.0 } else { 3.0 };
        painter.circle_filled(c, r, if on { ACCENT } else { SURFACE3 });
    }
    changed
}

/// Busy indicator like egui's Spinner, but it asks for about 30 frames per
/// second. egui's asks for a new frame every frame, which a hidden window
/// (no vsync) turns into hundreds of frames per second.
fn spinner(ui: &mut Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    // egui subtracts the expected frame time (about 16 ms) from the delay
    ui.ctx().request_repaint_after(Duration::from_millis(33));
    let radius = size / 2.0 - 2.0;
    let n = (radius.round() as u32).clamp(8, 128);
    let time = ui.input(|i| i.time);
    let start = time * std::f64::consts::TAU;
    let end = start + 240f64.to_radians() * time.sin();
    let points: Vec<Pos2> = (0..n)
        .map(|i| {
            let a = start + (end - start) * i as f64 / n as f64;
            rect.center() + radius * vec2(a.cos() as f32, a.sin() as f32)
        })
        .collect();
    ui.painter().add(egui::Shape::line(points, Stroke::new(3.0_f32, ACCENT)));
}

/// iOS-style switch
fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let size = vec2(36.0, 20.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    let p = ui.painter();
    let bg = if *on { ACCENT_DIM.lerp_to_gamma(ACCENT, 0.35 * t) } else { SURFACE3 };
    p.rect_filled(rect, CornerRadius::same(10), bg);
    let x = egui::lerp((rect.left() + 10.0)..=(rect.right() - 10.0), t);
    p.circle_filled(pos2(x, rect.center().y), 7.0, if *on { Color32::WHITE } else { TEXT_DIM });
    resp
}

fn usage_bar(ui: &mut Ui, frac: f32, width: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, 6.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(3), SURFACE3);
    let fill = Rect::from_min_size(rect.min, vec2(rect.width() * frac.clamp(0.0, 1.0), rect.height()));
    let col = if frac > 0.9 { DANGER } else if frac > 0.75 { WARN } else { ACCENT };
    p.rect_filled(fill, CornerRadius::same(3), col);
}

fn stat(ui: &mut Ui, ic: &str, text: impl Into<String>) -> Response {
    ui.label(RichText::new(format!("{ic}  {}", text.into())).color(TEXT_DIM).size(12.5))
}

// ------------------------------------------------------------------ panels

impl App {
    fn toolbar(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let mut changed_values = false;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            // navigation
            group(ui, |ui| {
                if icon_btn(ui, icon::ARROW_LEFT, "Back  (Backspace)", !self.back.is_empty(), false).clicked() {
                    self.go_back();
                }
                if icon_btn(ui, icon::ARROW_RIGHT, "Forward", !self.fwd.is_empty(), false).clicked() {
                    self.go_fwd();
                }
                let up = self.can_go_up();
                if icon_btn(ui, icon::ARROW_UP, "Up one level  (U)", up, false).clicked() {
                    self.go_up();
                }
                if icon_btn(ui, icon::HOUSE, "Top level  (Home)", up, false).clicked() {
                    self.go_home();
                }
            });

            // granularity: detail
            group(ui, |ui| {
                caption(ui, icon::SQUARES_FOUR, "Detail");
                if icon_btn(ui, icon::MINUS, "Less detail  ( [ )", self.settings.detail > 0, false).clicked() {
                    self.less_detail();
                }
                let mut d = self.settings.detail;
                level_dots(ui, &mut d, DETAIL_LEVELS.len());
                self.settings.detail = d;
                ui.add_sized(vec2(52.0, 28.0), egui::Label::new(RichText::new(DETAIL_LEVELS[d].1).color(TEXT_DIM).size(12.5)));
                if icon_btn(ui, icon::PLUS, "More detail  ( ] )", self.settings.detail + 1 < DETAIL_LEVELS.len(), false).clicked() {
                    self.more_detail();
                }
            });

            // granularity: depth
            group(ui, |ui| {
                caption(ui, icon::STACK, "Folder levels");
                if icon_btn(ui, icon::MINUS, "Fewer levels  ( - )", self.settings.depth > 1, false).clicked() {
                    self.less_depth();
                }
                let txt = if self.settings.depth >= DEPTH_UNLIMITED { "All".to_string() } else { format!("{} lvl", self.settings.depth) };
                ui.add_sized(vec2(40.0, 28.0), egui::Label::new(RichText::new(txt).color(TEXT_DIM).size(12.5)));
                if icon_btn(ui, icon::PLUS, "More levels  ( + )", self.settings.depth < DEPTH_UNLIMITED, false).clicked() {
                    self.more_depth();
                }
            });

            // colours
            group(ui, |ui| {
                for (mode, ic, tip) in [
                    (ColorMode::Sections, icon::PALETTE, "Color by top-level folder"),
                    (ColorMode::Mono, icon::CIRCLE_HALF, "Grayscale"),
                    (ColorMode::ByType, icon::TAG, "Color by file type"),
                    (ColorMode::ByAge, icon::CLOCK, "Color by age: green = recent, red = old"),
                ] {
                    if icon_btn(ui, ic, tip, true, self.settings.color == mode).clicked() {
                        self.settings.color = mode;
                    }
                }
                if self.settings.color == ColorMode::ByAge {
                    let label = |d: DateKind| match d {
                        DateKind::Modified => "Modified",
                        DateKind::Added => "Added",
                        DateKind::Opened => "Opened",
                    };
                    egui::ComboBox::from_id_salt("age_date")
                        .selected_text(RichText::new(label(self.settings.age_date)).size(12.5))
                        .width(84.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.settings.age_date, DateKind::Added, "Added")
                                .on_hover_text("When the file was put on this disk or last changed");
                            ui.selectable_value(&mut self.settings.age_date, DateKind::Modified, "Modified")
                                .on_hover_text("When the file contents were last changed. Installed and downloaded files often keep old dates.");
                            ui.selectable_value(&mut self.settings.age_date, DateKind::Opened, "Opened")
                                .on_hover_text(if cfg!(windows) { "When the file was last read. Windows does not reliably record this." } else { "When the file was last read. Not recorded on drives mounted with noatime." });
                        });
                    if self.settings.age_date == DateKind::Opened && self.tree.as_ref().map(|t| t.no_atime()).unwrap_or(false) {
                        ui.label(RichText::new(icon::WARNING).color(WARN))
                            .on_hover_text(if cfg!(windows) {
                                "Windows does not reliably record when files are opened. Colors fall back to the modified date."
                            } else {
                                "This drive is mounted with noatime, so open dates are not recorded. Colors fall back to the modified date."
                            });
                    }
                }
            });

            // size mode
            group(ui, |ui| {
                let before = self.settings.size_mode;
                if text_btn(ui, icon::HARD_DRIVES, "Disk", "Size on disk", self.settings.size_mode == SizeMode::OnDisk).clicked() {
                    self.settings.size_mode = SizeMode::OnDisk;
                }
                if text_btn(ui, icon::FILE, "Length", "Apparent file size", self.settings.size_mode == SizeMode::Apparent).clicked() {
                    self.settings.size_mode = SizeMode::Apparent;
                }
                changed_values |= before != self.settings.size_mode;
            });

            // free / unknown
            let has_space = self.tree.as_ref().map(|t| t.free_id != NONE).unwrap_or(false);
            let has_unknown = self.tree.as_ref().map(|t| t.unknown_id != NONE).unwrap_or(false);
            if has_space || has_unknown {
                group(ui, |ui| {
                    if has_space && text_btn(ui, icon::CIRCLE_DASHED, "Free", "Show free space", self.settings.show_free).clicked() {
                        self.settings.show_free = !self.settings.show_free;
                        changed_values = true;
                    }
                    if has_unknown
                        && text_btn(ui, icon::QUESTION, "Unknown", "Show unknown space", self.settings.show_unknown).clicked()
                    {
                        self.settings.show_unknown = !self.settings.show_unknown;
                        changed_values = true;
                    }
                });
            }

            // actions
            group(ui, |ui| {
                if icon_btn(ui, icon::SIDEBAR_SIMPLE, "Show list  (L)", true, self.panel.open).clicked() {
                    self.panel.open = !self.panel.open;
                }
                if icon_btn(ui, icon::ARROWS_CLOCKWISE, "Rescan  (F5)", self.job.is_none(), false).clicked() {
                    let id = self.view_root;
                    self.rescan_folder(id, &ctx);
                }
                if icon_btn(ui, icon::HARD_DRIVES, "Drives", true, false).clicked() {
                    self.to_drives = true;
                    ctx.request_repaint();
                }
            });
        });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            // search box
            let active = self.filter.is_active();
            egui::Frame::new()
                .fill(SURFACE2)
                .corner_radius(CornerRadius::same(10))
                .stroke(Stroke::new(1.0_f32, if active { ACCENT_DIM } else { Color32::TRANSPARENT }))
                .inner_margin(Margin::symmetric(10, 4))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(icon::FUNNEL).size(16.0).color(if active { ACCENT } else { TEXT_FAINT }));
                        let te = egui::TextEdit::singleline(&mut self.filter_text)
                            .id(egui::Id::new("filter_box"))
                            .frame(false)
                            .hint_text("Filter, e.g.  *.mkv  or  >1gb")
                            .desired_width(360.0);
                        let r = ui.add(te);
                        if r.changed() {
                            self.filter_edited = Some(Instant::now());
                        }
                        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                        if enter && self.filter_text != self.filter_applied {
                            changed_values = false;
                            self.apply_filter();
                        }
                        if !self.filter_text.is_empty() {
                            if ui.add(egui::Button::new(RichText::new(icon::X).color(TEXT_DIM)).frame(false)).on_hover_text("Clear filter").clicked() {
                                self.filter_text.clear();
                                self.apply_filter();
                            }
                        }
                        ui.label(RichText::new(icon::QUESTION).size(15.0).color(TEXT_FAINT)).on_hover_ui(|ui| {
                            ui.label(RichText::new("Filter (separate terms with ;)").font(font_bold(13.0)));
                            ui.add_space(4.0);
                            egui::Grid::new("fhelp").spacing(vec2(16.0, 3.0)).show(ui, |ui| {
                                for (a, b) in [
                                    ("*.mkv;*.mp4", "files matching any pattern"),
                                    ("cache", "name contains \"cache\""),
                                    ("/node_modules/", "match against the full path"),
                                    ("!*.log", "hide matching files"),
                                    (">100mb  <2gb", "size limits (kb, mb, gb, tb)"),
                                    (">1y  <7d", "modified more than a year ago / in the last 7 days"),
                                    ("added>1y  added<7d", "put on this disk or changed (installed, copied, downloaded)"),
                                    ("opened>1y", if cfg!(windows) { "not opened for a year (not reliable on Windows)" } else { "not opened for a year (not on noatime drives)" }),
                                    ("empty", "files of 0 bytes"),
                                    ("hidden", "hidden files and files in hidden folders"),
                                    ("", "age units: h, d, w, mo, y"),
                                ] {
                                    ui.monospace(a);
                                    ui.label(RichText::new(b).color(TEXT_DIM));
                                    ui.end_row();
                                }
                            });
                        });
                    });
                });
            self.filter_presets(ui);
            if let Some(err) = &self.filter.error {
                ui.label(RichText::new(format!("{}  {err}", icon::WARNING)).color(WARN));
            } else if self.filter.uses_opened() && self.tree.as_ref().map(|t| t.no_atime()).unwrap_or(false) {
                ui.label(RichText::new(format!("{}  Open dates not recorded on this drive", icon::WARNING)).color(WARN))
                    .on_hover_text(if cfg!(windows) {
                        "Windows does not reliably record when files are opened, so the \"opened\" filter cannot work here."
                    } else {
                        "The drive is mounted with the noatime option, so the system does not record when files are opened. The \"opened\" filter cannot work here."
                    });
            }
            ui.add_space(6.0);
            self.breadcrumbs(ui);
        });

        if changed_values {
            self.recompute();
        }
    }

    fn filter_presets(&mut self, ui: &mut Ui) {
        type Preset = (&'static str, &'static str, &'static str);
        const SIZE: &[Preset] = &[
            (icon::ARROW_FAT_UP, "Files over 1 GB", ">1gb"),
            (icon::ARROW_FAT_UP, "Files over 100 MB", ">100mb"),
            (icon::ARROW_FAT_UP, "Files over 10 MB", ">10mb"),
            (icon::FILE_DASHED, "Empty files", "empty"),
        ];
        const AGE: &[Preset] = &[
            (icon::SPARKLE, "Added in the last 7 days", "added<7d"),
            (icon::SPARKLE, "Added in the last 30 days", "added<30d"),
            (icon::CALENDAR_BLANK, "On this disk for over a year", "added>1y"),
            (icon::CALENDAR_BLANK, "Contents not changed for a year", "modified>1y"),
            (icon::EYE_SLASH, "Not opened for a year", "opened>1y"),
            (icon::STACK, "Large and old (over 100 MB, over a year)", ">100mb;added>1y"),
        ];
        const TYPES: &[Preset] = &[
            (icon::FILM_STRIP, "Videos", "*.mp4;*.mkv;*.mov;*.avi;*.webm;*.m4v;*.wmv;*.mpg;*.mpeg;*.ts"),
            (icon::IMAGE, "Images", "*.jpg;*.jpeg;*.png;*.gif;*.webp;*.heic;*.avif;*.tif;*.tiff;*.exr;*.hdr;*.raw;*.cr2;*.cr3;*.nef;*.arw;*.dng;*.psd;*.kra;*.xcf"),
            (icon::MUSIC_NOTES, "Audio", "*.mp3;*.flac;*.wav;*.ogg;*.opus;*.m4a;*.aac;*.aiff"),
            (icon::FILE_TEXT, "Documents", "*.pdf;*.doc;*.docx;*.odt;*.rtf;*.txt;*.md;*.xls;*.xlsx;*.ods;*.csv;*.ppt;*.pptx;*.odp;*.epub"),
            (icon::CUBE, "3D and design files", "*.blend;*.blend1;*.fbx;*.obj;*.gltf;*.glb;*.usd;*.usda;*.usdc;*.usdz;*.abc;*.ztl;*.zpr;*.spp;*.sbs;*.sbsar;*.max;*.ma;*.mb;*.uasset;*.umap"),
            (icon::FILE_ZIP, "Archives", "*.zip;*.7z;*.rar;*.tar;*.gz;*.tgz;*.xz;*.zst;*.bz2;*.lz4"),
            (icon::DISC, "Disk and VM images", "*.iso;*.img;*.qcow2;*.vdi;*.vmdk;*.vhd;*.vhdx;*.raw.xz"),
        ];
        #[cfg(all(unix, not(target_os = "macos")))]
        const CLEANUP: &[Preset] = &[
            (icon::BROOM, "Caches", "/.cache/;/cache/;/caches/"),
            (icon::GAME_CONTROLLER, "Shader caches", "/mesa_shader_cache/;/mesa_shader_cache_db/;/shadercache/;/.nv/;/nvidia/glcache/;/dxvk-cache/;/vkd3d-proton.cache"),
            (icon::GLOBE, "Browser caches", "/.cache/mozilla/;/.cache/chromium/;/.cache/google-chrome/;/.cache/bravesoftware/;/.cache/vivaldi/;/.cache/microsoft-edge/"),
            (icon::IMAGES, "Thumbnails", "/.cache/thumbnails/"),
            (icon::PACKAGE, "Package caches", "/var/cache/pacman/;/var/cache/apt/;/var/cache/dnf/;/var/cache/zypp/;/.cargo/registry/;/.npm/;/.cache/pip/;/.cache/yay/;/.cache/paru/;/.gradle/caches/;/.m2/repository/"),
            (icon::CODE, "Build and dependency folders", "/node_modules/;/target/;/__pycache__/;/.venv/;/venv/;/build/;/.gradle/;/dist/"),
            (icon::SCROLL, "Logs", "*.log;*.log.*;/var/log/"),
            (icon::BUG, "Crash dumps", "/var/lib/systemd/coredump/;/var/crash/"),
            (icon::TRASH, "Trash", "/.local/share/trash/"),
        ];
        #[cfg(all(unix, not(target_os = "macos")))]
        const APPS: &[Preset] = &[
            (icon::GAME_CONTROLLER, "Steam games", "/steamapps/"),
            (icon::GAME_CONTROLLER, "Wine and Proton prefixes", "/compatdata/;/.wine/;/pfx/drive_c/"),
            (icon::PACKAGE, "Flatpak apps and data", "/var/lib/flatpak/;/.local/share/flatpak/;/.var/app/"),
            (icon::PACKAGE, "Snap packages", "/var/lib/snapd/;/snap/"),
            (icon::CUBE, "Containers (Docker, Podman)", "/var/lib/docker/;/var/lib/containers/;/.local/share/containers/"),
            (icon::DESKTOP, "Virtual machines", "/var/lib/libvirt/;/virtualbox vms/;/.local/share/gnome-boxes/;*.qcow2;*.vdi;*.vmdk"),
        ];
        #[cfg(target_os = "macos")]
        const CLEANUP: &[Preset] = &[
            (icon::BROOM, "Caches", "/library/caches/"),
            (icon::GLOBE, "Browser caches", "/library/caches/com.apple.safari/;/library/caches/google/chrome/;/library/caches/firefox/;/library/caches/microsoft edge/;/library/caches/bravesoftware/"),
            (icon::CODE, "Xcode", "/library/developer/xcode/deriveddata/;/library/developer/xcode/archives/;/library/developer/xcode/ios devicesupport/;/library/developer/coresimulator/"),
            (icon::HARD_DRIVE, "iOS backups", "/library/application support/mobilesync/backup/"),
            (icon::PACKAGE, "Package caches", "/library/caches/homebrew/;/.cargo/registry/;/.npm/;/library/caches/pip/;/.gradle/caches/;/.m2/repository/"),
            (icon::CODE, "Build and dependency folders", "/node_modules/;/target/;/__pycache__/;/.venv/;/venv/;/build/;/.gradle/;/dist/"),
            (icon::SCROLL, "Logs", "*.log;/library/logs/;/private/var/log/"),
            (icon::BUG, "Crash reports", "/library/logs/diagnosticreports/"),
            (icon::TRASH, "Trash", "/.trash/"),
        ];
        #[cfg(target_os = "macos")]
        const APPS: &[Preset] = &[
            (icon::GAME_CONTROLLER, "Steam games", "/library/application support/steam/steamapps/"),
            (icon::CUBE, "Docker Desktop", "/library/containers/com.docker.docker/"),
            (icon::DESKTOP, "Virtual machines", "/library/containers/com.utmapp.utm/;/parallels/;*.vmwarevm;*.utm;*.pvm"),
            (icon::FILE_TEXT, "Mail and Messages attachments", "/library/mail/;/library/messages/attachments/"),
        ];
        #[cfg(windows)]
        const CLEANUP: &[Preset] = &[
            (icon::BROOM, "Temporary files", "/appdata/local/temp/;/windows/temp/"),
            (icon::BROOM, "Caches", "/cache/;/caches/;/cache2/;/inetcache/"),
            (icon::GAME_CONTROLLER, "Shader caches", "/d3dscache/;/nvidia/dxcache/;/nvidia/glcache/;/amd/dxcache/;/amd/vkcache/;/shadercache/;/steamapps/shadercache/"),
            (icon::GLOBE, "Browser caches", "/mozilla/firefox/profiles/;/google/chrome/user data/default/cache/;/microsoft/edge/user data/default/cache/;/bravesoftware/brave-browser/user data/default/cache/"),
            (icon::IMAGES, "Thumbnails", "/microsoft/windows/explorer/thumbcache_"),
            (icon::PACKAGE, "Windows Update downloads", "/windows/softwaredistribution/download/"),
            (icon::PACKAGE, "Installer and package caches", "/windows/installer/;/package cache/;/.cargo/registry/;/npm-cache/;/pip/cache/;/nuget/;/.gradle/caches/;/.m2/repository/"),
            (icon::CODE, "Build and dependency folders", "/node_modules/;/target/;/__pycache__/;/.venv/;/venv/;/build/;/.gradle/;/dist/;/bin/debug/;/obj/"),
            (icon::SCROLL, "Logs", "*.log;*.etl;/windows/logs/"),
            (icon::BUG, "Crash dumps", "*.dmp;/crashdumps/;/windows/minidump/;/windows/livekernelreports/"),
            (icon::HARD_DRIVE, "Page file, hibernation and swap", "pagefile.sys;hiberfil.sys;swapfile.sys"),
            (icon::TRASH, "Recycle Bin", "/$recycle.bin/"),
            (icon::CLOCK, "Old Windows installation", "/windows.old/"),
        ];
        #[cfg(windows)]
        const APPS: &[Preset] = &[
            (icon::GAME_CONTROLLER, "Steam games", "/steamapps/"),
            (icon::GAME_CONTROLLER, "Epic, GOG, EA and Ubisoft games", "/epic games/;/gog galaxy/games/;/ea games/;/ubisoft game launcher/games/"),
            (icon::GAME_CONTROLLER, "Xbox app games", "/xboxgames/;/windowsapps/"),
            (icon::PACKAGE, "Installed programs", "/program files/;/program files (x86)/"),
            (icon::PACKAGE, "App data", "/appdata/"),
            (icon::CUBE, "WSL and Docker", "/packages/canonicalgroup;/docker/wsl/;*.vhdx"),
            (icon::DESKTOP, "Virtual machines", "/virtualbox vms/;/virtual machines/;*.vdi;*.vmdk;*.vhd;*.vhdx;*.qcow2"),
        ];
        const OTHER: &[Preset] = &[(icon::EYE_SLASH, "Hidden files and folders", "hidden")];

        let no_atime = self.tree.as_ref().map(|t| t.no_atime()).unwrap_or(false);
        let mut chosen: Option<&str> = None;
        let section = |ui: &mut Ui, ic: &str, title: &str, list: &[Preset], chosen: &mut Option<&'static str>| {
            ui.menu_button(format!("{ic}   {title}"), |ui| {
                ui.set_min_width(290.0);
                for &(ic, label, pat) in list {
                    let disabled = no_atime && pat.contains("opened");
                    let tip = if disabled {
                        if cfg!(windows) {
                            "Windows does not reliably record when files are opened.".to_string()
                        } else {
                            "This drive is mounted with noatime, so it does not record when files are opened.".to_string()
                        }
                    } else {
                        pat.replace(';', "  ")
                    };
                    let r = ui.add_enabled(!disabled, egui::Button::new(format!("{ic}   {label}")));
                    let r = if disabled { r.on_disabled_hover_text(tip) } else { r.on_hover_text(tip) };
                    if r.clicked() {
                        *chosen = Some(pat);
                        ui.close();
                    }
                }
            });
        };
        group(ui, |ui| {
            ui.menu_button(RichText::new(format!("{}  Sort  {}", icon::FUNNEL_SIMPLE, icon::CARET_DOWN)).color(TEXT), |ui| {
                ui.set_min_width(220.0);
                section(ui, icon::ARROWS_OUT_SIMPLE, "Size", SIZE, &mut chosen);
                section(ui, icon::CLOCK, "Age", AGE, &mut chosen);
                section(ui, icon::TAG, "File types", TYPES, &mut chosen);
                section(ui, icon::BROOM, "Cleanup candidates", CLEANUP, &mut chosen);
                section(ui, icon::SQUARES_FOUR, "Apps and games", APPS, &mut chosen);
                section(ui, icon::DOTS_THREE, "Other", OTHER, &mut chosen);
                ui.separator();
                if ui.button(format!("{}   Clear filter", icon::X)).clicked() {
                    chosen = Some("");
                    ui.close();
                }
            });
        });
        if let Some(p) = chosen {
            self.filter_text = p.to_string();
            self.apply_filter();
        }
    }

    fn breadcrumbs(&mut self, ui: &mut Ui) {
        let Some(tree) = &self.tree else { return };
        let chain = tree.chain(self.view_root);
        let mut go_to = None;
        egui::ScrollArea::horizontal().id_salt("crumbs").max_height(32.0).stick_to_right(true).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for (i, &id) in chain.iter().enumerate() {
                    if i > 0 {
                        ui.label(RichText::new(icon::CARET_RIGHT).size(12.0).color(TEXT_FAINT));
                    }
                    let last = i + 1 == chain.len();
                    let name = if i == 0 {
                        format!("{}  {}", icon::HARD_DRIVE, tree.root_path.to_string_lossy())
                    } else {
                        tree.nodes[id as usize].name.to_string()
                    };
                    let txt = if last { RichText::new(name).color(TEXT).font(font_bold(13.0)) } else { RichText::new(name).color(TEXT_DIM) };
                    let b = egui::Button::new(txt).fill(if last { SURFACE2 } else { Color32::TRANSPARENT }).corner_radius(CornerRadius::same(8));
                    if ui.add(b).on_hover_text(fmt_size(self.vals.size[id as usize])).clicked() && !last {
                        go_to = Some(id);
                    }
                }
            });
        });
        if let Some(id) = go_to {
            self.go(id);
        }
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 18.0;
            if let Some(job) = &self.job {
                spinner(ui, 14.0);
                stat(ui, icon::ARROWS_CLOCKWISE, format!("Rescanning {}  ·  {}", job.path.display(), util::count_of(job.progress.files.load(Relaxed), "file")));
                if ui.small_button("Cancel").clicked() {
                    job.progress.cancel.store(true, Relaxed);
                }
            }
            if let Some(tree) = &self.tree {
                let mut real_total = self.vals.size[tree.root as usize];
                for id in [tree.free_id, tree.unknown_id] {
                    if id != NONE {
                        real_total = real_total.saturating_sub(self.vals.size[id as usize]);
                    }
                }
                stat(ui, icon::FILES, util::count_of(self.vals.files[tree.root as usize], "file"));
                stat(ui, icon::DATABASE, fmt_size(real_total));
                if let Some(space) = tree.space {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        stat(ui, icon::HARD_DRIVE, format!("{} free of {}", fmt_size(space.avail), fmt_size(space.total)));
                        let frac = if space.total > 0 { space.used as f32 / space.total as f32 } else { 0.0 };
                        usage_bar(ui, frac, 80.0);
                    });
                }
                if let Some((dur, _, cancelled)) = self.last_scan {
                    stat(ui, icon::TIMER, util::fmt_duration(dur));
                    if cancelled {
                        ui.label(RichText::new(format!("{}  Scan stopped early", icon::WARNING)).color(WARN).size(12.5));
                    }
                }
                if self.scanned_elevated {
                    stat(ui, icon::SHIELD_CHECK, "full access");
                }
                let denied = self.denied;
                let mut rescan_full = false;
                if denied > 0 {
                    stat(ui, icon::LOCK_SIMPLE, format!("{} folders not readable", fmt_count(denied as u64)))
                        .on_hover_text("Their size is included in Unknown.");
                    if !self.scanned_elevated && self.job.is_none() {
                        let b = egui::Button::new(RichText::new(format!("{}  Rescan with full access", icon::SHIELD_CHECK)).size(12.5).color(ACCENT))
                            .fill(SURFACE2)
                            .corner_radius(CornerRadius::same(8));
                        let tip = if cfg!(windows) { "Restarts Spacemapper as administrator" } else { "Requires administrator password" };
                        rescan_full = ui.add(b).on_hover_text(tip).clicked();
                    }
                    if cfg!(target_os = "macos") && !self.full_disk_access {
                        let b = egui::Button::new(RichText::new(format!("{}  Open Full Disk Access settings", icon::LOCK_SIMPLE)).size(12.5).color(ACCENT))
                            .fill(SURFACE2)
                            .corner_radius(CornerRadius::same(8));
                        if ui.add(b).on_hover_text("Allow Spacemapper in System Settings, then restart Spacemapper").clicked() {
                            util::open_full_disk_access_settings();
                        }
                    }
                }
                if rescan_full {
                    let p = tree.root_path.clone();
                    let ctx = ui.ctx().clone();
                    if cfg!(windows) {
                        self.restart_as_admin(Some(p), &ctx);
                    } else {
                        self.settings.full_access = true;
                        self.begin_scan(p, &ctx);
                    }
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some((msg, t)) = &self.toast {
                    if t.elapsed() < Duration::from_secs(3) {
                        ui.label(RichText::new(format!("{}  {msg}", icon::CHECK)).color(Color32::from_rgb(130, 210, 150)).size(12.5));
                    }
                }
            });
        });
    }

    fn start_screen(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let width = (ui.available_width() - 48.0).min(720.0);
            let margin = ((ui.available_width() - width) / 2.0).max(0.0);
            ui.add_space(40.0);
            ui.horizontal(|ui| {
                ui.add_space(margin);
                ui.vertical(|ui| {
                    ui.set_width(width);
                    // header
                    ui.horizontal(|ui| {
                        ui.add(egui::Image::new(&self.logo).fit_to_exact_size(vec2(52.0, 52.0)));
                        ui.add_space(6.0);
                        ui.vertical(|ui| {
                            ui.label(RichText::new("Spacemapper").font(font_bold(26.0)).color(TEXT));
                            ui.label(RichText::new("Disk usage viewer").color(TEXT_DIM));
                        });
                    });
                    ui.add_space(26.0);
                    if let Some(err) = &self.error {
                        ui.label(RichText::new(format!("{}  {err}", icon::WARNING)).color(DANGER));
                        ui.add_space(10.0);
                    }

                    egui::Frame::new()
                        .fill(SURFACE)
                        .stroke(Stroke::new(1.0_f32, Color32::from_rgb(34, 34, 40)))
                        .corner_radius(CornerRadius::same(12))
                        .inner_margin(Margin::symmetric(16, 12))
                        .show(ui, |ui| {
                            ui.set_width(width - 32.0);
                            if cfg!(windows) {
                                let admin = util::is_admin();
                                ui.horizontal(|ui| {
                                    ui.vertical(|ui| {
                                        ui.spacing_mut().item_spacing.y = 1.0;
                                        ui.label(RichText::new(format!("{}  Full access", icon::SHIELD_CHECK)).color(TEXT).font(font_bold(13.5)));
                                        let text = if admin {
                                            "Running as administrator. System folders are included."
                                        } else {
                                            "Run as administrator so system folders are included."
                                        };
                                        ui.label(RichText::new(text).size(12.0).color(TEXT_DIM));
                                    });
                                    if !admin {
                                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                            let b = egui::Button::new(RichText::new(format!("{}  Restart as administrator", icon::SHIELD_CHECK)).size(12.5).color(ACCENT))
                                                .fill(SURFACE2)
                                                .corner_radius(CornerRadius::same(8));
                                            if ui.add(b).clicked() {
                                                self.restart_as_admin(None, &ctx);
                                            }
                                        });
                                    }
                                });
                            } else {
                        ui.horizontal(|ui| {
                                    toggle(ui, &mut self.settings.full_access);
                                    ui.add_space(4.0);
                                    ui.vertical(|ui| {
                                        ui.spacing_mut().item_spacing.y = 1.0;
                                        ui.label(RichText::new(format!("{}  Full access", icon::SHIELD_CHECK)).color(TEXT).font(font_bold(13.5)));
                                        let text = if cfg!(target_os = "macos") {
                                            "Scan as administrator so system folders and other users' files are included. Asks for your password."
                                        } else {
                                            "Scan as administrator so system folders are included. Asks for your password."
                                        };
                                        ui.label(RichText::new(text).size(12.0).color(TEXT_DIM));
                                    });
                                });
                                if cfg!(target_os = "macos") {
                                    ui.add_space(6.0);
                                    ui.horizontal(|ui| {
                                        ui.vertical(|ui| {
                                            ui.spacing_mut().item_spacing.y = 1.0;
                                            ui.label(RichText::new(format!("{}  Full Disk Access", icon::LOCK_SIMPLE)).color(TEXT).font(font_bold(13.5)));
                                            let text = if self.full_disk_access {
                                                "Full Disk Access is on. Protected folders such as Mail and Messages are included."
                                            } else {
                                                "macOS hides some folders, such as Mail and Messages, unless Spacemapper has Full Disk Access."
                                            };
                                            ui.label(RichText::new(text).size(12.0).color(TEXT_DIM));
                                        });
                                        if !self.full_disk_access {
                                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                                let b = egui::Button::new(RichText::new(format!("{}  Open Full Disk Access settings", icon::SHIELD_CHECK)).size(12.5).color(ACCENT))
                                                    .fill(SURFACE2)
                                                    .corner_radius(CornerRadius::same(8));
                                                if ui.add(b).clicked() {
                                                    util::open_full_disk_access_settings();
                                                }
                                            });
                                        }
                                    });
                                }
                            }
                            ui.add_space(6.0);
                            #[cfg(unix)]
                            ui.horizontal(|ui| {
                                toggle(ui, &mut self.settings.cross_fs);
                                ui.add_space(4.0);
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 1.0;
                                    ui.label(RichText::new(format!("{}  Other drives", icon::HARD_DRIVES)).color(TEXT).font(font_bold(13.5)));
                                    ui.label(RichText::new("Also scan USB drives and network shares mounted inside the folder.").size(12.0).color(TEXT_DIM));
                                });
                            });
                        });
                    ui.add_space(18.0);

                    ui.horizontal(|ui| {
                        ui.label(RichText::new("DRIVES").size(11.5).color(TEXT_FAINT).strong());
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.add(egui::Button::new(RichText::new(format!("{}  Refresh", icon::ARROWS_CLOCKWISE)).size(12.0).color(TEXT_DIM)).frame(false)).clicked() {
                                self.refresh_mounts(&ctx);
                            }
                        });
                    });
                    ui.add_space(4.0);
                    let mut scan: Option<PathBuf> = None;
                    for m in &self.mounts {
                        egui::Frame::new()
                            .fill(SURFACE)
                            .stroke(Stroke::new(1.0_f32, Color32::from_rgb(34, 34, 40)))
                            .corner_radius(CornerRadius::same(12))
                            .inner_margin(Margin::symmetric(16, 14))
                            .show(ui, |ui| {
                                ui.set_width(width - 32.0);
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(icon::HARD_DRIVE).size(28.0).color(TEXT_DIM));
                                    ui.add_space(8.0);
                                    ui.vertical(|ui| {
                                        ui.set_width(width - 200.0);
                                        ui.horizontal(|ui| {
                                            ui.label(RichText::new(m.point.to_string_lossy()).font(font_bold(15.0)).color(TEXT));
                                            let src: String = if m.source.chars().count() > 40 { m.source.chars().take(38).collect::<String>() + "…" } else { m.source.clone() };
                                            ui.label(RichText::new(format!("{src}  ·  {}", m.fstype)).size(12.0).color(TEXT_FAINT));
                                        });
                                        let space = self.mount_space.lock().ok().and_then(|map| map.get(&m.point).copied());
                                        if let Some(s) = space {
                                            let frac = if s.total > 0 { s.used as f32 / s.total as f32 } else { 0.0 };
                                            ui.add_space(2.0);
                                            usage_bar(ui, frac, width - 200.0);
                                            ui.add_space(2.0);
                                            ui.label(
                                                RichText::new(format!("{} used of {}   ·   {} free", fmt_size(s.used), fmt_size(s.total), fmt_size(s.avail)))
                                                    .size(12.0)
                                                    .color(TEXT_DIM),
                                            );
                                        }
                                    });
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        let b = egui::Button::new(RichText::new(format!("Scan  {}", icon::ARROW_RIGHT)).color(Color32::WHITE).font(font_bold(13.5)))
                                            .fill(ACCENT_DIM)
                                            .corner_radius(CornerRadius::same(9))
                                            .min_size(vec2(92.0, 34.0));
                                        if ui.add(b).clicked() {
                                            scan = Some(m.point.clone());
                                        }
                                    });
                                });
                            });
                        ui.add_space(8.0);
                    }

                    ui.add_space(16.0);
                    ui.label(RichText::new("FOLDER").size(11.5).color(TEXT_FAINT).strong());
                    ui.add_space(4.0);
                    egui::Frame::new()
                        .fill(SURFACE)
                        .stroke(Stroke::new(1.0_f32, Color32::from_rgb(34, 34, 40)))
                        .corner_radius(CornerRadius::same(12))
                        .inner_margin(Margin::symmetric(14, 12))
                        .show(ui, |ui| {
                            ui.set_width(width - 28.0);
                            ui.horizontal(|ui| {
                                egui::Frame::new()
                                    .fill(SURFACE2)
                                    .corner_radius(CornerRadius::same(9))
                                    .inner_margin(Margin::symmetric(10, 6))
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.label(RichText::new(icon::FOLDER_SIMPLE).size(16.0).color(TEXT_FAINT));
                                            let r = ui.add(egui::TextEdit::singleline(&mut self.path_input).frame(false).desired_width(width - 260.0));
                                            if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                                                let p = PathBuf::from(self.path_input.trim());
                                                scan = Some(util::absolute(&p));
                                            }
                                        });
                                    });
                                if icon_btn(ui, icon::HOUSE, "Home folder", true, false).clicked() {
                                    self.path_input = util::home_dir().to_string_lossy().into_owned();
                                }
                                let b = egui::Button::new(RichText::new(format!("Scan  {}", icon::ARROW_RIGHT)).color(Color32::WHITE).font(font_bold(13.5)))
                                    .fill(ACCENT_DIM)
                                    .corner_radius(CornerRadius::same(9))
                                    .min_size(vec2(92.0, 34.0));
                                if ui.add(b).clicked() {
                                    let p = PathBuf::from(self.path_input.trim());
                                    scan = Some(util::absolute(&p));
                                }
                            });
                            ui.add_space(4.0);
                            ui.label(RichText::new(format!("{}  You can also drag a folder onto this window.", icon::INFO)).size(12.0).color(TEXT_FAINT));
                        });


                    if let Some(p) = scan {
                        self.path_input = p.to_string_lossy().into_owned();
                        self.begin_scan(p, &ctx);
                    }
                });
            });
        });
    }

    fn scanning_screen(&mut self, ui: &mut Ui) {
        let Some(job) = &self.job else { return };
        let p = &job.progress;
        let mut cancel = false;
        let width = 520.0f32.min(ui.available_width() - 40.0);
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.5 - 150.0).max(20.0));
            egui::Frame::new()
                .fill(SURFACE)
                .stroke(Stroke::new(1.0_f32, Color32::from_rgb(34, 34, 40)))
                .corner_radius(CornerRadius::same(16))
                .inner_margin(Margin::same(24))
                .show(ui, |ui| {
                    ui.set_width(width);
                    ui.vertical_centered(|ui| {
                        spinner(ui, 30.0);
                        ui.add_space(10.0);
                        let title = if job.elevated { format!("{}  Scanning (full access)", icon::SHIELD_CHECK) } else { "Scanning".to_string() };
                        ui.label(RichText::new(title).font(font_bold(18.0)).color(TEXT));
                        if job.elevated && p.dirs.load(Relaxed) == 0 {
                            ui.label(RichText::new("Waiting for password").color(TEXT_DIM));
                        }
                        ui.label(RichText::new(job.path.display().to_string()).color(TEXT_DIM));
                        ui.add_space(14.0);
                        ui.columns(3, |cols| {
                            for (col, (val, label)) in cols.iter_mut().zip([
                                (fmt_count(p.files.load(Relaxed)), "files"),
                                (fmt_count(p.dirs.load(Relaxed)), "folders"),
                                (fmt_size(p.bytes.load(Relaxed)), "total"),
                            ]) {
                                col.vertical_centered(|ui| {
                                    ui.label(RichText::new(val).font(font_bold(20.0)).color(TEXT));
                                    ui.label(RichText::new(label).size(12.0).color(TEXT_FAINT));
                                });
                            }
                        });
                        ui.add_space(12.0);
                        if let Some(used) = job.fs_used {
                            if used > 0 {
                                let frac = (p.bytes.load(Relaxed) as f32 / used as f32).min(1.0);
                                usage_bar(ui, frac, width - 20.0);
                                ui.add_space(6.0);
                            }
                        }
                        let cur = p.current.lock().map(|c| c.clone()).unwrap_or_default();
                        let painter = ui.painter().clone();
                        if let Some(g) = fit_text(&painter, &cur, &font(11.5), TEXT_FAINT, width - 20.0) {
                            let (r, _) = ui.allocate_exact_size(vec2(width - 20.0, g.size().y), Sense::hover());
                            painter.galley(pos2(r.center().x - g.size().x / 2.0, r.min.y), g, TEXT_FAINT);
                        }
                        ui.label(RichText::new(util::fmt_duration(job.started.elapsed())).size(12.0).color(TEXT_FAINT));
                        ui.add_space(12.0);
                        if ui.add(egui::Button::new(format!("{}  Stop", icon::STOP)).corner_radius(CornerRadius::same(9))).clicked() {
                            cancel = true;
                        }
                    });
                });
        });
        if cancel {
            p.cancel.store(true, Relaxed);
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if std::mem::take(&mut self.to_drives) {
            if let Some(job) = self.job.take() {
                job.progress.cancel.store(true, Relaxed);
            }
            self.tree = None;
            self.cache = None;
            self.refresh_mounts(ctx);
        }
        self.poll_job();
        self.poll_dupes();
        self.save_prefs_if_changed();
        self.handle_drops(ctx);

        if let Some(t) = self.filter_edited {
            // Apply while typing when filtering is fast; on very large scans
            // wait for a short pause so typing stays smooth.
            let wait = if self.filter_cost < Duration::from_millis(60) { Duration::ZERO } else { Duration::from_millis(200) };
            if self.filter_text == self.filter_applied {
                self.filter_edited = None;
            } else if t.elapsed() >= wait {
                self.apply_filter();
            } else {
                ctx.request_repaint_after(wait - t.elapsed());
            }
        }

        let title = match (&self.tree, &self.job) {
            (_, Some(j)) if j.target.is_none() => format!("Scanning {} – Spacemapper", j.path.display()),
            (Some(t), _) => format!("{} – Spacemapper", t.path(self.view_root).display()),
            _ => "Spacemapper".into(),
        };
        if title != self.last_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }

        let full_scan = self.job.as_ref().map(|j| j.target.is_none()).unwrap_or(false);
        if self.job.is_some() {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
        if self.toast.as_ref().map(|t| t.1.elapsed() < Duration::from_secs(3)).unwrap_or(false) {
            ctx.request_repaint_after(Duration::from_millis(500));
        }

        if self.access_prompt {
            self.access_window(ctx);
        }

        let bg = egui::Frame::new().fill(BG);
        if full_scan {
            egui::CentralPanel::default().frame(bg).show(ctx, |ui| self.scanning_screen(ui));
            return;
        }
        if self.tree.is_none() {
            egui::CentralPanel::default().frame(bg).show(ctx, |ui| self.start_screen(ui));
            return;
        }

        self.handle_keys(ctx);
        egui::TopBottomPanel::top("toolbar")
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(SURFACE).inner_margin(Margin { left: 10, right: 10, top: 8, bottom: 8 }))
            .show(ctx, |ui| self.toolbar(ui));
        egui::TopBottomPanel::bottom("status")
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(SURFACE).inner_margin(Margin::symmetric(12, 6)))
            .show(ctx, |ui| self.status_bar(ui));
        if self.panel.open {
            egui::SidePanel::right("list")
                .resizable(true)
                .default_width(340.0)
                .width_range(240.0..=700.0)
                .show_separator_line(false)
                .frame(egui::Frame::new().fill(SURFACE).inner_margin(Margin { left: 10, right: 8, top: 0, bottom: 4 }))
                .show(ctx, |ui| self.side_panel(ui));
        } else {
            self.panel.hovered = None;
        }
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ctx, |ui| self.treemap(ui));
    }
}
