use std::ffi::OsStr;
use git2::Repository;
use fuse;
use std::env;
use gitfs::*;
use openat::Dir;

mod gitfs;

fn main() {
    let repo_path = env::args().nth(1).unwrap();
    let mountpoint = env::args().nth(2).unwrap();
    let dir = Dir::open(&mountpoint).unwrap();
    let repo = Repository::open(repo_path).unwrap();
    let fs = GitFS::new(repo, dir, None);
    let options = ["-o", "ro", "-o", "fsname=gitfs"]
        .iter()
        .map(|x| x.as_ref())
        .collect::<Vec<&OsStr>>();
    fuse::mount(fs, &mountpoint, &options).unwrap();
}
