// This file contains definitions for data structures.

use std::collections::{BTreeMap, HashMap};
use std::ops::AddAssign;
use std::fs::{File, Permissions};
use std::rc::Rc;
use std::ffi::OsString;

use git2::Oid;
use time::Timespec;
use fuse::FileType;

#[macro_use]
extern crate log;

pub mod gitfs;
pub mod macros;


#[derive(Debug, Copy, Clone, Ord, PartialOrd, Eq, PartialEq)]
pub struct Ino(u64);

impl Ino {
    const ROOT: Ino = Ino(1);
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
}


// TODO: ghost dirs
#[derive(Debug, Clone)]
pub enum Entry {
    Directory {
        /// an OID pointing to the tree object
        oid: Oid,
        /// The name of root (toplevel) is an empty string.
        name: OsString,
        ctime: Timespec,
        atime: Timespec,
        mtime: Timespec,
        /// Files under this directory.
        /// None means the tree is not traversed.
        /// `opendir` will set it to Some.
        children: Option<HashMap<OsString, Ino>>,
    },
    CleanFile {
        /// an OID pointing to the blob object
        oid: Oid,
        parent: Ino,
        size: usize,
        perm: Permissions,
        ctime: Timespec,
        atime: Timespec,
        mtime: Timespec,
    },
    DirtyFile {
        /// The actual file on disk.
        file: Rc<File>,
    },
}

impl From<&Entry> for FileType {
    fn from(x: &Entry) -> FileType {
        match x {
            Entry::Directory { .. } => FileType::Directory,
            Entry::CleanFile { .. } | Entry::DirtyFile { .. } => FileType::RegularFile,
        }
    }
}
