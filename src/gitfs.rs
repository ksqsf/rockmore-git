use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::io::Write;
use time::Timespec;
use fuse::{Filesystem, FileAttr, FileType, Request, ReplyAttr, ReplyDirectory, ReplyEntry, ReplyData, ReplyOpen};
use git2::{Repository, Oid, ObjectType};
use libc::{c_int, ENOENT, ENOTDIR, EISDIR};
use openat::Dir;

type InoMap = BTreeMap<u64, Oid>;
type OidMap = HashMap<Oid, u64>;

/// GitFS.
pub struct GitFS {
    repo: Repository,
    dir: Dir,
    commitish: Option<Oid>,
    inomap: InoMap,
    oidmap: OidMap,
}

impl GitFS {
    pub fn new(repo: Repository, dir: Dir, commitish: Option<Oid>) -> GitFS {
        GitFS {
            repo,
            dir,
            commitish,
            inomap: InoMap::new(),
            oidmap: OidMap::new(),
        }
    }

    fn ttl() -> Timespec {
        Timespec::new(1, 0)
    }

    fn make_attr(&mut self, ino: u64) -> FileAttr {
        let oid = self.inomap[&ino];
        let object = self.repo.find_object(oid, Some(ObjectType::Any)).unwrap();
        match object.kind() {
            Some(ObjectType::Tree) => {
                FileAttr {
                    ino,
                    size: 0,
                    blocks: 0,
                    atime: Timespec::new(0, 0),
                    mtime: Timespec::new(0, 0),
                    ctime: Timespec::new(0, 0),
                    crtime: Timespec::new(0, 0),
                    kind: FileType::Directory,
                    perm: 0o755,
                    nlink: 2,
                    uid: 501,
                    gid: 20,
                    rdev: 0,
                    flags: 0,
                }
            }
            Some(ObjectType::Blob) => {
                let blob = object.into_blob().unwrap();
                FileAttr {
                    ino,
                    size: blob.content().len() as u64,
                    blocks: 0,
                    atime: Timespec::new(0, 0),
                    mtime: Timespec::new(0, 0),
                    ctime: Timespec::new(0, 0),
                    crtime: Timespec::new(0, 0),
                    kind: FileType::RegularFile,
                    perm: 0o755,
                    nlink: 1,
                    uid: 501,
                    gid: 20,
                    rdev: 0,
                    flags: 0,
                }
            }
            _ => {
                panic!("wtf");
            }
        }
    }
}

impl Filesystem for GitFS {
    fn init(&mut self, _req: &Request) -> Result<(), c_int> {
        let mut f = self.dir.write_file("write_when_mounted", 0o644).unwrap();
        f.write(b"just a test...\ng").unwrap();

        let tree_id = match self.commitish {
            Some(commitish) => self.repo.find_commit(commitish).unwrap().tree().unwrap().id(),
            None => self.repo.head().unwrap().peel_to_commit().unwrap().tree().unwrap().id(),
        };
        self.inomap.insert(1, tree_id);
        self.oidmap.insert(tree_id, 1);
        Ok(())
    }

    fn read(&mut self, _req: &Request, ino: u64, _fh: u64, offset: i64, size: u32, reply: ReplyData) {
        let blob_id = match self.inomap.get(&ino).cloned() {
            Some(oid) => oid,
            None => return reply.error(ENOENT),
        };
        let blob = match self.repo.find_blob(blob_id) {
            Ok(blob) => blob,
            Err(_) => return reply.error(EISDIR),
        };
        let offset = offset as usize;
        let size = size as usize;
        reply.data(&blob.content()[offset..offset+size]);
    }

    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let entry_ino = {
            let tree_id = self.inomap[&parent];
            let tree = match self.repo.find_tree(tree_id) {
                Ok(tree) => tree,
                Err(_) => return reply.error(ENOTDIR),
            };
            tree.get_name(name.to_str().unwrap())
                .and_then(|entry| self.oidmap.get(&entry.id()).cloned())
        };
        match entry_ino {
            Some(ino) => reply.entry(&Self::ttl(), &self.make_attr(ino), 0),
            None => reply.error(ENOENT),
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        reply.attr(&GitFS::ttl(), &self.make_attr(ino));
    }

    fn opendir(&mut self, _req: &Request, ino: u64, mut reply: ReplyOpen) {
        for entry in tree.iter() {
            dir.add(entry.name_bytes(), );
        }
        reply.opened();
    }

    fn readdir(&mut self, _req: &Request, ino: u64, _fh: u64, offset: i64, mut reply: ReplyDirectory) {
        println!("readdir ino={} offset={}", ino, offset);
        let tree_id = match self.inomap.get(&ino).cloned() {
            Some(oid) => oid,
            None => return reply.error(ENOENT),
        };
        let tree = match self.repo.find_tree(tree_id) {
            Ok(tree) => tree,
            Err(_) => return reply.error(ENOTDIR),
        };
        for (i, entry) in tree.iter().enumerate().skip(offset as usize) {
            let filetype = match entry.kind() {
                Some(ObjectType::Tree) => FileType::Directory,
                Some(ObjectType::Blob) => FileType::RegularFile,
                _ => unreachable!(),
            };
            let entry_id = entry.id();
            let next_ino = (self.oidmap.len() + 1) as u64;
            let ino = *self.oidmap.entry(entry_id).or_insert(next_ino);
            self.inomap.insert(ino, entry_id);
            println!("{} {} {}", entry.name().unwrap(), ino, (i+1) as i64);
            if reply.add(ino, (i+1) as i64, filetype, entry.name().unwrap()) {
                return reply.ok()
            }
        }
        reply.ok();
    }
}
