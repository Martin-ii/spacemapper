// SPDX-License-Identifier: GPL-3.0-or-later
//! Read-only parallel directory scanner.
//!
//! Uses only `read_dir` and `lstat` (via DirEntry::metadata). Symlinks are
//! never followed, hardlinked files are counted once, and the scan stays on
//! the filesystem it started on (btrfs subvolumes of the same device count as
//! the same filesystem) unless "cross filesystems" is enabled. Kernel pseudo
//! filesystems (/proc, /sys, ...) are never entered.

use crate::util::{self, Mount};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Dir,
    File,
    Link,
    Other,
    Free,
    Unknown,
}

pub const F_DENIED: u8 = 1; // directory could not be read
pub const F_MOUNT: u8 = 2; // mount point of another filesystem, not entered
pub const F_HARDLINK_DUP: u8 = 4; // file already counted through another hardlink
pub const F_SNAPSHOTS: u8 = 8; // btrfs snapshot folder, not entered (would count data many times)

pub struct TNode {
    pub name: Box<str>,
    pub kind: Kind,
    pub alloc: u64,
    pub apparent: u64,
    pub mtime: i64,
    /// inode change time: when the file was written to this disk or its
    /// metadata last changed (install, copy, extract, chmod, rename)
    pub ctime: i64,
    /// last access time (only meaningful without the noatime mount option)
    pub atime: i64,
    pub flags: u8,
    pub children: Vec<TNode>,
}

#[derive(Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub bytes: AtomicU64,
    pub errors: AtomicU64,
    pub skipped_mounts: AtomicU64,
    pub cancel: AtomicBool,
    pub current: Mutex<String>,
}

struct Ctx {
    progress: Arc<Progress>,
    seen: Mutex<HashSet<(u64, u64)>>,
    mounts: HashMap<PathBuf, Mount>,
    root_source: String,
    root_fstype: String,
    cross: bool,
    root: PathBuf,
}

impl Ctx {
    /// Called for a directory whose device differs from its parent's.
    fn may_enter(&self, p: &Path) -> bool {
        match self.mounts.get(p) {
            Some(m) => {
                if util::is_virtual_mount(m) {
                    false
                } else if self.cross {
                    true
                } else {
                    // same block device + type => e.g. a btrfs subvolume (@home)
                    m.source == self.root_source && m.fstype == self.root_fstype
                }
            }
            // device changed without a mount entry: a nested btrfs subvolume on
            // Linux; the platform decides (see `util::ENTER_UNLISTED_DEVICES`)
            None => util::ENTER_UNLISTED_DEVICES,
        }
    }
}

pub struct ScanOutput {
    pub root: Option<TNode>,
    pub error: Option<String>,
    pub cancelled: bool,
}

pub fn scan(path: &Path, cross: bool, progress: Arc<Progress>) -> ScanOutput {
    let md = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            return ScanOutput { root: None, error: Some(format!("{}: {e}", path.display())), cancelled: false };
        }
    };
    if !md.is_dir() {
        return ScanOutput { root: None, error: Some(format!("{} is not a folder", path.display())), cancelled: false };
    }
    let mounts = util::read_mounts();
    let (root_source, root_fstype) = util::mount_of(&mounts, path)
        .map(|m| (m.source.clone(), m.fstype.clone()))
        .unwrap_or_default();
    let ctx = Ctx {
        progress: progress.clone(),
        seen: Mutex::new(HashSet::new()),
        mounts: util::mount_map(&mounts),
        root_source,
        root_fstype,
        cross,
        root: path.to_path_buf(),
    };
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads((threads * 2).clamp(4, 32))
        .stack_size(16 * 1024 * 1024)
        .build()
        .expect("thread pool");
    let name: Box<str> = path.to_string_lossy().into();
    let root = pool.install(|| scan_dir(&ctx, path, name, &md));
    ScanOutput { root: Some(root), error: None, cancelled: progress.cancel.load(Relaxed) }
}

fn scan_dir(ctx: &Ctx, path: &Path, name: Box<str>, md: &fs::Metadata) -> TNode {
    let p = &ctx.progress;
    let m = util::meta(md);
    let mut node = TNode {
        name,
        kind: Kind::Dir,
        alloc: m.alloc,
        apparent: m.apparent,
        mtime: m.mtime,
        ctime: m.ctime,
        atime: m.atime,
        flags: 0,
        children: Vec::new(),
    };
    let nd = p.dirs.fetch_add(1, Relaxed);
    p.bytes.fetch_add(node.alloc, Relaxed);
    if nd % 64 == 0 {
        if let Ok(mut cur) = p.current.try_lock() {
            *cur = path.to_string_lossy().into_owned();
        }
    }
    if p.cancel.load(Relaxed) {
        return node;
    }
    let rd = match fs::read_dir(path) {
        Ok(r) => r,
        Err(_) => {
            node.flags |= F_DENIED;
            p.errors.fetch_add(1, Relaxed);
            return node;
        }
    };
    let dev = m.dev;
    let mut subdirs: Vec<(PathBuf, Box<str>, fs::Metadata)> = Vec::new();
    for ent in rd {
        let Ok(ent) = ent else {
            p.errors.fetch_add(1, Relaxed);
            continue;
        };
        let name: Box<str> = ent.file_name().to_string_lossy().into();
        // DirEntry::metadata does NOT follow symlinks (lstat semantics)
        let Ok(emd) = ent.metadata() else {
            p.errors.fetch_add(1, Relaxed);
            continue;
        };
        let ft = emd.file_type();
        if ft.is_dir() {
            subdirs.push((ent.path(), name, emd));
            continue;
        }
        let kind = if ft.is_symlink() {
            Kind::Link
        } else if ft.is_file() {
            Kind::File
        } else {
            Kind::Other
        };
        let em = util::meta(&emd);
        let mut alloc = em.alloc;
        let mut apparent = em.apparent;
        let mut flags = 0;
        if em.nlink > 1 && kind == Kind::File {
            let fresh = ctx.seen.lock().map(|mut s| s.insert((em.dev, em.ino))).unwrap_or(true);
            if !fresh {
                alloc = 0;
                apparent = 0;
                flags |= F_HARDLINK_DUP;
            }
        }
        p.files.fetch_add(1, Relaxed);
        p.bytes.fetch_add(alloc, Relaxed);
        node.children.push(TNode {
            name,
            kind,
            alloc,
            apparent,
            mtime: em.mtime,
            ctime: em.ctime,
            atime: em.atime,
            flags,
            children: Vec::new(),
        });
    }

    let sub: Vec<TNode> = subdirs
        .into_par_iter()
        .map(|(sp, sname, smd)| {
            let sm = util::meta(&smd);
            if is_snapshot_dir(&sname, sm.ino) {
                // Snapshots share their data with the live system; scanning them
                // would count the same files again for every snapshot.
                TNode {
                    name: sname,
                    kind: Kind::Dir,
                    alloc: 0,
                    apparent: 0,
                    mtime: sm.mtime,
                    ctime: sm.ctime,
                    atime: sm.atime,
                    flags: F_SNAPSHOTS,
                    children: Vec::new(),
                }
            } else if util::is_system_volume_dir(&ctx.root, &sp) || (sm.dev != dev && !ctx.may_enter(&sp)) {
                p.skipped_mounts.fetch_add(1, Relaxed);
                TNode {
                    name: sname,
                    kind: Kind::Dir,
                    alloc: 0,
                    apparent: 0,
                    mtime: sm.mtime,
                    ctime: sm.ctime,
                    atime: sm.atime,
                    flags: F_MOUNT,
                    children: Vec::new(),
                }
            } else {
                scan_dir(ctx, &sp, sname, &smd)
            }
        })
        .collect();
    node.children.extend(sub);
    node
}

/// btrfs subvolume roots always have inode 256; snapper / timeshift keep their
/// snapshots in subvolumes with these names.
fn is_snapshot_dir(name: &str, ino: u64) -> bool {
    matches!(name, ".snapshots" | "timeshift-btrfs" | "@snapshots") && ino == 256
}

// ---------------------------------------------------------------- full access
//
// For a "full access" scan the GUI starts a second copy of this program via
// `pkexec` (the system password prompt; on macOS `sudo` with a password
// dialog, see `elevate`). That copy only runs the read-only
// scanner above and streams the result back through a pipe; it never writes
// any file. The GUI itself keeps running as your normal user.

use std::io::{self, BufWriter, Read, Write};
#[cfg(unix)]
use std::io::{BufRead, BufReader};
#[cfg(unix)]
use std::process::{Command, Stdio};

fn write_tree(w: &mut impl Write, n: &TNode) -> io::Result<()> {
    let kind = match n.kind {
        Kind::Dir => 0u8,
        Kind::File => 1,
        Kind::Link => 2,
        _ => 3,
    };
    w.write_all(&[kind, n.flags])?;
    w.write_all(&n.alloc.to_le_bytes())?;
    w.write_all(&n.apparent.to_le_bytes())?;
    w.write_all(&n.mtime.to_le_bytes())?;
    w.write_all(&n.ctime.to_le_bytes())?;
    w.write_all(&n.atime.to_le_bytes())?;
    w.write_all(&(n.name.len() as u32).to_le_bytes())?;
    w.write_all(n.name.as_bytes())?;
    w.write_all(&(n.children.len() as u32).to_le_bytes())?;
    for c in &n.children {
        write_tree(w, c)?;
    }
    Ok(())
}

#[cfg(unix)]
fn read_exact<const N: usize>(r: &mut impl Read) -> io::Result<[u8; N]> {
    let mut b = [0u8; N];
    r.read_exact(&mut b)?;
    Ok(b)
}

#[cfg(unix)]
fn read_tree(r: &mut impl Read) -> io::Result<TNode> {
    let [kind, flags] = read_exact::<2>(r)?;
    let alloc = u64::from_le_bytes(read_exact(r)?);
    let apparent = u64::from_le_bytes(read_exact(r)?);
    let mtime = i64::from_le_bytes(read_exact(r)?);
    let ctime = i64::from_le_bytes(read_exact(r)?);
    let atime = i64::from_le_bytes(read_exact(r)?);
    let nlen = u32::from_le_bytes(read_exact(r)?) as usize;
    if nlen > 65536 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad name length"));
    }
    let mut name = vec![0u8; nlen];
    r.read_exact(&mut name)?;
    let count = u32::from_le_bytes(read_exact(r)?) as usize;
    let mut children = Vec::with_capacity(count.min(1 << 16));
    for _ in 0..count {
        children.push(read_tree(r)?);
    }
    Ok(TNode {
        name: String::from_utf8_lossy(&name).into(),
        kind: match kind {
            0 => Kind::Dir,
            1 => Kind::File,
            2 => Kind::Link,
            _ => Kind::Other,
        },
        alloc,
        apparent,
        mtime,
        ctime,
        atime,
        flags,
        children,
    })
}

/// Entry point of the privileged helper (`spacemapper --scan-helper PATH [--cross]`).
pub fn run_helper(path: &Path, cross: bool) -> i32 {
    let progress = Arc::new(Progress::default());
    // the GUI closes our stdin to ask us to stop
    {
        let p = progress.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            let mut stdin = io::stdin();
            while let Ok(n) = stdin.read(&mut buf) {
                if n == 0 {
                    break;
                }
            }
            p.cancel.store(true, Relaxed);
        });
    }
    // progress lines on stderr
    let done = Arc::new(AtomicBool::new(false));
    {
        let p = progress.clone();
        let done = done.clone();
        std::thread::spawn(move || {
            let mut err = io::stderr();
            while !done.load(Relaxed) {
                let cur = p.current.lock().map(|c| c.clone()).unwrap_or_default();
                let _ = writeln!(
                    err,
                    "P {} {} {} {}\t{}",
                    p.files.load(Relaxed),
                    p.dirs.load(Relaxed),
                    p.bytes.load(Relaxed),
                    p.errors.load(Relaxed),
                    cur.replace('\n', " ")
                );
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
        });
    }
    let out = scan(path, cross, progress.clone());
    done.store(true, Relaxed);
    let Some(root) = out.root else {
        eprintln!("E {}", out.error.unwrap_or_default());
        return 2;
    };
    let mut w = BufWriter::with_capacity(1 << 20, io::stdout().lock());
    let flag = if progress.cancel.load(Relaxed) { 1u8 } else { 0u8 };
    if w.write_all(b"SPCM2").and_then(|_| w.write_all(&[flag])).and_then(|_| write_tree(&mut w, &root)).and_then(|_| w.flush()).is_err() {
        return 3;
    }
    0
}

/// On Windows the whole program is restarted as administrator instead (see
/// util::relaunch_as_admin), so a normal scan already has full access.
#[cfg(windows)]
pub fn scan_elevated(path: &Path, cross: bool, progress: Arc<Progress>) -> ScanOutput {
    scan(path, cross, progress)
}

/// The command that starts this program with administrator rights:
/// `pkexec`, which shows the system password prompt.
#[cfg(all(unix, not(target_os = "macos")))]
fn elevate(exe: &Path) -> Command {
    let mut cmd = Command::new("pkexec");
    // Inside an AppImage the running binary lives on a FUSE mount that only
    // the current user can access, so root has to start the AppImage file
    // itself. Extracting instead of mounting avoids needing FUSE as root.
    match std::env::var_os("APPIMAGE").map(PathBuf::from) {
        Some(appimage) if appimage.is_file() => {
            cmd.arg(appimage).arg("--appimage-extract-and-run");
        }
        _ => {
            cmd.arg(exe);
        }
    }
    cmd
}

/// macOS has no pkexec. `sudo -A` asks for the password through the program
/// in SUDO_ASKPASS, which is this program again: with SPACEMAPPER_ASKPASS
/// set it only shows a password dialog (see `util::askpass`). `-k` asks
/// every time, like pkexec.
#[cfg(target_os = "macos")]
fn elevate(exe: &Path) -> Command {
    let mut cmd = Command::new("/usr/bin/sudo");
    cmd.env("SUDO_ASKPASS", exe).env("SPACEMAPPER_ASKPASS", "1");
    cmd.args(["-A", "-k", "--"]).arg(exe);
    cmd
}

/// Run a scan through `pkexec` (macOS: `sudo`) with administrator rights.
/// Blocks until done.
#[cfg(unix)]
pub fn scan_elevated(path: &Path, cross: bool, progress: Arc<Progress>) -> ScanOutput {
    let fail = |msg: String| ScanOutput { root: None, error: Some(msg), cancelled: false };
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => return fail(format!("Could not locate the spacemapper executable: {e}")),
    };
    let mut cmd = elevate(&exe);
    cmd.arg("--scan-helper").arg(path);
    if cross {
        cmd.arg("--cross");
    }
    let mut child = match cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => return fail(format!("Could not start {}: {e}", if cfg!(target_os = "macos") { "sudo" } else { "pkexec" })),
    };
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    // progress reader
    let last_err = Arc::new(Mutex::new(String::new()));
    let err_thread = {
        let p = progress.clone();
        let last_err = last_err.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Some(rest) = line.strip_prefix("P ") {
                    let (nums, cur) = rest.split_once('\t').unwrap_or((rest, ""));
                    let v: Vec<u64> = nums.split(' ').filter_map(|x| x.parse().ok()).collect();
                    if v.len() == 4 {
                        p.files.store(v[0], Relaxed);
                        p.dirs.store(v[1], Relaxed);
                        p.bytes.store(v[2], Relaxed);
                        p.errors.store(v[3], Relaxed);
                    }
                    if let Ok(mut c) = p.current.lock() {
                        *c = cur.to_string();
                    }
                } else if !line.trim().is_empty() {
                    if let Ok(mut e) = last_err.lock() {
                        *e = line.strip_prefix("E ").unwrap_or(&line).to_string();
                    }
                }
            }
        })
    };
    // cancel watcher: closing stdin tells the helper to stop
    let finished = Arc::new(AtomicBool::new(false));
    {
        let p = progress.clone();
        let finished = finished.clone();
        std::thread::spawn(move || {
            let mut stdin = stdin;
            while !finished.load(Relaxed) {
                if p.cancel.load(Relaxed) {
                    stdin.take();
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        });
    }

    let mut r = BufReader::with_capacity(1 << 20, stdout);
    let mut magic = [0u8; 6];
    let result = r.read_exact(&mut magic).and_then(|_| {
        if &magic[..5] != b"SPCM2" {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected helper output"));
        }
        read_tree(&mut r)
    });
    finished.store(true, Relaxed);
    let status = child.wait();
    let _ = err_thread.join();
    match result {
        Ok(root) => ScanOutput { root: Some(root), error: None, cancelled: magic[5] == 1 },
        Err(_) => {
            let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
            let detail = last_err.lock().map(|e| e.clone()).unwrap_or_default();
            let msg = match code {
                126 if cfg!(not(target_os = "macos")) => "Authentication cancelled.".to_string(),
                127 if cfg!(not(target_os = "macos")) => "Authentication failed. A polkit agent (e.g. hyprpolkitagent) must be running.".to_string(),
                // sudo itself failed: "no password was provided", "incorrect password attempts"
                1 if cfg!(target_os = "macos") && detail.starts_with("sudo:") => "Authentication cancelled or failed.".to_string(),
                _ if !detail.is_empty() => detail,
                _ => format!("Scan failed (exit code {code})."),
            };
            fail(msg)
        }
    }
}
