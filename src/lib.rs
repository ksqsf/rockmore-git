// This file contains definitions for data structures.

use std::collections::{BTreeMap, HashMap};
use std::ops::AddAssign;
use std::fs::{File, Permissions};
use std::path::PathBuf;
use std::ffi::{OsString, OsStr};

use git2::Oid;
use time::Timespec;
use fuse::FileType;

#[macro_use]
extern crate log;

pub mod gitfs;


#[derive(Debug, Copy, Clone, Ord, PartialOrd, Eq, PartialEq)]
pub struct Ino(u64);

impl Ino {
    const ROOT: Ino = Ino(1);

    fn is_root(self) -> bool {
        return self == Self::ROOT
    }
}

impl From<u64> for Ino {
    fn from(x: u64) -> Ino { Ino(x) }
}

impl From<Ino> for u64 {
    fn from(x: Ino) -> u64 { x.0 }
}

impl AddAssign<u64> for Ino {
    fn add_assign(&mut self, rhs: u64) {
        self.0 += rhs
    }
}


/// Collect metadata: map ino to fs entries.
///
/// Inos are allocated contiguously (1, 2, 3, ...).
#[derive(Debug)]
pub struct InoMap{
    next_ino: Ino,
    inner: BTreeMap<Ino, Entry>
}

impl InoMap {
    /// Create a new inomap. Don't forget to add entry for root!
    fn new() -> InoMap {
        InoMap {
            next_ino: Ino::ROOT,
            inner: BTreeMap::new(),
        }
    }

    /// Add an entry to inomap. Return the ino for the entry just
    /// inserted.
    fn add(&mut self, entry: Entry) -> Ino {
        let ino = self.next_ino;
        self.inner.insert(ino, entry);
        self.next_ino += 1;
        ino
    }

    fn get(&self, ino: Ino) -> Option<&Entry> {
        self.inner.get(&ino)
    }

    fn get_mut(&mut self, ino: Ino) -> Option<&mut Entry> {
        self.inner.get_mut(&ino)
    }

    fn remove(&mut self, ino: Ino) -> Option<Entry> {
        self.inner.remove(&ino)
    }

    fn next_ino(&self) -> Ino {
        self.next_ino
    }

    /// Return a fs prefix as PathBuf.
    fn prefix(&self, mut ino: Ino) -> Option<PathBuf> {
        let mut parts = vec![];
        while !ino.is_root() {
            let entry = self.get(ino)?;
            parts.push(entry.name.clone());
            ino = entry.parent;
        }

        let mut prefix = PathBuf::new();
        for part in parts.iter().rev() {
            prefix.push(part);
        }
        println!("prefix: {:?}", prefix);
        Some(prefix)
    }
}


#[derive(Debug)]
struct Entry {
    name: OsString,

    /// root.parent == root.
    parent: Ino,

    /// File status changed.
    ctime: Timespec,

    /// Last accessed.
    atime: Timespec,

    /// Data modified.
    mtime: Timespec,

    /// Created.
    crtime: Timespec,

    /// Permission bits.
    perm: Permissions,

    /// Size.
    size: u64,

    /// Entry kind.
    u: EntryKind,
}

#[derive(Debug)]
pub enum EntryKind {
    GitTree {
        /// an OID pointing to the tree object
        oid: Oid,
        /// Files under this directory.
        /// None means the tree is not traversed.
        children: Option<HashMap<OsString, Ino>>,
    },
    GitBlob {
        /// an OID pointing to the blob object
        oid: Oid,
    },
    DirtyDir {
        children: Option<HashMap<OsString, Ino>>,
    },
    DirtyFile {
        refcnt: i32,
        /// The actual file on disk.
        file: Option<File>,
    },
}

impl Entry {
    fn get_child(&self, name: &OsStr) -> Option<Ino> {
        match self.u {
            EntryKind::DirtyDir { children: Some(ref c) } => c.get(name).cloned(),
            EntryKind::GitTree { children: Some(ref c), .. } => c.get(name).cloned(),
            _ => unreachable!(),
        }
    }

    fn add_child(&mut self, name: OsString, ino: Ino) {
        match self.u {
            EntryKind::DirtyDir { children: Some(ref mut c) } => {c.insert(name, ino);}
            EntryKind::GitTree { children: Some(ref mut c), .. } => {c.insert(name, ino);}
            _ => unreachable!(),
        }
    }

    fn remove_child(&mut self, name: &OsStr) -> Option<Ino> {
        match self.u {
            EntryKind::DirtyDir { children: Some(ref mut c) } => c.remove(name),
            EntryKind::GitTree { children: Some(ref mut c), .. } => c.remove(name),
            _ => unreachable!(),
        }
    }
}

impl From<&Entry> for FileType {
    fn from(x: &Entry) -> FileType {
        match x.u {
            EntryKind::GitTree { .. } | EntryKind::DirtyDir { .. } => FileType::Directory,
            EntryKind::GitBlob { .. } | EntryKind::DirtyFile { .. } => FileType::RegularFile,
        }
    }
}
