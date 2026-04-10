use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use helix_event::register_hook;
use helix_view::events::DocumentDidOpen;
use helix_view::handlers::Handlers;

const RECENT_FILES_FILENAME: &str = "recent_files";
const MAX_RECENT_FILES: usize = 100;

fn recent_files_path() -> PathBuf {
    helix_loader::cache_dir().join(RECENT_FILES_FILENAME)
}

/// Read the recent files file and return the raw list of paths (no existence
/// filtering). Returns an empty vec on any read or parse error.
fn read_lines() -> Vec<PathBuf> {
    let path = recent_files_path();
    let contents = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                log::debug!("failed to read {}: {err}", path.display());
            }
            return Vec::new();
        }
    };
    contents
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .take(MAX_RECENT_FILES)
        .collect()
}

/// Load the list of recent files for display. Non-existent paths are filtered
/// out but not removed from the on-disk file (so a temporarily-missing file
/// won't lose its history).
pub fn load() -> Vec<PathBuf> {
    read_lines()
        .into_iter()
        .filter(|p| matches!(p.try_exists(), Ok(true)))
        .collect()
}

/// Atomically write the recent files list to disk. Any failure is logged but
/// swallowed — a recent-files write must never crash the editor.
fn save(entries: &[PathBuf]) {
    let target = recent_files_path();
    let Some(parent) = target.parent() else {
        return;
    };
    if let Err(err) = fs::create_dir_all(parent) {
        log::warn!("failed to create {}: {err}", parent.display());
        return;
    }

    let tmp = target.with_extension("tmp");
    let write_result = (|| -> std::io::Result<()> {
        let mut file = fs::File::create(&tmp)?;
        for entry in entries {
            file.write_all(entry.to_string_lossy().as_bytes())?;
            file.write_all(b"\n")?;
        }
        file.sync_all()
    })();

    if let Err(err) = write_result {
        log::warn!("failed to write {}: {err}", tmp.display());
        let _ = fs::remove_file(&tmp);
        return;
    }

    if let Err(err) = fs::rename(&tmp, &target) {
        log::warn!(
            "failed to rename {} -> {}: {err}",
            tmp.display(),
            target.display()
        );
        let _ = fs::remove_file(&tmp);
    }
}

fn record_open(path: &Path) {
    if !path.is_absolute() {
        return;
    }
    let canonical = helix_stdx::path::canonicalize(path);

    let mut entries = read_lines();
    entries.retain(|p| p != &canonical);
    entries.insert(0, canonical);
    entries.truncate(MAX_RECENT_FILES);
    save(&entries);
}

pub(super) fn register_hooks(_handlers: &Handlers) {
    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        let doc = doc!(event.editor, &event.doc);
        if let Some(path) = doc.path() {
            record_open(path);
        }
        Ok(())
    });
}
