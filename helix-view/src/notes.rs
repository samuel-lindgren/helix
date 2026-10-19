//! Project notes: tasks and notes captured from code, kept in one Markdown
//! file per project.

use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct Config {
    /// Directory holding one `<project>.md` file per project. `~` expands to
    /// the home directory. Defaults to `~/notes/projects`.
    pub directory: PathBuf,
    /// The heading that captured entries are added under. Created as a
    /// second-level heading when the notes file lacks it. Defaults to `Inbox`.
    pub heading: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            directory: PathBuf::from("~/notes/projects"),
            heading: "Inbox".to_owned(),
        }
    }
}

/// Where a capture draft came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capture {
    /// The project's workspace root.
    pub project: PathBuf,
    /// The notes file that the entry goes to.
    pub notes_file: PathBuf,
    /// The rendered `code:` link and the anchored line below the entry text.
    pub anchor: String,
}

#[derive(Debug, Default)]
pub struct State {
    /// Capture drafts by scratch document.
    pub captures: HashMap<crate::DocumentId, Capture>,
}
