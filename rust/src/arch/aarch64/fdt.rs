//! The device tree the boot loader passes: which devices the machine has
//! and where. Just enough of the format to find nodes and read properties.

use core::sync::atomic::{AtomicUsize, Ordering};

static ADDRESS: AtomicUsize = AtomicUsize::new(0);

/// Remembers where the tree is.
///
/// # Safety
/// `addr` must be 0 or the address of a device tree that is never changed.
pub unsafe fn set(addr: usize) {
    ADDRESS.store(addr, Ordering::Relaxed);
}

pub struct Tree {
    structure: &'static [u8],
    strings: &'static [u8],
}

/// A node of the tree: the position of its properties.
pub struct Node<'a> {
    tree: &'a Tree,
    at: usize,
}

fn be32(b: &[u8], at: usize) -> Option<usize> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?) as usize)
}

/// The tree, if the boot loader passed one.
pub fn tree() -> Option<Tree> {
    let addr = ADDRESS.load(Ordering::Relaxed);
    if addr == 0 {
        return None;
    }
    // SAFETY: see `set`; the size is taken from the tree's own header
    let header = unsafe { core::slice::from_raw_parts(addr as *const u8, 40) };
    if be32(header, 0)? != 0xD00D_FEED {
        return None;
    }
    let total = be32(header, 4)?.min(16 << 20);
    let all = unsafe { core::slice::from_raw_parts(addr as *const u8, total) };
    Some(Tree { structure: all.get(be32(header, 8)?..)?, strings: all.get(be32(header, 12)?..)? })
}

impl Tree {
    /// The memory the tree itself occupies: (address, size).
    pub fn extent(&self) -> (usize, usize) {
        let addr = ADDRESS.load(Ordering::Relaxed);
        // SAFETY: the header was checked when the tree was opened
        let header = unsafe { core::slice::from_raw_parts(addr as *const u8, 8) };
        (addr, be32(header, 4).unwrap_or(0))
    }

    /// Calls `each(name, node)` for every node, at any depth, until it
    /// returns true.
    fn walk<'a>(&'a self, mut each: impl FnMut(&[u8], Node<'a>) -> bool) {
        let s = self.structure;
        let mut at = 0;
        loop {
            let Some(token) = be32(s, at) else { return };
            at += 4;
            match token {
                1 => {
                    // a node begins: its name, NUL-terminated and padded
                    let Some(len) = s.get(at..).and_then(|rest| rest.iter().position(|&b| b == 0)) else { return };
                    let name = &s[at..at + len];
                    at = (at + len + 4) & !3;
                    if each(name, Node { tree: self, at }) {
                        return;
                    }
                }
                3 => {
                    let Some(len) = be32(s, at) else { return };
                    at = (at + 8 + len + 3) & !3;
                }
                2 | 4 => {}
                _ => return,
            }
        }
    }

    fn find<'a>(&'a self, wanted: impl Fn(&[u8], &Node) -> bool) -> Option<Node<'a>> {
        let mut found = None;
        self.walk(|name, node| {
            if wanted(name, &node) {
                found = Some(node);
            }
            found.is_some()
        });
        found
    }

    /// The "reg" address of every node that says it is compatible with
    /// `device`.
    pub fn all_compatible(&self, device: &str, mut each: impl FnMut(usize)) {
        self.walk(|_, node| {
            if node.is_compatible(device) {
                if let Some((base, _)) = node.reg(0) {
                    each(base);
                }
            }
            false
        });
    }

    /// The first node whose name starts with `prefix`.
    pub fn named(&self, prefix: &str) -> Option<Node<'_>> {
        self.find(|name, _| name.starts_with(prefix.as_bytes()))
    }

    /// The first node that says it is compatible with `device`.
    pub fn compatible(&self, device: &str) -> Option<Node<'_>> {
        self.find(|_, node| node.is_compatible(device))
    }
}

impl Node<'_> {
    fn is_compatible(&self, device: &str) -> bool {
        self.property("compatible").is_some_and(|c| c.split(|&b| b == 0).any(|s| s == device.as_bytes()))
    }

    /// The value of a property of this node.
    pub fn property(&self, name: &str) -> Option<&'static [u8]> {
        let (s, strings) = (self.tree.structure, self.tree.strings);
        let mut at = self.at;
        loop {
            match be32(s, at)? {
                3 => {
                    let (len, name_at) = (be32(s, at + 4)?, be32(s, at + 8)?);
                    let value = s.get(at + 12..at + 12 + len)?;
                    if strings.get(name_at..)?.starts_with(name.as_bytes()) && strings.get(name_at + name.len()) == Some(&0) {
                        return Some(value);
                    }
                    at = (at + 12 + len + 3) & !3;
                }
                4 => at += 4,
                _ => return None, // properties come before child nodes
            }
        }
    }

    /// Entry `index` of the node's "reg" property as (address, size),
    /// with two cells each, as these machines have at the top level.
    pub fn reg(&self, index: usize) -> Option<(usize, usize)> {
        let reg = self.property("reg")?;
        let cell = |i: usize| Some(u64::from_be_bytes(reg.get(16 * index + 8 * i..16 * index + 8 * i + 8)?.try_into().ok()?) as usize);
        Some((cell(0)?, cell(1)?))
    }
}
