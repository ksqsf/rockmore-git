use std::ffi::OsStr;
use git2::*;
use fuse;
use env_logger;
use std::env;
use openat::Dir;

extern crate rockmore_git;
use rockmore_git::gitfs::*;

fn main() {
    env_logger::init();
    let repo_path = env::args().nth(1).unwrap();
    let mountpoint = env::args().nth(2).unwrap();
    let dir = Dir::open(&mountpoint).unwrap();
    let repo = Repository::open(repo_path).unwrap();

    let fs = GitFS::new(repo, dir);
    let options = ["-o", "fsname=gitfs"]
        .iter()
        .map(|x| x.as_ref())
        .collect::<Vec<&OsStr>>();
    fuse::mount(fs, &mountpoint, &options).unwrap();
}
