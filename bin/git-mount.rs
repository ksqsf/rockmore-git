use git2::*;
use fuser::{self, MountOption};
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
    let options = [
        MountOption::AutoUnmount,
        MountOption::FSName("gitfs".to_string()),
        MountOption::CUSTOM("nonempty".to_string())
    ];
    fuser::mount2(fs, &mountpoint, &options).unwrap();
}
