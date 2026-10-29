// SPDX-License-Identifier: GPL-3.0-or-later
//! Side panel tabs: duplicate files and changes since an earlier scan.

use super::*;
use crate::dupes;
use crate::history;
use std::collections::HashSet;

pub(super) const DUP_MIN: [(u64, &str); 4] = [(1 << 20, "1 MB"), (10 << 20, "10 MB"), (100 << 20, "100 MB"), (1 << 30, "1 GB")];

pub(super) struct DupJob {
    progress: Arc<dupes::Progress>,
    rx: Receiver<Vec<dupes::Group>>,
    root: u32,
    started: Instant,
}

pub(super) struct DupResult {
    pub root: u32,
    pub groups: Vec<dupes::Group>,
    pub unreadable: u64,
    pub took: Duration,
}

#[derive(Default)]
pub(super) struct DupState {
    pub min_idx: usize,
    pub job: Option<DupJob>,
    pub result: Option<DupResult>,
    pub open: HashSet<usize>,
}

#[derive(Default)]
pub(super) struct ChangesState {
    /// earlier summaries of the scanned folder, newest first
    pub entries: Vec<history::Entry>,
    pub selected: usize,
    pub loaded: Option<(PathBuf, history::Snapshot)>,
    pub error: Option<String>,
}

struct ChangeRow {
    name: String,
    node: Option<u32>,
    now: u64,
    before: Option<u64>,
}

impl ChangeRow {
    fn delta(&self) -> i64 {
        self.now as i64 - self.before.unwrap_or(0) as i64
    }
}

/// Changes below this are folder metadata noise and shown as "no change".
const NOISE: i64 = 64 * 1024;

fn fmt_delta(d: i64) -> String {
    if d.abs() < NOISE {
        "no change".into()
    } else if d > 0 {
        format!("+{}", fmt_size(d as u64))
    } else {
        format!("−{}", fmt_size(d.unsigned_abs()))
    }
}

fn delta_color(d: i64) -> Color32 {
    if d.abs() < NOISE {
        TEXT_FAINT
    } else if d > 0 {
        Color32::from_rgb(240, 140, 110)
    } else if d < 0 {
        Color32::from_rgb(120, 200, 150)
    } else {
        TEXT_FAINT
    }
}

impl App {
    // ------------------------------------------------------------ duplicates

    pub(super) fn poll_dupes(&mut self) {
        let Some(job) = &self.dupes.job else { return };
        let Ok(groups) = job.rx.try_recv() else { return };
        let job = self.dupes.job.take().unwrap();
        if job.progress.cancel.load(Relaxed) {
            return;
        }
        self.dupes.open.clear();
        self.dupes.result = Some(DupResult {
            root: job.root,
            groups,
            unreadable: job.progress.unreadable.load(Relaxed),
            took: job.started.elapsed(),
        });
    }

    fn start_dupes(&mut self, ctx: &egui::Context) {
        let Some(tree) = &self.tree else { return };
        let min = DUP_MIN[self.dupes.min_idx].0;
        // regular files below the current view, each physical file once
        let mut files = Vec::new();
        let mut stack = vec![self.view_root];
        while let Some(i) = stack.pop() {
            let n = &tree.nodes[i as usize];
            if n.is_dir() {
                stack.extend_from_slice(&n.children);
            } else if n.kind == Kind::File && n.flags & F_HARDLINK_DUP == 0 && n.apparent >= min {
                files.push((i, n.apparent));
            }
        }
        let cands: Vec<dupes::Candidate> = dupes::by_size(files)
            .into_iter()
            .map(|(node, size)| dupes::Candidate { node, path: tree.path(node), size })
            .collect();
        let progress = Arc::new(dupes::Progress::default());
        let (tx, rx) = mpsc::channel();
        let p2 = progress.clone();
        let ctx2 = ctx.clone();
        std::thread::Builder::new()
            .name("dupes".into())
            .spawn(move || {
                let groups = dupes::find(cands, p2);
                let _ = tx.send(groups);
                ctx2.request_repaint();
            })
            .expect("spawn duplicate search");
        self.dupes.result = None;
        self.dupes.job = Some(DupJob { progress, rx, root: self.view_root, started: Instant::now() });
    }

    pub(super) fn dupes_panel(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        // running
        if let Some(job) = &self.dupes.job {
            let p = &job.progress;
            ctx.request_repaint_after(Duration::from_millis(150));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                spinner(ui, 16.0);
                ui.label(RichText::new("Comparing files").font(font_bold(14.0)).color(TEXT));
            });
            ui.add_space(4.0);
            let total = p.bytes_total.load(Relaxed).max(1);
            let frac = p.bytes_done.load(Relaxed) as f32 / total as f32;
            usage_bar(ui, frac, ui.available_width() - 8.0);
            ui.label(
                RichText::new(format!(
                    "{} of {} candidates checked  ·  {} read",
                    fmt_count(p.files_done.load(Relaxed)),
                    fmt_count(p.files_total.load(Relaxed)),
                    fmt_size(p.bytes_done.load(Relaxed))
                ))
                .size(12.0)
                .color(TEXT_DIM),
            );
            let cur = p.current.lock().map(|c| c.clone()).unwrap_or_default();
            let painter = ui.painter().clone();
            if let Some(g) = fit_text(&painter, &cur, &font(11.0), TEXT_FAINT, ui.available_width() - 8.0) {
                ui.label(RichText::new(g.text()).size(11.0).color(TEXT_FAINT));
            }
            ui.add_space(6.0);
            if ui.button(format!("{}  Stop", icon::STOP)).clicked() {
                p.cancel.store(true, Relaxed);
            }
            return;
        }

        // options + start button
        ui.add_space(4.0);
        ui.label(
            RichText::new("Finds files with identical contents in the current folder and below. Files are only read, never changed.")
                .size(12.0)
                .color(TEXT_DIM),
        );
        ui.add_space(4.0);
        let mut start = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Larger than").size(12.5).color(TEXT_FAINT));
            group(ui, |ui| {
                for (i, &(_, label)) in DUP_MIN.iter().enumerate() {
                    let sel = self.dupes.min_idx == i;
                    let b = egui::Button::new(RichText::new(label).size(12.5).color(if sel { ACCENT } else { TEXT }))
                        .selected(sel)
                        .min_size(vec2(0.0, 24.0));
                    if ui.add(b).clicked() {
                        self.dupes.min_idx = i;
                    }
                }
            });
        });
        ui.add_space(4.0);
        let label = if self.dupes.result.is_some() { "Search again" } else { "Find duplicates" };
        let b = egui::Button::new(RichText::new(format!("{}  {label}", icon::MAGNIFYING_GLASS)).color(Color32::WHITE).font(font_bold(13.0)))
            .fill(ACCENT_DIM)
            .corner_radius(CornerRadius::same(8))
            .min_size(vec2(0.0, 30.0));
        if ui.add(b).clicked() {
            start = true;
        }
        if start {
            self.start_dupes(&ctx);
            return;
        }

        let Some(res) = &self.dupes.result else { return };
        let Some(tree) = &self.tree else { return };
        ui.add_space(8.0);
        let wasted: u64 = res.groups.iter().map(|g| g.wasted()).sum();
        let scope = tree.path(res.root).to_string_lossy().into_owned();
        if res.groups.is_empty() {
            ui.label(RichText::new(format!("{}  No duplicates found", icon::CHECK)).color(TEXT));
        } else {
            ui.label(
                RichText::new(format!("{} could be freed", fmt_size(wasted)))
                    .font(font_bold(15.0))
                    .color(TEXT),
            );
            ui.label(
                RichText::new(format!("{} in {}  ·  {}", util::count_of(res.groups.len() as u64, "group"), scope, util::fmt_duration(res.took)))
                    .size(11.5)
                    .color(TEXT_FAINT),
            );
        }
        if res.unreadable > 0 {
            ui.label(RichText::new(format!("{}  {} could not be read", icon::LOCK_SIMPLE, util::count_of(res.unreadable, "file"))).size(11.5).color(TEXT_FAINT));
        }
        ui.label(
            RichText::new("Hard links are already left out. On btrfs, XFS or APFS, copies made with reflinks or clones share space and free nothing when deleted.")
                .size(11.0)
                .color(TEXT_FAINT),
        );
        ui.add_space(4.0);

        let mut hovered_group: Option<usize> = None;
        let mut hovered_node: Option<u32> = None;
        let mut action: Option<(u32, bool)> = None; // (node, open) false = reveal in map
        let mut toggle: Option<usize> = None;
        egui::ScrollArea::vertical().id_salt("dupes").auto_shrink([false, false]).show(ui, |ui| {
            for (gi, g) in res.groups.iter().enumerate() {
                let first = &tree.nodes[g.nodes[0] as usize];
                let open = self.dupes.open.contains(&gi);
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::click());
                let p = ui.painter().clone();
                if resp.hovered() {
                    p.rect_filled(rect, CornerRadius::same(7), SURFACE2);
                    hovered_group = Some(gi);
                }
                let caret = if open { icon::CARET_DOWN } else { icon::CARET_RIGHT };
                let sg = p.layout_no_wrap(format!("{}", fmt_size(g.wasted())), font_bold(12.5), TEXT);
                let right = rect.max.x - 8.0;
                p.galley(pos2(right - sg.size().x, rect.min.y + 4.0), sg.clone(), TEXT);
                let sub = p.layout_no_wrap("to free".into(), font(11.0), TEXT_FAINT);
                p.galley(pos2(right - sub.size().x, rect.min.y + 20.0), sub, TEXT_FAINT);
                let w = rect.width() - 30.0 - sg.size().x.max(46.0);
                if let Some(t) = fit_text(&p, &format!("{caret}  {}", first.name), &font(13.0), TEXT, w) {
                    p.galley(pos2(rect.min.x + 6.0, rect.min.y + 3.0), t, TEXT);
                }
                let info = format!("{} copies  ·  {} each", g.nodes.len(), fmt_size(g.size));
                if let Some(t) = fit_text(&p, &info, &font(11.0), TEXT_FAINT, w) {
                    p.galley(pos2(rect.min.x + 24.0, rect.min.y + 20.0), t, TEXT_FAINT);
                }
                if resp.clicked() {
                    toggle = Some(gi);
                }
                if open {
                    for &n in &g.nodes {
                        let path = tree.path(n);
                        let shown = path.strip_prefix(tree.path(res.root)).map(|p| p.to_path_buf()).unwrap_or(path.clone());
                        let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::click());
                        if resp.hovered() {
                            ui.painter().rect_filled(r, CornerRadius::same(6), SURFACE2);
                            hovered_node = Some(n);
                            hovered_group = Some(gi);
                        }
                        if let Some(t) = fit_text(&p, &format!("{}  {}", icon::FILE, shown.display()), &font(12.0), TEXT_DIM, r.width() - 34.0) {
                            ui.painter().galley(pos2(r.min.x + 24.0, r.min.y + 3.0), t, TEXT_DIM);
                        }
                        let resp = resp.on_hover_text(format!("{}\nClick to show in the map, double-click to open the folder", path.display()));
                        if resp.double_clicked() {
                            action = Some((n, true));
                        } else if resp.clicked() {
                            action = Some((n, false));
                        }
                    }
                    ui.add_space(4.0);
                }
            }
        });
        if let Some(gi) = toggle {
            if !self.dupes.open.remove(&gi) {
                self.dupes.open.insert(gi);
            }
        }
        // highlight all copies of the hovered group in the treemap
        self.panel.highlight = match (hovered_node, hovered_group) {
            (Some(n), _) => vec![n],
            (None, Some(gi)) => self.dupes.result.as_ref().map(|r| r.groups[gi].nodes.clone()).unwrap_or_default(),
            _ => Vec::new(),
        };
        if let Some((n, open)) = action {
            let tree = self.tree.as_ref().unwrap();
            if open {
                util::show_in_file_manager(&tree.path(n));
            } else {
                let parent = tree.nodes[n as usize].parent;
                if parent != NONE {
                    self.go(parent);
                }
                self.selected = Some(Target::Node(n));
            }
        }
    }

    // ------------------------------------------------------------ changes

    /// Called after a complete scan: save its summary in the background and
    /// list the earlier ones.
    pub(super) fn record_history(&mut self) {
        let Some(tree) = &self.tree else { return };
        let time = util::now_secs();
        let snap = history::from_tree(tree, time, self.scanned_elevated);
        let root = tree.root_path.clone();
        std::thread::spawn(move || {
            let _ = history::save(&root, &snap);
        });
        self.changes = ChangesState::default();
        self.changes.entries = history::list(&tree.root_path)
            .into_iter()
            .filter(|e| e.time != time)
            .collect();
    }

    fn ensure_loaded(&mut self) {
        let Some(entry) = self.changes.entries.get(self.changes.selected) else { return };
        if self.changes.loaded.as_ref().map(|(f, _)| f == &entry.file).unwrap_or(false) {
            return;
        }
        match history::load(&entry.file) {
            Ok(s) => {
                self.changes.loaded = Some((entry.file.clone(), s));
                self.changes.error = None;
            }
            Err(e) => {
                self.changes.loaded = None;
                self.changes.error = Some(format!("Could not read the earlier scan: {e}"));
            }
        }
    }

    pub(super) fn changes_panel(&mut self, ui: &mut Ui) {
        ui.add_space(4.0);
        if self.changes.entries.is_empty() {
            let path = self.tree.as_ref().map(|t| t.root_path.display().to_string()).unwrap_or_default();
            ui.label(RichText::new(format!("{}  No earlier scan of {path} yet", icon::CLOCK)).color(TEXT));
            ui.add_space(4.0);
            ui.label(
                RichText::new("After each complete scan, Spacemapper keeps a summary of the folder sizes. Scan this folder again another day to see what grew or shrank.")
                    .size(12.0)
                    .color(TEXT_DIM),
            );
            return;
        }
        // which earlier scan
        let mut sel = self.changes.selected;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Compared with").size(12.5).color(TEXT_FAINT));
            egui::ComboBox::from_id_salt("history")
                .selected_text(util::fmt_date(self.changes.entries[sel].time))
                .show_ui(ui, |ui| {
                    for (i, e) in self.changes.entries.iter().enumerate() {
                        ui.selectable_value(&mut sel, i, util::fmt_date(e.time));
                    }
                });
        });
        self.changes.selected = sel;
        self.ensure_loaded();
        if let Some(err) = &self.changes.error {
            ui.label(RichText::new(format!("{}  {err}", icon::WARNING)).color(WARN));
            return;
        }
        let Some((_, snap)) = &self.changes.loaded else { return };
        let Some(tree) = &self.tree else { return };
        if snap.full_access != self.scanned_elevated {
            ui.label(
                RichText::new(format!(
                    "{}  The earlier scan {} full access, so system folders may differ.",
                    icon::INFO,
                    if snap.full_access { "used" } else { "did not use" }
                ))
                .size(11.5)
                .color(TEXT_FAINT),
            );
        }

        // folder sizes now vs. then, unfiltered and on disk
        let vals = &self.history_vals;
        let base = history::rel_path(tree, self.view_root);
        let child_depth = if base.is_empty() { 1 } else { base.split('/').count() + 1 };
        let join = |name: &str| if base.is_empty() { name.to_string() } else { format!("{base}/{name}") };
        let now_total = vals.size[self.view_root as usize];
        let before_total = snap.sizes.get(&base).copied();
        let mut rows: Vec<ChangeRow> = Vec::new();
        let mut seen = HashSet::new();
        let mut dirs_now = 0u64;
        for &c in &tree.nodes[self.view_root as usize].children {
            let n = &tree.nodes[c as usize];
            if !n.is_dir() {
                continue;
            }
            let key = join(&n.name);
            dirs_now += vals.size[c as usize];
            seen.insert(key.clone());
            rows.push(ChangeRow { name: n.name.to_string(), node: Some(c), now: vals.size[c as usize], before: snap.sizes.get(&key).copied() });
        }
        // folders that existed then and are gone now
        let prefix = if base.is_empty() { String::new() } else { format!("{base}/") };
        let mut dirs_before = 0u64;
        for (k, &v) in &snap.sizes {
            if let Some(rest) = k.strip_prefix(&prefix) {
                if !rest.is_empty() && !rest.contains('/') && (!base.is_empty() || !k.is_empty()) {
                    dirs_before += v;
                    if !seen.contains(k) {
                        rows.push(ChangeRow { name: rest.to_string(), node: None, now: 0, before: Some(v) });
                    }
                }
            }
        }
        // files directly in this folder
        if let Some(bt) = before_total {
            let files_now = now_total.saturating_sub(dirs_now);
            let files_before = bt.saturating_sub(dirs_before);
            if (files_now as i64 - files_before as i64).abs() >= NOISE {
                rows.push(ChangeRow { name: "Files in this folder".into(), node: None, now: files_now, before: Some(files_before) });
            }
        }
        rows.sort_by_key(|r| std::cmp::Reverse(r.delta().unsigned_abs()));

        ui.add_space(6.0);
        match before_total {
            Some(b) => {
                let d = now_total as i64 - b as i64;
                ui.horizontal(|ui| {
                    ui.label(RichText::new(fmt_size(now_total)).font(font_bold(15.0)).color(TEXT));
                    ui.label(RichText::new(fmt_delta(d)).font(font_bold(15.0)).color(delta_color(d)));
                });
                ui.label(RichText::new(format!("was {}", fmt_size(b))).size(11.5).color(TEXT_FAINT));
            }
            None => {
                ui.label(RichText::new("This folder was not part of the earlier scan, or was too small to be saved.").size(12.0).color(TEXT_DIM));
            }
        }
        ui.add_space(4.0);

        let mut go_to = None;
        let mut hovered = None;
        egui::ScrollArea::vertical().id_salt("changes").auto_shrink([false, false]).show_rows(ui, 38.0, rows.len(), |ui, range| {
            for r in &rows[range] {
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::click());
                let p = ui.painter().clone();
                if resp.hovered() {
                    p.rect_filled(rect, CornerRadius::same(7), SURFACE2);
                    hovered = r.node;
                }
                let d = r.delta();
                let dg = p.layout_no_wrap(fmt_delta(d), font_bold(12.5), delta_color(d));
                let right = rect.max.x - 8.0;
                p.galley(pos2(right - dg.size().x, rect.min.y + 4.0), dg.clone(), delta_color(d));
                let state = match (r.before, r.node) {
                    (None, _) if child_depth <= history::ALWAYS_DEPTH => "new".to_string(),
                    (None, _) => "was under 1 MB".to_string(),
                    (Some(_), None) if r.name != "Files in this folder" => "removed".to_string(),
                    _ => fmt_size(r.now),
                };
                let sg = p.layout_no_wrap(state, font(11.0), TEXT_FAINT);
                p.galley(pos2(right - sg.size().x, rect.min.y + 20.0), sg, TEXT_FAINT);
                let w = rect.width() - 24.0 - dg.size().x.max(60.0);
                let ic = if r.node.is_some() || r.name != "Files in this folder" { icon::FOLDER_SIMPLE } else { icon::FILES };
                if let Some(t) = fit_text(&p, &format!("{ic}  {}", r.name), &font(13.0), if r.node.is_some() { TEXT } else { TEXT_DIM }, w) {
                    p.galley(pos2(rect.min.x + 8.0, rect.min.y + 3.0), t, TEXT);
                }
                if let Some(b) = r.before {
                    if let Some(t) = fit_text(&p, &format!("was {}", fmt_size(b)), &font(11.0), TEXT_FAINT, w - 18.0) {
                        p.galley(pos2(rect.min.x + 26.0, rect.min.y + 20.0), t, TEXT_FAINT);
                    }
                }
                if let Some(n) = r.node {
                    if resp.double_clicked() {
                        go_to = Some(n);
                    } else if resp.clicked() {
                        self.selected = Some(Target::Node(n));
                    }
                }
            }
        });
        self.panel.hovered = hovered;
        if let Some(n) = go_to {
            self.go(n);
        }
    }
}
