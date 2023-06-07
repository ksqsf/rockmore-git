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
use std::io;
use std::io::SeekFrom;
use std::io::{Read, Seek, Write};
use std::os::unix::{ffi::OsStrExt, fs::PermissionsExt};
use std::time::{Duration, SystemTime};

use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyWrite, Request, KernelConfig, TimeOrNow,
};
use git2::{Error as GitError, ObjectType, Oid, Repository, Tree};
use libc::{c_int, mode_t, stat, EIO, EISDIR, ENOENT, ENOTDIR, O_RDONLY};
use openat::{Dir, SimpleType};
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

macro_rules! io_ok {
    ($value:expr, $reply:ident, $default_errno:expr) => {
        match $value {
            Ok(value) => value,
            Err(e) => return $reply.error(e.raw_os_error().unwrap_or($default_errno)),
        }
    };
    ($value:expr, $reply:ident) => {
        io_ok!($value, $reply, libc::EIO)
    };
}

pub struct GitFS {
    repo: Repository,
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
    fn init(&mut self, _req: &Request, _config: &mut KernelConfig) -> Result<(), c_int> {
        let commit = self.repo.head().unwrap().peel_to_commit().unwrap();
        let tree = commit.tree().unwrap();
        self.inomap.add(self.root_entry(tree));
        info!("gitfs is mounted");
        Ok(())
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
                return reply.entry(&Self::ttl(), &Self::make_attr(child, child_entry), 0);
            }
            EntryKind::DirtyDir {
                children: Some(children),
            } => {
                let child = *some!(children.get(name), reply, ENOENT);
                let child_entry = some!(self.inomap.get(child), reply, ENOENT);
                return reply.entry(&Self::ttl(), &Self::make_attr(child, child_entry), 0);
            }
            EntryKind::GitTree { children: None, .. } => match self.do_opendir(parent.into()) {
                Ok(_) => (),
                Err(e) => return reply.error(e),
            },
            EntryKind::DirtyDir { children: None, .. } => match self.do_opendir(parent.into()) {
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
                return reply.entry(&Self::ttl(), &Self::make_attr(child, child_entry), 0);
            }
            EntryKind::DirtyDir {
                children: Some(children),
            } => {
                let child = *some!(children.get(name), reply, ENOENT);
                let child_entry = some!(self.inomap.get(child), reply, ENOENT);
                return reply.entry(&Self::ttl(), &Self::make_attr(child, child_entry), 0);
            }
            EntryKind::GitTree { children: None, .. } => {
                warn!("children is empty after fill, skipping");
                return reply.error(ENOENT);
            }
            EntryKind::DirtyDir { children: None, .. } => {
                warn!("children is empty after fill, skipping");
                return reply.error(ENOENT);
            }
            _ => return reply.error(ENOTDIR),
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        let ino = Ino::from(ino);
        let entry = some!(self.inomap.get(ino), reply, ENOENT);
        return reply.attr(&Self::ttl(), &Self::make_attr(ino, entry));
    }

    fn setattr(
        &mut self,
        _req: &Request,
        ino: u64,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let ino = Ino::from(ino);
        let entry = some!(self.inomap.get_mut(ino), reply, ENOENT);
        // We are just making up numbers to satisfy FUSE.  Git has its
        // own idea of these attributes, so don't take them seriously.
        mode.map(|x| entry.perm = Permissions::from_mode(x));
        size.map(|x| entry.size = x);
        atime.map(|x| entry.atime = match x {
            TimeOrNow::SpecificTime(time) => time,
            TimeOrNow::Now => SystemTime::now()
        });
        mtime.map(|x| entry.mtime = match x {
            TimeOrNow::SpecificTime(time) => time,
            TimeOrNow::Now => SystemTime::now()
        });
        crtime.map(|x| entry.crtime = x);
        dbg!(&entry);
        return reply.attr(&Self::ttl(), &Self::make_attr(ino, entry));
    }

    fn opendir(&mut self, _req: &Request, ino: u64, _flags: i32, reply: ReplyOpen) {
        let ino = Ino::from(ino);
        match self.do_opendir(ino) {
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
            EntryKind::DirtyDir {
                children: Some(children),
            } => {
                dbg!(&children);
                dbg!(offset);
                for (i, (name, &child)) in children.iter().enumerate().skip(offset as usize) {
                    println!("{:?} {:?}", Ino::from(child), name);
                    if reply.add(child.into(), (i + 1) as i64, entry.into(), name) {
                        return reply.ok();
                    }
                }
                return reply.ok();
            }
            _ => {
                // readdir cannot be called before opendir
                unreachable!()
            }
        }
    }

    fn releasedir(&mut self, _req: &Request, ino: u64, _fh: u64, _flags: i32, reply: ReplyEmpty) {
        let ino = Ino::from(ino);
        let entry = some!(self.inomap.get_mut(ino), reply, ENOENT);
        match entry.u {
            EntryKind::DirtyFile { .. } | EntryKind::GitBlob { .. } => return reply.error(ENOTDIR),
            EntryKind::GitTree { .. } | EntryKind::DirtyDir { .. } => (),
        }
        return reply.ok();
    }

    fn open(&mut self, _req: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
        dbg!(flags);
        let ino = Ino::from(ino);
        let entry = some!(self.inomap.get_mut(ino), reply, ENOENT);
        match entry.u {
            EntryKind::GitTree { .. } | EntryKind::DirtyDir { .. } => return reply.error(EISDIR),
            EntryKind::DirtyFile {
                file: Some(_),
                ref mut refcnt,
            } => {
                *refcnt += 1;
            }
            EntryKind::DirtyFile {
                file: None,
                ref mut refcnt,
            } => {
                *refcnt = 1;
                let path = self.inomap.prefix(ino).unwrap();
                debug!("Open dirty file {:?}", path);
                io_ok!(self.open_dirty_file(ino), reply);
                return reply.opened(0, 0);
            }
            EntryKind::GitBlob { .. } if flags & !O_RDONLY == 0 => {
                return reply.opened(0, 0);
            }
            EntryKind::GitBlob { oid } => {
                io_ok!(self.open_git_blob_for_update(oid, ino), reply);
                return reply.opened(0, 0);
            }
        }
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let entry = some!(self.inomap.get_mut(ino.into()), reply, ENOENT);
        let offset = offset as usize;
        let size = size as usize;
        match &mut entry.u {
            EntryKind::GitBlob { oid, .. } => {
                let blob = self.repo.find_blob(*oid).unwrap();
                return reply.data(&blob.content()[offset..offset + size]);
            }
            EntryKind::DirtyFile { file, .. } => {
                if file.is_none() {
                    warn!("read closed file!");
                    return reply.error(libc::EIO);
                }
                let file = file.as_mut().unwrap();
                let mut buf = vec![0; size];
                io_ok!(file.seek(SeekFrom::Start(offset as u64)), reply);
                let nbytes = io_ok!(file.read(&mut buf), reply);
                return reply.data(&buf[0..nbytes]);
            }
            EntryKind::GitTree { .. } | EntryKind::DirtyDir { .. } => return reply.error(EISDIR),
        }
    }

    fn write(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let ino = Ino::from(ino);
        let entry = some!(self.inomap.get_mut(ino), reply, ENOENT);
        match &mut entry.u {
            EntryKind::GitTree { .. } | EntryKind::DirtyDir { .. } => return reply.error(EISDIR),
            EntryKind::DirtyFile {
                file: Some(ref mut file),
                ..
            } => {
                io_ok!(file.seek(SeekFrom::Start(offset as u64)), reply);
                let nbytes = io_ok!(file.write(data), reply);

                // Maintain size.
                entry.size = entry.size.max((offset as u64) + (data.len() as u64));

                return reply.written(nbytes as u32);
            }
            _ => {
                // 1. We should have already replaced all GitBlob with DirtyFile
                // 2. Such files must have been opened for updating
                unreachable!()
            }
        }
    }

    fn flush(&mut self, _req: &Request, ino: u64, _fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        let ino = Ino::from(ino);
        let entry = some!(self.inomap.get_mut(ino), reply, ENOENT);
        match entry.u {
            EntryKind::GitTree { .. } | EntryKind::DirtyDir { .. } => return reply.error(EISDIR),
            EntryKind::GitBlob { .. } => {
                // A flush() will be called on read-only files as well.
                return reply.ok();
            }
            EntryKind::DirtyFile {
                file: Some(ref mut f),
                ..
            } => {
                io_ok!(f.flush(), reply);
                return reply.ok();
            }
            _ => {
                unreachable!();
            }
        }
    }

    fn release(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let ino = Ino::from(ino);
        let entry = some!(self.inomap.get_mut(ino), reply, ENOENT);
        match entry.u {
            EntryKind::DirtyDir { .. } | EntryKind::GitTree { .. } => return reply.error(EISDIR),
            EntryKind::GitBlob { .. } => (),
            EntryKind::DirtyFile {
                ref mut file,
                ref mut refcnt,
            } => {
                *refcnt -= 1;
                if *refcnt <= 0 {
                    *refcnt = 0;
                    *file = None;
                }
            }
        }
        return reply.ok();
    }

    fn create(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let path = {
            let mut p = some!(self.inomap.prefix(Ino::from(parent)), reply, EIO);
            p.push(name);
            p
        };
        let file = io_ok!(self.underlying_dir.write_file(&path, mode as mode_t), reply);
        let fentry = Entry {
            name: name.to_owned(),
            parent: Ino::from(parent),
            ctime: SystemTime::now(),
            mtime: SystemTime::now(),
            atime: SystemTime::now(),
            crtime: SystemTime::now(),
            perm: Permissions::from_mode(mode),
            size: 0,
            u: EntryKind::DirtyFile {
                refcnt: 1,
                file: Some(file),
            },
        };
        let attr = Self::make_attr(self.inomap.next_ino(), &fentry);
        let ino = self.inomap.add(fentry);
        let dir = some!(self.inomap.get_mut(Ino::from(parent)), reply, ENOENT);
        let children = match &mut dir.u {
            EntryKind::GitTree {
                children: Some(ref mut c),
                ..
            } => c,
            EntryKind::DirtyDir {
                children: Some(ref mut c),
                ..
            } => c,
            _ => unreachable!(),
        };
        children.insert(name.to_owned(), ino);
        reply.created(&Self::ttl(), &attr, 0, 0, 0)
    }

    fn mkdir(&mut self,
             _req: &Request,
             parent: u64,
             name: &OsStr,
             mode: u32,
             _umask: u32,
             reply: ReplyEntry
    ) {
        let path = {
            let mut p = some!(self.inomap.prefix(Ino::from(parent)), reply, EIO);
            p.push(name);
            p
        };
        io_ok!(self.underlying_dir.create_dir(&path, mode as mode_t), reply);
        let dentry = Entry {
            name: name.to_owned(),
            parent: Ino::from(parent),
            ctime: SystemTime::now(),
            mtime: SystemTime::now(),
            atime: SystemTime::now(),
            crtime: SystemTime::now(),
            perm: Permissions::from_mode(mode),
            size: 0,
            u: EntryKind::DirtyDir { children: None },
        };
        let attr = Self::make_attr(self.inomap.next_ino(), &dentry);
        let ino = self.inomap.add(dentry);
        let dir = some!(self.inomap.get_mut(Ino::from(parent)), reply, ENOENT);
        let children = match &mut dir.u {
            EntryKind::GitTree {
                children: Some(ref mut c),
                ..
            } => c,
            EntryKind::DirtyDir {
                children: Some(ref mut c),
                ..
            } => c,
            _ => unreachable!(),
        };
        children.insert(name.to_owned(), ino);
        reply.entry(&Self::ttl(), &attr, 0);
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        self.do_remove(parent.into(), name, reply)
    }

    fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        self.do_remove(parent.into(), name, reply)
    }

    fn rename(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let oldp = parent.into();
        let oldpent = some!(self.inomap.get(oldp), reply, ENOENT);
        let c = some!(oldpent.get_child(name), reply, ENOENT);
        let cent = some!(self.inomap.get(c), reply, ENOENT);
        let newp = newparent.into();
        let _newpent = some!(self.inomap.get(newp), reply, ENOENT);

        // Move dirty files/directories physically.
        match cent.u {
            EntryKind::DirtyFile { .. } | EntryKind::DirtyDir { .. } => {
                let oldpath = self.inomap.prefix(c).unwrap();
                let mut newpath = self.inomap.prefix(newp).unwrap();
                newpath.push(newname);
                debug!("move {:?} to {:?}", oldpath, newpath);
                io_ok!(self.underlying_dir.local_rename(&oldpath, &newpath), reply);
            }
            _ => (),
        }

        // Move entry from oldp to newp. Keep ino intact.
        let cent = self.inomap.get_mut(c).unwrap();
        cent.name = newname.to_os_string();
        let oldpent = self.inomap.get_mut(oldp).unwrap();
        oldpent.remove_child(name).unwrap();
        let newpent = self.inomap.get_mut(newp).unwrap();
        newpent.add_child(newname.to_os_string(), c);
        return reply.ok();
    }

    fn destroy(&mut self) {}

    fn forget(&mut self, _req: &Request<'_>, _ino: u64, _nlookup: u64) {}

    fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
        debug!("[Not Implemented] readlink(ino: {:#x?})", ino);
        reply.error(libc::ENOSYS);
    }

    fn mknod(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        rdev: u32,
        reply: ReplyEntry,
    ) {
        debug!(
            "[Not Implemented] mknod(parent: {:#x?}, name: {:?}, mode: {}, \
            umask: {:#x?}, rdev: {})",
            parent, name, mode, umask, rdev
        );
        reply.error(libc::ENOSYS);
    }

    fn symlink(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        link: &std::path::Path,
        reply: ReplyEntry,
    ) {
        debug!(
            "[Not Implemented] symlink(parent: {:#x?}, name: {:?}, link: {:?})",
            parent, name, link,
        );
        reply.error(libc::EPERM);
    }

    fn link(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        newparent: u64,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        debug!(
            "[Not Implemented] link(ino: {:#x?}, newparent: {:#x?}, newname: {:?})",
            ino, newparent, newname
        );
        reply.error(libc::EPERM);
    }

    fn fsync(&mut self, _req: &Request<'_>, ino: u64, fh: u64, datasync: bool, reply: ReplyEmpty) {
        debug!(
            "[Not Implemented] fsync(ino: {:#x?}, fh: {}, datasync: {})",
            ino, fh, datasync
        );
        reply.error(libc::ENOSYS);
    }

    fn readdirplus(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        reply: fuser::ReplyDirectoryPlus,
    ) {
        debug!(
            "[Not Implemented] readdirplus(ino: {:#x?}, fh: {}, offset: {})",
            ino, fh, offset
        );
        reply.error(libc::ENOSYS);
    }

    fn fsyncdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        debug!(
            "[Not Implemented] fsyncdir(ino: {:#x?}, fh: {}, datasync: {})",
            ino, fh, datasync
        );
        reply.error(libc::ENOSYS);
    }

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: fuser::ReplyStatfs) {
        reply.statfs(0, 0, 0, 0, 0, 512, 255, 0);
    }

    fn setxattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        name: &OsStr,
        _value: &[u8],
        flags: i32,
        position: u32,
        reply: ReplyEmpty,
    ) {
        debug!(
            "[Not Implemented] setxattr(ino: {:#x?}, name: {:?}, flags: {:#x?}, position: {})",
            ino, name, flags, position
        );
        reply.error(libc::ENOSYS);
    }

    fn getxattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        name: &OsStr,
        size: u32,
        reply: fuser::ReplyXattr,
    ) {
        debug!(
            "[Not Implemented] getxattr(ino: {:#x?}, name: {:?}, size: {})",
            ino, name, size
        );
        reply.error(libc::ENOSYS);
    }

    fn listxattr(&mut self, _req: &Request<'_>, ino: u64, size: u32, reply: fuser::ReplyXattr) {
        debug!(
            "[Not Implemented] listxattr(ino: {:#x?}, size: {})",
            ino, size
        );
        reply.error(libc::ENOSYS);
    }

    fn removexattr(&mut self, _req: &Request<'_>, ino: u64, name: &OsStr, reply: ReplyEmpty) {
        debug!(
            "[Not Implemented] removexattr(ino: {:#x?}, name: {:?})",
            ino, name
        );
        reply.error(libc::ENOSYS);
    }

    fn access(&mut self, _req: &Request<'_>, ino: u64, mask: i32, reply: ReplyEmpty) {
        debug!("[Not Implemented] access(ino: {:#x?}, mask: {})", ino, mask);
        reply.error(libc::ENOSYS);
    }

    fn getlk(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        lock_owner: u64,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        reply: fuser::ReplyLock,
    ) {
        debug!(
            "[Not Implemented] getlk(ino: {:#x?}, fh: {}, lock_owner: {}, start: {}, \
            end: {}, typ: {}, pid: {})",
            ino, fh, lock_owner, start, end, typ, pid
        );
        reply.error(libc::ENOSYS);
    }

    fn setlk(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        lock_owner: u64,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        sleep: bool,
        reply: ReplyEmpty,
    ) {
        debug!(
            "[Not Implemented] setlk(ino: {:#x?}, fh: {}, lock_owner: {}, start: {}, \
            end: {}, typ: {}, pid: {}, sleep: {})",
            ino, fh, lock_owner, start, end, typ, pid, sleep
        );
        reply.error(libc::ENOSYS);
    }

    fn bmap(&mut self, _req: &Request<'_>, ino: u64, blocksize: u32, idx: u64, reply: fuser::ReplyBmap) {
        debug!(
            "[Not Implemented] bmap(ino: {:#x?}, blocksize: {}, idx: {})",
            ino, blocksize, idx,
        );
        reply.error(libc::ENOSYS);
    }

    fn ioctl(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        flags: u32,
        cmd: u32,
        in_data: &[u8],
        out_size: u32,
        reply: fuser::ReplyIoctl,
    ) {
        debug!(
            "[Not Implemented] ioctl(ino: {:#x?}, fh: {}, flags: {}, cmd: {}, \
            in_data.len(): {}, out_size: {})",
            ino,
            fh,
            flags,
            cmd,
            in_data.len(),
            out_size,
        );
        reply.error(libc::ENOSYS);
    }

    fn fallocate(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        length: i64,
        mode: i32,
        reply: ReplyEmpty,
    ) {
        debug!(
            "[Not Implemented] fallocate(ino: {:#x?}, fh: {}, offset: {}, \
            length: {}, mode: {})",
            ino, fh, offset, length, mode
        );
        reply.error(libc::ENOSYS);
    }

    fn lseek(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        whence: i32,
        reply: fuser::ReplyLseek,
    ) {
        debug!(
            "[Not Implemented] lseek(ino: {:#x?}, fh: {}, offset: {}, whence: {})",
            ino, fh, offset, whence
        );
        reply.error(libc::ENOSYS);
    }

    fn copy_file_range(
        &mut self,
        _req: &Request<'_>,
        ino_in: u64,
        fh_in: u64,
        offset_in: i64,
        ino_out: u64,
        fh_out: u64,
        offset_out: i64,
        len: u64,
        flags: u32,
        reply: ReplyWrite,
    ) {
        debug!(
            "[Not Implemented] copy_file_range(ino_in: {:#x?}, fh_in: {}, \
            offset_in: {}, ino_out: {:#x?}, fh_out: {}, offset_out: {}, \
            len: {}, flags: {})",
            ino_in, fh_in, offset_in, ino_out, fh_out, offset_out, len, flags
        );
        reply.error(libc::ENOSYS);
    }

    fn setvolname(&mut self, _req: &Request<'_>, name: &OsStr, reply: ReplyEmpty) {
        debug!("[Not Implemented] setvolname(name: {:?})", name);
        reply.error(libc::ENOSYS);
    }

    fn exchange(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        options: u64,
        reply: ReplyEmpty,
    ) {
        debug!(
            "[Not Implemented] exchange(parent: {:#x?}, name: {:?}, newparent: {:#x?}, \
            newname: {:?}, options: {})",
            parent, name, newparent, newname, options
        );
        reply.error(libc::ENOSYS);
    }

    fn getxtimes(&mut self, _req: &Request<'_>, ino: u64, reply: fuser::ReplyXTimes) {
        debug!("[Not Implemented] getxtimes(ino: {:#x?})", ino);
        reply.error(libc::ENOSYS);
    }
}

// private interfaces
impl GitFS {
    /// by default, ttl = 1 second
    fn ttl() -> Duration {
        Duration::from_secs(1)
    }

    /// Create the root entry.
    #[cfg(target_os = "macos")]
    fn root_entry(&self, tree: Tree<'_>) -> Entry {
        let metadata = self.underlying_dir.self_metadata().unwrap();
        let stat = metadata.stat();
        let atime = SystemTime::UNIX_EPOCH + Duration::from_secs(stat.st_atime as u64);
        let ctime = SystemTime::UNIX_EPOCH + Duration::from_secs(stat.st_ctime as u64);
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(stat.st_mtime as u64);
        let crtime = SystemTime::UNIX_EPOCH + Duration::from_secs(stat.st_birthtime as u64);
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

    #[cfg(not(target_os = "macos"))]
    fn root_entry(&self, tree: Tree<'_>) -> Entry {
        let metadata = self.underlying_dir.self_metadata().unwrap();
        let stat = metadata.stat();
        let atime = Timespec::new(stat.st_atime, stat.st_atime_nsec as i32);
        let mtime = Timespec::new(stat.st_mtime, stat.st_mtime_nsec as i32);
        let ctime = Timespec::new(stat.st_ctime, stat.st_ctime_nsec as i32);
        let crtime = Timespec::new(0, 0);
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

    /// Remove a file or a directory.
    fn do_remove(&mut self, parent: Ino, name: &OsStr, reply: ReplyEmpty) {
        let parent_entry = some!(self.inomap.get(parent.into()), reply, ENOENT);
        let child = match parent_entry.u {
            EntryKind::DirtyDir {
                children: Some(ref c),
            } => *some!(c.get(name), reply, ENOENT),
            EntryKind::GitTree {
                children: Some(ref c),
                ..
            } => *some!(c.get(name), reply, ENOENT),
            _ => unreachable!(),
        };

        match self.remove_entry(child) {
            Ok(_) => return reply.ok(),
            Err((entry, err)) => {
                let ino = self.inomap.add(entry);
                let parent_entry = self.inomap.get_mut(parent.into()).unwrap();
                let c = match parent_entry.u {
                    EntryKind::DirtyDir {
                        children: Some(ref mut c),
                    } => c,
                    EntryKind::GitTree {
                        children: Some(ref mut c),
                        ..
                    } => c,
                    _ => unreachable!(),
                };
                c.insert(name.to_os_string(), ino);
                return reply.error(err.raw_os_error().unwrap_or(EIO));
            }
        }
    }

    /// Remove ino from inomap. If the entry fails to be removed
    /// (e.g. cannot delete dirty file on disk), the entry itself is
    /// returned so that it can be inserted.
    fn remove_entry(&mut self, ino: Ino) -> Result<(), (Entry, io::Error)> {
        let path = self.inomap.prefix(ino).unwrap();
        let mut entry = self.inomap.remove(ino).unwrap();
        match entry.u {
            EntryKind::DirtyFile {
                ref mut refcnt,
                ref mut file,
            } => match self.underlying_dir.remove_file(&path) {
                Ok(_) => {
                    *refcnt = 0;
                    let _ = file.take();
                    Ok(())
                }
                Err(err) => Err((entry, err)),
            },
            EntryKind::DirtyDir { .. } => match self.underlying_dir.remove_dir(&path) {
                Ok(_) => Ok(()),
                Err(err) => Err((entry, err)),
            },
            EntryKind::GitBlob { .. } => {
                // TODO: Perhaps we should record such information, so
                // that when the repo is mounted here, we can restore
                // the unstaged deletion.
                return Ok(());
            }
            EntryKind::GitTree { .. } => Ok(()),
        }
    }

    fn open_dirty_file(&mut self, ino: Ino) -> Result<(), io::Error> {
        let path = self.inomap.prefix(ino).unwrap();
        let entry = self.inomap.get_mut(ino).unwrap();
        let f = self
            .underlying_dir
            .update_file(&path, entry.perm.mode() as mode_t)?;
        entry.u = EntryKind::DirtyFile {
            file: Some(f),
            refcnt: 1,
        };
        Ok(())
    }

    fn open_git_blob_for_update(&mut self, oid: Oid, ino: Ino) -> Result<(), io::Error> {
        // checkout git blob
        let path = self.inomap.prefix(ino).unwrap();
        let blob = self.repo.find_blob(oid).unwrap();
        let entry = self.inomap.get_mut(ino).unwrap();
        let mut f = self
            .underlying_dir
            .update_file(&path, entry.perm.mode() as mode_t)?;
        f.write_all(blob.content())?;

        // replace git blob entry with a dirty file entry
        entry.u = EntryKind::DirtyFile {
            file: Some(f),
            refcnt: 1,
        };

        Ok(())
    }

    /// List a GitTree or open a dirty dir.
    fn do_opendir(&mut self, ino: Ino) -> Result<(), c_int> {
        let dir_entry = self.inomap.get(ino).ok_or(ENOENT)?;

        // Step1: check if has been listed. if so, return early;
        // otherwise, list the dir.
        //
        // We treat directories specially, because dir_entry points to
        // inomap, but inomap should stay unchanged during our walk.
        let walk;
        match dir_entry.u {
            EntryKind::DirtyDir { children: Some(_) } => return Ok(()),
            EntryKind::GitTree {
                children: Some(_), ..
            } => return Ok(()),
            EntryKind::GitTree {
                oid,
                children: None,
                ..
            } => {
                walk = self.walk_dir(ino, Some(oid))?;
            }
            EntryKind::DirtyDir { children: None, .. } => {
                walk = self.walk_dir(ino, None)?;
            }
            _ => return Err(ENOTDIR),
        }
        dbg!(&walk);

        // Step2: walk done, insert data to inomap so that we have inos
        let children_entries = walk
            .into_iter()
            .map(|(name, entry)| (name, self.inomap.add(entry)))
            .collect::<HashMap<OsString, Ino>>();
        dbg!(&children_entries);

        // Step3: lookup dir_entry again in case it's moved
        let dir_entry = self.inomap.get_mut(ino).ok_or(ENOENT)?;
        match dir_entry.u {
            EntryKind::GitTree {
                children: ref mut c @ None,
                ..
            } => {
                c.replace(children_entries);
                return Ok(());
            }
            EntryKind::DirtyDir {
                children: ref mut c @ None,
                ..
            } => {
                c.replace(children_entries);
                return Ok(());
            }
            _ => unreachable!(),
        }
    }

    fn walk_tree(&self, ino: Ino, tree_id: Oid) -> Result<HashMap<OsString, Entry>, GitError> {
        let tree = self.repo.find_tree(tree_id)?;
        let mut entries = HashMap::new();

        for tree_entry in tree.iter() {
            let name = OsString::from(OsStr::from_bytes(tree_entry.name_bytes()));
            let perm = Permissions::from_mode(tree_entry.filemode() as u32);
            let entry = match tree_entry.kind() {
                Some(ObjectType::Blob) => {
                    let blob = self.repo.find_blob(tree_entry.id())?;
                    Entry {
                        name: name.clone(),
                        parent: ino,
                        size: blob.size() as u64,
                        perm,
                        ctime: SystemTime::UNIX_EPOCH,
                        atime: SystemTime::UNIX_EPOCH,
                        mtime: SystemTime::UNIX_EPOCH,
                        crtime: SystemTime::UNIX_EPOCH,
                        u: EntryKind::GitBlob {
                            oid: tree_entry.id(),
                        },
                    }
                }
                Some(ObjectType::Tree) => Entry {
                    parent: ino,
                    name: name.clone(),
                    perm: Permissions::from_mode(0o755), // tree doesn't have a proper mode
                    size: 0,
                    ctime: SystemTime::UNIX_EPOCH,
                    atime: SystemTime::UNIX_EPOCH,
                    mtime: SystemTime::UNIX_EPOCH,
                    crtime: SystemTime::UNIX_EPOCH,
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
            entries.insert(name, entry);
        }
        Ok(entries)
    }

    /// If tree_id is None, then the directory is considered a dirty
    /// dir. All files and dirs under a dirty dir are dirty.
    /// Of course, it can be recursive, but laziness is a virtue.
    fn walk_dir(
        &self,
        ino: Ino,
        tree_id: Option<Oid>,
    ) -> Result<HashMap<OsString, Entry>, c_int> {
        let mut entries = match tree_id {
            Some(tree_id) => self.walk_tree(ino, tree_id).map_err(|_| ENOENT)?,
            None => HashMap::new(),
        };

        // look at underlying_dir/prefix
        let prefix = self.inomap.prefix(ino).unwrap();
        let dir_iter = if ino.is_root() {
            self.underlying_dir.list_self()
        } else {
            self.underlying_dir.list_dir(prefix.as_os_str())
        };

        if dir_iter.is_err() && tree_id.is_some() {
            // git tree doesn't have a ghost dir, return early
            return Ok(entries);
        } else if dir_iter.is_err() && tree_id.is_none() {
            // error inside dirty dir
            return Err(EIO);
        }
        let dir_iter = dir_iter.unwrap();

        // try to collect dirty entries
        for dirty_entry in dir_iter {
            if dirty_entry.is_err() {
                warn!("a dirty entry cannot be read, skipping");
                continue;
            }

            let dirty_entry = dirty_entry.unwrap();
            let mut path = prefix.clone();
            path.push(dirty_entry.file_name());
            let metadata = match self.underlying_dir.metadata(&path) {
                Ok(metadata) => metadata,
                Err(e) => {
                    warn!("metadata of a dirty entry cannot be read {}, skipping", e);
                    continue;
                }
            };
            let stat = metadata.stat();
            match dirty_entry.simple_type() {
                Some(SimpleType::Dir) => {
                    println!("found dir: {:?}", dirty_entry.file_name());
                    // a dir is dirty <=> it's on disk but not in git tree
                    if !entries.contains_key(dirty_entry.file_name()) {
                        let name = dirty_entry.file_name().to_owned();
                        entries.insert(
                            name.clone(),
                            Entry {
                                name: name,
                                parent: ino,
                                perm: Permissions::from_mode(stat.st_mode as u32),
                                size: stat.st_size as u64,
                                atime: SystemTime::UNIX_EPOCH + Duration::from_secs(stat.st_atime as u64),
                                mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(stat.st_mtime as u64),
                                ctime: SystemTime::UNIX_EPOCH + Duration::from_secs(stat.st_ctime as u64),
                                crtime: birthtime(stat),
                                u: EntryKind::DirtyDir { children: None },
                            },
                        );
                    }
                }
                Some(SimpleType::File) => {
                    // a file on disk is always considered dirty
                    println!("found file: {:?}", dirty_entry.file_name());
                    let name = dirty_entry.file_name().to_owned();
                    entries.insert(
                        name.clone(),
                        Entry {
                            name,
                            parent: ino,
                            perm: Permissions::from_mode(stat.st_mode as u32),
                            size: stat.st_size as u64,
                            atime: SystemTime::UNIX_EPOCH + Duration::from_secs(stat.st_atime as u64),
                            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(stat.st_mtime as u64),
                            ctime: SystemTime::UNIX_EPOCH + Duration::from_secs(stat.st_ctime as u64),
                            crtime: birthtime(stat),
                            u: EntryKind::DirtyFile {
                                file: None,
                                refcnt: 0,
                            },
                        },
                    );
                }
                _ => {
                    warn!("unknown file type, skipping");
                    continue;
                }
            }
        }

        Ok(entries)
    }

    fn make_attr(ino: Ino, entry: &Entry) -> FileAttr {
        FileAttr {
            ino: ino.into(),
            size: entry.size,
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
            blksize: 0,
            flags: 0,
        }
    }
}

#[cfg(target_os = "macos")]
fn birthtime(stat: &stat) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(stat.st_birthtime as u64)
}

#[cfg(not(target_os = "macos"))]
fn birthtime(_: &stat) -> SystemTime {
    SystemTime::UNIX_EPOCH
}
