//! In-memory flat file system: path -> bytes. Contents are lost on reboot.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::rt::KCell;

/// Sorted by name, so listings are stable.
static FILES: KCell<BTreeMap<Vec<u8>, Vec<u8>>> = KCell::new(BTreeMap::new());

/// "/a", "./a" and "a" are the same file.
fn norm(mut path: &[u8]) -> &[u8] {
    loop {
        if let Some(rest) = path.strip_prefix(b"/") {
            path = rest;
        } else if let Some(rest) = path.strip_prefix(b"./") {
            path = rest;
        } else {
            return path;
        }
    }
}

pub fn read(path: &[u8]) -> Option<Vec<u8>> {
    FILES.with(|f| f.get(norm(path)).cloned())
}

pub fn write(path: &[u8], data: &[u8]) -> bool {
    let path = norm(path);
    if path.is_empty() || path.len() > 200 || path.contains(&0) {
        return false;
    }
    let mut copy = Vec::new();
    if copy.try_reserve_exact(data.len()).is_err() {
        return false; // out of memory: the old contents stay
    }
    copy.extend_from_slice(data);
    FILES.with(|f| f.insert(path.to_vec(), copy));
    true
}

pub fn delete(path: &[u8]) -> bool {
    FILES.with(|f| f.remove(norm(path)).is_some())
}

pub fn rename(from: &[u8], to: &[u8]) -> bool {
    let Some(data) = read(from) else { return false };
    if !write(to, &data) {
        return false;
    }
    if norm(from) != norm(to) {
        delete(from);
    }
    true
}

/// Names and sizes of all files.
pub fn list() -> Vec<(Vec<u8>, usize)> {
    FILES.with(|f| f.iter().map(|(name, data)| (name.clone(), data.len())).collect())
}
