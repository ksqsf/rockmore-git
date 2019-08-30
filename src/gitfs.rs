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

use crate::{Entry, Ino, InoMap};

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
        match entry {
            Entry::CleanFile { oid, .. } => {
                let blob = self.repo.find_blob(*oid).unwrap();
                return reply.data(&blob.content()[offset..offset + size]);
            }
            Entry::DirtyFile { .. } => unimplemented!(),
            Entry::Directory { .. } => return reply.error(EISDIR),
        }
    }

    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let parent_entry = some!(self.inomap.get(parent.into()), reply, ENOENT);
        match parent_entry {
            Entry::Directory {
                children: Some(children),
                ..
            } => {
                let child = *some!(children.get(name), reply, ENOENT);
                let child_entry = some!(self.inomap.get(child), reply, ENOENT);
                return reply.entry(&Self::ttl(), &self.make_attr(child, child_entry), 0);
            }
            Entry::Directory { children: None, .. } => match self.fill_children(parent.into()) {
                Ok(_) => (),
                Err(e) => return reply.error(e),
            },
            _ => return reply.error(ENOTDIR),
        }

        // if we reachabled here, it means we have filled children of
        // a directory
        let parent_entry = some!(self.inomap.get(parent.into()), reply, ENOENT);
        match parent_entry {
            Entry::Directory {
                children: Some(children),
                ..
            } => {
                let child = *some!(children.get(name), reply, ENOENT);
                let child_entry = some!(self.inomap.get(child), reply, ENOENT);
                return reply.entry(&Self::ttl(), &self.make_attr(child, child_entry), 0);
            }
            Entry::Directory { children: None, .. } => {
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
        match entry {
            Entry::CleanFile { .. } | Entry::DirtyFile { .. } => return reply.error(ENOTDIR),
            Entry::Directory {
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
            Entry::Directory { children: None, .. } => {
                // readdir cannot be called before opendir
                unreachable!()
            }
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
        Entry::Directory {
            oid: tree.id(),
            name: "".to_string().into(),
            atime,
            ctime,
            mtime,
            crtime,
            children: None,
        }
    }

    /// Fill the `children` field of `Entry::Directory`.
    fn fill_children(&mut self, ino: Ino) -> Result<(), c_int> {
        let dir_entry = self.inomap.get(ino).ok_or(ENOENT)?;

        // we treat directories specially becausedir_entry points to
        // inomap, but inomap should stay unchanged during our walk.
        let walk;
        match dir_entry {
            Entry::Directory {
                oid,
                children: None,
                ..
            } => {
                walk = self.walk_tree(ino, *oid).map_err(|_| ENOENT)?;
            }
            Entry::Directory {
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
        match dir_entry {
            Entry::Directory {
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
                    Entry::CleanFile {
                        oid: tree_entry.id(),
                        size: blob.size(),
                        perm,
                        parent: ino,
                        ctime: Timespec::new(0, 0),
                        atime: Timespec::new(0, 0),
                        mtime: Timespec::new(0, 0),
                    }
                }
                Some(ObjectType::Tree) => Entry::Directory {
                    oid: tree_entry.id(),
                    name: name.clone(),
                    ctime: Timespec::new(0, 0),
                    atime: Timespec::new(0, 0),
                    mtime: Timespec::new(0, 0),
                    crtime: Timespec::new(0, 0),
                    children: None,
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
        match entry {
            Entry::Directory { .. } => self.make_dir_attr(ino, entry),
            Entry::CleanFile { .. } => self.make_clean_file_attr(ino, entry),
            Entry::DirtyFile { .. } => self.make_dirty_file_attr(ino, entry),
        }
    }

    fn make_dir_attr(&self, ino: Ino, entry: &Entry) -> FileAttr {
        match entry {
            Entry::Directory {
                atime,
                ctime,
                mtime,
                crtime,
                ..
            } => {
                FileAttr {
                    ino: ino.into(),
                    size: 0,
                    blocks: 0,
                    atime: *atime,
                    mtime: *mtime,
                    ctime: *ctime,
                    crtime: *crtime,
                    kind: FileType::Directory,
                    perm: 0o755,
                    nlink: 2,
                    uid: 501, // FIXME
                    gid: 20,  // FIXME
                    rdev: 0,
                    flags: 0,
                }
            }
            _ => unreachable!(),
        }
    }

    fn make_dirty_file_attr(&self, _ino: Ino, _entry: &Entry) -> FileAttr {
        unimplemented!()
    }

    fn make_clean_file_attr(&self, ino: Ino, entry: &Entry) -> FileAttr {
        match entry {
            Entry::CleanFile {
                perm, size, ..
            } => {
                FileAttr {
                    ino: ino.into(),
                    size: *size as u64,
                    blocks: 0,
                    atime: Timespec::new(0, 0),
                    mtime: Timespec::new(0, 0),
                    ctime: Timespec::new(0, 0),
                    crtime: Timespec::new(0, 0),
                    kind: FileType::RegularFile,
                    perm: perm.mode() as u16,
                    nlink: 1,
                    uid: 501,
                    gid: 20,
                    rdev: 0,
                    flags: 0,
                }
            }
            _ => unreachable!(),
        }
    }
}
