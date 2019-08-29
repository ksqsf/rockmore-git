use std::ffi::OsStr;
use git2::*;
use fuse;
use std::env;
//use gitfs::*;
use openat::Dir;

//mod gitfs;

fn walk<'a>(objs: &mut Vec<Object<'a>>, repo: &'a Repository, tree: &Tree<'a>, prefix: String) {
    for entry in tree.iter() {
        match entry.kind() {
            Some(ObjectType::Commit) | Some(ObjectType::Tag) | Some(ObjectType::Any) | None
                => {
                    eprintln!("I don't know what it is: {}/{}", prefix, entry.name().unwrap());
                    continue
                }
            _ => (),
        }
        let object = entry.to_object(repo).unwrap();
        let pathname = format!("{}/{}", prefix, entry.name().unwrap());
        objs.push(object.clone());
        match entry.kind().unwrap() {
            ObjectType::Tree => walk(objs, repo, &object.into_tree().unwrap(), pathname),
            ObjectType::Blob => println!("{}", pathname),
            _ => eprintln!("[ERROR] unknown type"),
        }
    }
}

fn main() {
    let repo_path = env::args().nth(1).unwrap();
    //let mountpoint = env::args().nth(2).unwrap();
    let repo = Repository::open(repo_path).unwrap();

    let mut all_objects: Vec<Object<'_>> = vec![];
    let root = repo.head().unwrap().peel_to_commit().unwrap().tree().unwrap();
    all_objects.push(root.as_object().clone());

    println!("/");
    walk(&mut all_objects, &repo, &root, "".to_string());

    std::thread::sleep_ms(1000000);

    // let dir = Dir::open(&mountpoint).unwrap();
    // let repo = Repository::open(repo_path).unwrap();
    // let fs = GitFS::new(repo, dir, None);
    // let options = ["-o", "ro", "-o", "fsname=gitfs"]
    //     .iter()
    //     .map(|x| x.as_ref())
    //     .collect::<Vec<&OsStr>>();
    // fuse::mount(fs, &mountpoint, &options).unwrap();
}
