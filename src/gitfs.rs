/// GitFS works as a transparent translation layer that takes any file
/// read and transforms it into a git lookup. Essentially, gitfs
/// simulates git-worktree in a lightweight way.
///
/// Due to how directories (or trees) are handled in git, gitfs
/// doesn't track whether a dir is modified or not. However, new files
/// may be created, so some new directories can appear.
///
/// Please read the source code for the details.
use std::ffi::{OsStr, OsString};
use std::fs::Permissions;
use std::os::unix::{ffi::OsStrExt, fs::PermissionsExt};
use time::Timespec;

use fuse::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyOpen,
    Request,
};
use git2::{Error as GitError, ObjectType, Oid, Repository, Tree};
use libc::{c_int, EISDIR, ENOENT, ENOTDIR};
use openat::Dir;
use std::collections::HashMap;

use crate::{Entry, EntryKind, Ino, InoMap};

macro_rules! some {
    ($value:expr, $reply:ident, $errno:expr) => {
        match $value {
            Some(val) => val,
            None => return $reply.error($errno),
        }
    };
}

pub struct GitFS {
    repo: Repository,
    #[allow(unused)]
    underlying_dir: Dir,
    inomap: InoMap,
}

// public interfaces
impl GitFS {
    pub fn new(repo: Repository, underlying_dir: Dir) -> GitFS {
        GitFS {
            repo,
            underlying_dir,
            inomap: InoMap::new(),
        }
    }
}

// file system interfaces
impl Filesystem for GitFS {
    fn init(&mut self, _req: &Request) -> Result<(), c_int> {
        let commit = self.repo.head().unwrap().peel_to_commit().unwrap();
        let tree = commit.tree().unwrap();
        self.inomap.add(self.root_entry(tree));
        info!("gitfs is mounted");
        Ok(())
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        reply: ReplyData,
    ) {
        let entry = some!(self.inomap.get(ino.into()), reply, ENOENT);
        let offset = offset as usize;
        let size = size as usize;
        match entry.u {
            EntryKind::GitBlob { oid, .. } => {
                let blob = self.repo.find_blob(oid).unwrap();
                return reply.data(&blob.content()[offset..offset + size]);
            }
            EntryKind::DirtyFile { .. } => unimplemented!(),
            EntryKind::GitTree { .. } | EntryKind::DirtyDir { .. } => return reply.error(EISDIR),
        }
    }

    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let parent_entry = some!(self.inomap.get(parent.into()), reply, ENOENT);
        match &parent_entry.u {
            EntryKind::GitTree {
                children: Some(children),
                ..
            } => {
                let child = *some!(children.get(name), reply, ENOENT);
                let child_entry = some!(self.inomap.get(child), reply, ENOENT);
                return reply.entry(&Self::ttl(), &self.make_attr(child, child_entry), 0);
            }
            EntryKind::GitTree { children: None, .. } => match self.fill_children(parent.into()) {
                Ok(_) => (),
                Err(e) => return reply.error(e),
            },
            _ => return reply.error(ENOTDIR),
        }

        // if we reachabled here, it means we have filled children of
        // a directory
        let parent_entry = some!(self.inomap.get(parent.into()), reply, ENOENT);
        match &parent_entry.u {
            EntryKind::GitTree {
                children: Some(children),
                ..
            } => {
                let child = *some!(children.get(name), reply, ENOENT);
                let child_entry = some!(self.inomap.get(child), reply, ENOENT);
                return reply.entry(&Self::ttl(), &self.make_attr(child, child_entry), 0);
            }
            EntryKind::GitTree { children: None, .. } => {
                warn!("children is empty after fill, skipping");
                return reply.error(ENOENT);
            }
            _ => return reply.error(ENOTDIR),
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        let ino = Ino::from(ino);
        let entry = some!(self.inomap.get(ino), reply, ENOENT);
        return reply.attr(&Self::ttl(), &self.make_attr(ino, entry));
    }

    fn opendir(&mut self, _req: &Request, ino: u64, _flags: u32, reply: ReplyOpen) {
        let ino = Ino::from(ino);
        match self.fill_children(ino) {
            Ok(_) => reply.opened(0, 0),
            Err(e) => reply.error(e),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let ino = Ino::from(ino);
        let entry = some!(self.inomap.get(ino), reply, ENOENT);
        match &entry.u {
            EntryKind::GitBlob { .. } | EntryKind::DirtyFile { .. } => return reply.error(ENOTDIR),
            EntryKind::GitTree {
                children: Some(children),
                ..
            } => {
                for (i, (name, &child)) in children.iter().enumerate().skip(offset as usize) {
                    if reply.add(child.into(), (i + 1) as i64, entry.into(), name) {
                        return reply.ok();
                    }
                }
                return reply.ok();
            }
            EntryKind::GitTree { children: None, .. } => {
                // readdir cannot be called before opendir
                unreachable!()
            }
            EntryKind::DirtyDir { .. } => unimplemented!(),
        }
    }
}

// private interfaces
impl GitFS {
    /// by default, ttl = 1 second
    fn ttl() -> Timespec {
        Timespec::new(1, 0)
    }

    /// Create the root entry.
    fn root_entry(&self, tree: Tree<'_>) -> Entry {
        let metadata = self.underlying_dir.self_metadata().unwrap();
        let stat = metadata.stat();
        let atime = Timespec::new(stat.st_atime, stat.st_atime_nsec as i32);
        let mtime = Timespec::new(stat.st_mtime, stat.st_mtime_nsec as i32);
        let ctime = Timespec::new(stat.st_ctime, stat.st_ctime_nsec as i32);
        let crtime = Timespec::new(stat.st_birthtime, stat.st_birthtime_nsec as i32);
        Entry {
            name: "".to_string().into(),
            parent: Ino::ROOT,
            size: 0,
            atime,
            ctime,
            mtime,
            crtime,
            perm: metadata.permissions(),
            u: EntryKind::GitTree {
                oid: tree.id(),
                children: None,
            },
        }
    }

    /// Fill the `children` field of `Entry::Directory`.
    fn fill_children(&mut self, ino: Ino) -> Result<(), c_int> {
        let dir_entry = self.inomap.get(ino).ok_or(ENOENT)?;

        // we treat directories specially becausedir_entry points to
        // inomap, but inomap should stay unchanged during our walk.
        let walk;
        match dir_entry.u {
            EntryKind::GitTree {
                oid,
                children: None,
                ..
            } => {
                walk = self.walk_tree(ino, oid).map_err(|_| ENOENT)?;
            }
            EntryKind::GitTree {
                children: Some(_), ..
            } => return Ok(()),
            _ => return Err(ENOTDIR),
        }

        // walk done, insert data to inomap so that we have inos
        let children_entries = walk
            .into_iter()
            .map(|(name, entry)| (name, self.inomap.add(entry)))
            .collect::<HashMap<OsString, Ino>>();

        // lookup dir_entry again in case it's moved
        let dir_entry = self.inomap.get_mut(ino).ok_or(ENOENT)?;
        match dir_entry.u {
            EntryKind::GitTree {
                children: ref mut c @ None,
                ..
            } => {
                c.replace(children_entries);
                return Ok(());
            }
            _ => unreachable!(),
        }
    }

    fn walk_tree(&self, ino: Ino, tree_id: Oid) -> Result<(Vec<(OsString, Entry)>), GitError> {
        let tree = self.repo.find_tree(tree_id)?;
        let mut entries = Vec::new();

        for tree_entry in tree.iter() {
            let name = OsString::from(OsStr::from_bytes(tree_entry.name_bytes()));
            let perm = Permissions::from_mode(tree_entry.filemode() as u32);
            let entry = match tree_entry.kind() {
                Some(ObjectType::Blob) => {
                    // FIXME: can this fail?
                    let blob = self.repo.find_blob(tree_entry.id()).unwrap();

                    // TODO: dirty files
                    Entry {
                        name: name.clone(),
                        parent: ino,
                        size: blob.size(),
                        perm,
                        ctime: Timespec::new(0, 0),
                        atime: Timespec::new(0, 0),
                        mtime: Timespec::new(0, 0),
                        crtime: Timespec::new(0, 0),
                        u: EntryKind::GitBlob {
                            oid: tree_entry.id(),
                        },
                    }
                }
                Some(ObjectType::Tree) => Entry {
                    parent: ino,
                    name: name.clone(),
                    perm: Permissions::from_mode(0o755),  // tree doesn't have a proper mode
                    size: 0,
                    ctime: Timespec::new(0, 0),
                    atime: Timespec::new(0, 0),
                    mtime: Timespec::new(0, 0),
                    crtime: Timespec::new(0, 0),
                    u: EntryKind::GitTree {
                        oid: tree_entry.id(),
                        children: None,
                    },
                },
                _ => {
                    warn!(
                        "{} ({}) is not supported, skipping",
                        tree_entry.id(),
                        String::from_utf8_lossy(tree_entry.name_bytes())
                    );
                    continue;
                }
            };
            entries.push((name, entry));
        }
        Ok(entries)
    }

    fn make_attr(&self, ino: Ino, entry: &Entry) -> FileAttr {
        FileAttr {
            ino: ino.into(),
            size: entry.size as u64,
            blocks: 0,
            atime: entry.atime,
            mtime: entry.mtime,
            ctime: entry.ctime,
            crtime: entry.crtime,
            kind: FileType::from(entry),
            perm: entry.perm.mode() as u16,
            nlink: if FileType::from(entry) == FileType::Directory {
                2
            } else {
                1
            },
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            flags: 0,
        }
    }
}
