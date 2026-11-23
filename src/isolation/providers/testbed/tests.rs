use std::{fs, os::unix::fs::symlink, path::Path};

use super::*;
use crate::testkit;

const ID: &str = "sandbox";

fn testbed(root: &Path) -> TestbedIsolation {
    let testbed = TestbedIsolation::new(&testkit::config(root));
    fs::create_dir_all(testbed.sandbox_root(ID)).unwrap();
    testbed
}

fn checkpoint_of(testbed: &TestbedIsolation, archive: &Path) {
    testbed.archive_sandbox(ID, None, archive).unwrap();
}

#[test]
fn a_symlink_to_a_directory_and_a_dangling_one_survive_a_round_trip() {
    let scratch = tempfile::tempdir().unwrap();
    let source = testbed(&scratch.path().join("source"));
    let root = source.sandbox_root(ID);
    fs::create_dir_all(root.join("modules/real")).unwrap();
    fs::write(root.join("modules/real/file"), "kept").unwrap();
    symlink("real", root.join("modules/linked")).unwrap();
    symlink("nowhere", root.join("modules/dangling")).unwrap();
    let archive = scratch.path().join("checkpoint.tar");
    checkpoint_of(&source, &archive);

    let destination = testbed(&scratch.path().join("destination"));
    destination.restore_sandbox(ID, None, &archive).unwrap();

    let restored = destination.sandbox_root(ID).join("modules");
    let linked = fs::symlink_metadata(restored.join("linked")).unwrap();
    assert!(linked.file_type().is_symlink());
    assert_eq!(
        fs::read_link(restored.join("linked")).unwrap(),
        Path::new("real")
    );
    assert_eq!(
        fs::read_to_string(restored.join("linked/file")).unwrap(),
        "kept"
    );
    assert_eq!(
        fs::read_link(restored.join("dangling")).unwrap(),
        Path::new("nowhere")
    );
}

#[test]
fn a_link_out_of_the_tree_is_archived_as_a_link_not_as_what_it_points_at() {
    let scratch = tempfile::tempdir().unwrap();
    let outside = scratch.path().join("outside.txt");
    fs::write(&outside, "secret").unwrap();
    let source = testbed(&scratch.path().join("source"));
    symlink(&outside, source.sandbox_root(ID).join("escape")).unwrap();
    let archive = scratch.path().join("checkpoint.tar");
    checkpoint_of(&source, &archive);

    let mut tar = Archive::new(fs::File::open(&archive).unwrap());
    let entry = tar.entries().unwrap().next().unwrap().unwrap();
    assert!(entry.header().entry_type().is_symlink());
    assert_eq!(entry.header().size().unwrap(), 0);
}

#[test]
fn a_restore_replaces_what_stands_where_an_entry_lands() {
    let scratch = tempfile::tempdir().unwrap();
    let source = testbed(&scratch.path().join("source"));
    let root = source.sandbox_root(ID);
    fs::create_dir_all(root.join("modules/real")).unwrap();
    symlink("real", root.join("modules/linked")).unwrap();
    fs::write(root.join("modules/file"), "from the checkpoint").unwrap();
    let archive = scratch.path().join("checkpoint.tar");
    checkpoint_of(&source, &archive);

    let destination = testbed(&scratch.path().join("destination"));
    let existing = destination.sandbox_root(ID).join("modules");
    fs::create_dir_all(existing.join("linked/inner")).unwrap();
    fs::create_dir_all(existing.join("file")).unwrap();
    fs::write(existing.join("real"), "in the way").unwrap();
    destination.restore_sandbox(ID, None, &archive).unwrap();

    assert!(
        fs::symlink_metadata(existing.join("linked"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_to_string(existing.join("file")).unwrap(),
        "from the checkpoint"
    );
}

#[test]
fn a_restore_does_not_write_through_a_link_already_in_place() {
    let scratch = tempfile::tempdir().unwrap();
    let source = testbed(&scratch.path().join("source"));
    fs::write(source.sandbox_root(ID).join("file"), "from the checkpoint").unwrap();
    let archive = scratch.path().join("checkpoint.tar");
    checkpoint_of(&source, &archive);

    let destination = testbed(&scratch.path().join("destination"));
    let outside = scratch.path().join("outside.txt");
    fs::write(&outside, "untouched").unwrap();
    symlink(&outside, destination.sandbox_root(ID).join("file")).unwrap();
    destination.restore_sandbox(ID, None, &archive).unwrap();

    assert_eq!(fs::read_to_string(&outside).unwrap(), "untouched");
    assert_eq!(
        fs::read_to_string(destination.sandbox_root(ID).join("file")).unwrap(),
        "from the checkpoint"
    );
}
