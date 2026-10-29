// SPDX-License-Identifier: GPL-3.0-or-later
//! Scan history for "changes since last scan".
//!
//! After a complete scan, the size of every folder (above a small minimum) is
//! saved to ~/.local/state/spacemapper/history/ (macOS: ~/Library/Application
//! Support/Spacemapper/history/). This is Spacemapper's own data;
//! nothing is written next to the scanned files. The newest 10 summaries per
//! scanned folder are kept.

use crate::tree::{SizeMode, Tree, NONE};
use crate::filter::Filter;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use xxhash_rust::xxh3::xxh3_64;

const MAGIC: &[u8; 6] = b"SMHIS1";
const KEEP: usize = 10;
/// folders smaller than this are not stored (unless near the top)
const MIN_SIZE: u64 = 1 << 20;
pub const ALWAYS_DEPTH: usize = 2;

pub struct Snapshot {
    pub time: i64,
    pub full_access: bool,
    /// folder path relative to the scanned root ("" = the root) -> bytes on disk
    pub sizes: HashMap<String, u64>,
}

/// One saved summary, without its contents loaded.
#[derive(Clone)]
pub struct Entry {
    pub file: PathBuf,
    pub time: i64,
}

pub fn state_dir() -> PathBuf {
    crate::util::state_dir().join("history")
}

fn key(root: &Path) -> String {
    format!("{:016x}", xxh3_64(root.as_os_str().as_encoded_bytes()))
}

/// Saved summaries for this folder, newest first.
pub fn list(root: &Path) -> Vec<Entry> {
    let prefix = format!("{}-", key(root));
    let mut v: Vec<Entry> = fs::read_dir(state_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let time = name.strip_prefix(&prefix)?.strip_suffix(".bin")?.parse().ok()?;
            Some(Entry { file: e.path(), time })
        })
        .collect();
    v.sort_by_key(|e| std::cmp::Reverse(e.time));
    v
}

/// Folder sizes of a finished scan (unfiltered, bytes on disk).
pub fn from_tree(tree: &Tree, time: i64, full_access: bool) -> Snapshot {
    let vals = tree.compute_values(SizeMode::OnDisk, &Filter::default(), false, false);
    let mut sizes = HashMap::new();
    // depth-first with the relative path of each folder
    let mut stack: Vec<(u32, String, usize)> = vec![(tree.root, String::new(), 0)];
    while let Some((id, path, depth)) = stack.pop() {
        let size = vals.size[id as usize];
        if depth > ALWAYS_DEPTH && size < MIN_SIZE {
            continue;
        }
        for &c in &tree.nodes[id as usize].children {
            let n = &tree.nodes[c as usize];
            if n.is_dir() {
                let p = if path.is_empty() { n.name.to_string() } else { format!("{path}/{}", n.name) };
                stack.push((c, p, depth + 1));
            }
        }
        sizes.insert(path, size);
    }
    Snapshot { time, full_access, sizes }
}

fn write_snapshot(w: &mut impl Write, root: &Path, s: &Snapshot) -> io::Result<()> {
    w.write_all(MAGIC)?;
    w.write_all(&s.time.to_le_bytes())?;
    w.write_all(&[s.full_access as u8])?;
    let r = root.as_os_str().as_encoded_bytes();
    w.write_all(&(r.len() as u32).to_le_bytes())?;
    w.write_all(r)?;
    w.write_all(&(s.sizes.len() as u32).to_le_bytes())?;
    for (p, size) in &s.sizes {
        w.write_all(&(p.len() as u32).to_le_bytes())?;
        w.write_all(p.as_bytes())?;
        w.write_all(&size.to_le_bytes())?;
    }
    Ok(())
}

fn take<const N: usize>(r: &mut impl Read) -> io::Result<[u8; N]> {
    let mut b = [0u8; N];
    r.read_exact(&mut b)?;
    Ok(b)
}

fn bad() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "not a Spacemapper history file")
}

pub fn load(file: &Path) -> io::Result<Snapshot> {
    let data = fs::read(file)?;
    let r = &mut &data[..];
    if &take::<6>(r)? != MAGIC {
        return Err(bad());
    }
    let time = i64::from_le_bytes(take(r)?);
    let full_access = take::<1>(r)?[0] != 0;
    let rlen = u32::from_le_bytes(take(r)?) as usize;
    if rlen > r.len() {
        return Err(bad());
    }
    *r = &r[rlen..];
    let n = u32::from_le_bytes(take(r)?) as usize;
    let mut sizes = HashMap::with_capacity(n.min(1 << 20));
    for _ in 0..n {
        let len = u32::from_le_bytes(take(r)?) as usize;
        if len > r.len() {
            return Err(bad());
        }
        let p = String::from_utf8_lossy(&r[..len]).into_owned();
        *r = &r[len..];
        let size = u64::from_le_bytes(take(r)?);
        sizes.insert(p, size);
    }
    Ok(Snapshot { time, full_access, sizes })
}

/// Save a summary and remove the oldest ones beyond the limit.
pub fn save(root: &Path, s: &Snapshot) -> io::Result<()> {
    let dir = state_dir();
    fs::create_dir_all(&dir)?;
    let file = dir.join(format!("{}-{}.bin", key(root), s.time));
    let tmp = file.with_extension("tmp");
    {
        let mut w = io::BufWriter::new(fs::File::create(&tmp)?);
        write_snapshot(&mut w, root, s)?;
        w.flush()?;
    }
    fs::rename(&tmp, &file)?;
    for old in list(root).into_iter().skip(KEEP) {
        // only ever removes Spacemapper's own history files
        let _ = fs::remove_file(old.file);
    }
    Ok(())
}

/// Relative path of a node below the tree root, as stored in snapshots.
pub fn rel_path(tree: &Tree, id: u32) -> String {
    let chain = tree.chain(id);
    let mut parts = Vec::new();
    for &c in chain.iter().skip(1) {
        if c == NONE {
            break;
        }
        parts.push(&*tree.nodes[c as usize].name);
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut sizes = HashMap::new();
        sizes.insert(String::new(), 100);
        sizes.insert("a/b c".to_string(), 42);
        let s = Snapshot { time: 1234, full_access: true, sizes };
        let mut buf = Vec::new();
        write_snapshot(&mut buf, Path::new("/home/x"), &s).unwrap();
        let file = std::env::temp_dir().join(format!("sm-hist-{}.bin", std::process::id()));
        fs::write(&file, &buf).unwrap();
        let back = load(&file).unwrap();
        fs::remove_file(&file).ok();
        assert_eq!(back.time, 1234);
        assert!(back.full_access);
        assert_eq!(back.sizes.get("a/b c"), Some(&42));
        assert_eq!(back.sizes.get(""), Some(&100));
    }
}
