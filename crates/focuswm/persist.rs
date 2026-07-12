//! Persistence of the task list to a JSON file in the user's config dir.

use std::path::PathBuf;

use focuswm_shell::TaskList;

/// Path of the persisted state file (`~/.config/focuswm/focuswm.json`), falling
/// back to the current directory.
pub fn state_path() -> PathBuf {
    if let Some(dir) = dirs::config_dir() {
        let dir = dir.join("focuswm");
        let _ = std::fs::create_dir_all(&dir);
        return dir.join("focuswm.json");
    }
    PathBuf::from("focuswm.json")
}

/// Load the task list from disk, or a fresh empty list if none/unreadable.
pub fn load() -> TaskList {
    let path = state_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<TaskList>(&text) {
            Ok(mut list) => {
                list.reindex_after_load();
                list
            }
            Err(err) => {
                log::warn!("could not parse {}: {err}; starting fresh", path.display());
                TaskList::new()
            }
        },
        Err(_) => TaskList::new(),
    }
}

/// Save the task list to disk (best-effort; logs on failure). Writes to a
/// sibling temp file and renames it into place, so a crash or power loss
/// mid-write can't truncate the existing state file (a partial JSON would be
/// discarded as unparseable on the next start, losing every task).
pub fn save(list: &TaskList) {
    let path = state_path();
    let text = match serde_json::to_string_pretty(list) {
        Ok(text) => text,
        Err(err) => {
            log::warn!("could not serialize task list: {err}");
            return;
        }
    };
    let tmp = path.with_extension("json.tmp");
    let result = std::fs::write(&tmp, text)
        .and_then(|()| std::fs::File::open(&tmp)?.sync_all())
        .and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(err) = result {
        log::warn!("could not write {}: {err}", path.display());
        let _ = std::fs::remove_file(&tmp);
    }
}
