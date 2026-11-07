use std::{
    collections::HashSet,
    fs, io,
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::task::{ProjectMemoryFile, TaskHistoryEvent};

// The notes Altitude keeps in the directory: what it is, and how to search it.
// They are the machine's, refreshed at provisioning, so they are not the project's
// memory and no machine carries them.
const NOTES: [&str; 2] = ["README.md", "QMD.md"];

// A memory path is one a machine is about to write from a file another machine
// wrote, so it may never name anything but a place under the memory directory.
fn safe_path(path: &str) -> Option<&Path> {
    let path = Path::new(path);
    (!path.as_os_str().is_empty()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_))))
    .then_some(path)
}

fn millis(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or(0)
}

// What a directory holds now, as the state a task carries away with it: every regular
// file a person or an agent put there, with the time it was last written.
pub fn read(dir: &Path) -> io::Result<Vec<ProjectMemoryFile>> {
    let mut files = Vec::new();
    if !dir.is_dir() {
        return Ok(files);
    }
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        for entry in fs::read_dir(dir.join(&relative))? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let child = relative.join(&name);
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                pending.push(child);
                continue;
            }
            let is_note = relative.as_os_str().is_empty() && NOTES.contains(&name.as_str());
            if !metadata.is_file() || is_note {
                continue;
            }
            let Ok(contents) = fs::read_to_string(entry.path()) else {
                // Not text: whatever it is, it is not memory a person reads.
                continue;
            };
            let at_ms = metadata.modified().map(millis).unwrap_or(0);
            files.push(ProjectMemoryFile {
                path: child.to_string_lossy().into_owned(),
                contents: Some(contents),
                at_ms,
            });
        }
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

// Write a project's memory from its newest events: a path is written when the event
// about it is newer than the file already there, and removed when the newest event
// says so. Newer work on this machine therefore survives an older one arriving.
pub fn write(dir: &Path, state: &[ProjectMemoryFile]) -> io::Result<()> {
    for file in state {
        let Some(relative) = safe_path(&file.path) else {
            continue;
        };
        let target = dir.join(relative);
        let written = fs::metadata(&target)
            .and_then(|metadata| metadata.modified())
            .map(millis)
            .ok();
        if written.is_some_and(|now| now >= file.at_ms) {
            continue;
        }
        match &file.contents {
            Some(contents) => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&target, contents)?;
            }
            None => match fs::remove_file(&target) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            },
        }
    }
    Ok(())
}

// The events a machine records when it notices the directory changed: a write for
// each file whose contents are not what the newest event already says, and a removal
// for each path the state knows that the directory no longer has.
pub fn changes(
    project_id: &str,
    on_disk: &[ProjectMemoryFile],
    known: &[ProjectMemoryFile],
) -> Vec<TaskHistoryEvent> {
    let mut events = Vec::new();
    for file in on_disk {
        let unchanged = known
            .iter()
            .find(|known| known.path == file.path)
            .is_some_and(|known| known.contents == file.contents);
        if unchanged {
            continue;
        }
        events.push(write_event(project_id, file, file.contents.clone()));
    }
    let present: HashSet<&str> = on_disk.iter().map(|file| file.path.as_str()).collect();
    for file in known {
        if file.contents.is_some() && !present.contains(file.path.as_str()) {
            // A removal has no time of its own, so it is stamped when it is noticed:
            // it has to beat the write it undoes, whoever wrote that.
            events.push(write_event(project_id, file, None));
        }
    }
    events
}

fn write_event(
    project_id: &str,
    file: &ProjectMemoryFile,
    contents: Option<String>,
) -> TaskHistoryEvent {
    let written_at_ms = if contents.is_some() {
        file.at_ms
    } else {
        millis(SystemTime::now())
    };
    TaskHistoryEvent::ProjectMemoryFileWritten {
        project_id: project_id.to_owned(),
        path: file.path.clone(),
        contents,
        written_at_ms,
    }
}
