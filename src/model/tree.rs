//! Compact arena file tree. Children of a directory are stored contiguously,
//! names live in one shared byte buffer, so millions of entries stay cheap.

use serde::{Deserialize, Serialize};

pub type NodeId = u32;
pub const NONE: NodeId = u32::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Kind {
    Dir,
    File,
    Symlink,
    /// Device nodes, sockets, fifos.
    Special,
}

pub mod flags {
    /// Directory could not be opened (EACCES etc.); its contents are unknown.
    pub const DENIED: u8 = 1;
    /// A hardlink whose inode was already counted elsewhere; excluded from totals.
    pub const HARDLINK_DUP: u8 = 1 << 1;
    /// Another filesystem is mounted here; contents are under a different root.
    pub const MOUNTPOINT: u8 = 1 << 2;
    /// Allocated size is meaningfully smaller than apparent size.
    pub const SPARSE: u8 = 1 << 3;
    /// Some other error occurred reading this entry.
    pub const ERROR: u8 = 1 << 4;
    /// Some descendant is DENIED or ERROR.
    pub const INCOMPLETE: u8 = 1 << 5;
    /// Files with nlink > 1 (first occurrence, counted).
    pub const HARDLINK: u8 = 1 << 6;
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct FNode {
    pub name_off: u32,
    pub name_len: u16,
    pub kind: Kind,
    pub flags: u8,
    pub parent: NodeId,
    pub child_start: NodeId,
    pub child_count: u32,
    /// Bytes for files, aggregated for directories (excluding HARDLINK_DUP).
    pub apparent: u64,
    pub alloc: u64,
    /// Number of entries in this subtree, including itself.
    pub items: u32,
    /// Unix seconds; for directories the newest mtime in the subtree.
    pub mtime: i64,
}

impl FNode {
    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }
    pub fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileTree {
    pub nodes: Vec<FNode>,
    pub names: Vec<u8>,
    /// One root per scanned filesystem; root names are absolute mount paths.
    pub roots: Vec<NodeId>,
}

impl FileTree {
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn node(&self, id: NodeId) -> &FNode {
        &self.nodes[id as usize]
    }

    pub fn name_bytes(&self, id: NodeId) -> &[u8] {
        let n = self.node(id);
        &self.names[n.name_off as usize..n.name_off as usize + n.name_len as usize]
    }

    pub fn name(&self, id: NodeId) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(self.name_bytes(id))
    }

    pub fn children(&self, id: NodeId) -> std::ops::Range<NodeId> {
        let n = self.node(id);
        if n.child_count == 0 {
            return 0..0;
        }
        n.child_start..n.child_start + n.child_count
    }

    pub fn child_by_name(&self, id: NodeId, name: &[u8]) -> Option<NodeId> {
        self.children(id).find(|&c| self.name_bytes(c) == name)
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        let p = self.node(id).parent;
        (p != NONE).then_some(p)
    }

    pub fn root_of(&self, mut id: NodeId) -> NodeId {
        while let Some(p) = self.parent(id) {
            id = p;
        }
        id
    }

    pub fn ancestors(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        std::iter::successors(Some(id), move |&n| self.parent(n))
    }

    pub fn path(&self, id: NodeId) -> String {
        let mut parts: Vec<NodeId> = self.ancestors(id).collect();
        parts.reverse();
        let mut s = String::new();
        for (i, p) in parts.iter().enumerate() {
            let name = self.name(*p);
            if i == 0 {
                s.push_str(&name);
            } else {
                if !s.ends_with('/') {
                    s.push('/');
                }
                s.push_str(&name);
            }
        }
        s
    }

    pub fn push_name(&mut self, name: &[u8]) -> (u32, u16) {
        let off = self.names.len() as u32;
        let len = name.len().min(u16::MAX as usize);
        self.names.extend_from_slice(&name[..len]);
        (off, len as u16)
    }
}
