//! Commit drafts and running Git operations. No process I/O.
use std::{collections::HashMap, path::PathBuf};

/// Separates a commit draft's message from its context. Git's scissors line,
/// so `git-commit` highlighting treats the rest as context and diff.
pub const COMMIT_SEPARATOR: &str = "# ------------------------ >8 ------------------------";

/// What a commit draft shows. A commit is only created while HEAD and the
/// index still match it, so Git records exactly what was reviewed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// HEAD commit; `None` before the first commit.
    pub head: Option<String>,
    /// Tree of the index (`git write-tree`).
    pub tree: String,
    /// Repository-relative paths the commit changes.
    pub staged: Vec<String>,
}

/// A commit message being written in a scratch buffer, bound to one repository
/// and branch when opened.
#[derive(Clone, Debug)]
pub struct Draft {
    pub root: PathBuf,
    pub branch: String,
    pub snapshot: Snapshot,
    /// A commit is running; another `:w` must not start a second one.
    pub committing: bool,
    /// Changes on every commit attempt and update, so that a slower
    /// background update cannot replace newer content.
    pub revision: u64,
    /// The review discussion selected when the draft opened, as
    /// `@author on path:line`.
    pub discussion: Option<String>,
}

/// A running write operation (staging, commit or push) in one repository.
pub struct Running {
    pub what: &'static str,
    pub task: tokio::task::AbortHandle,
}

#[derive(Default)]
pub struct State {
    /// Commit drafts by scratch document.
    pub drafts: HashMap<crate::DocumentId, Draft>,
    /// At most one write operation per repository root.
    pub running: HashMap<PathBuf, Running>,
}

impl State {
    /// The running operation in `root`, ignoring finished tasks.
    pub fn running(&mut self, root: &PathBuf) -> Option<&'static str> {
        if self.running.get(root).is_some_and(|r| r.task.is_finished()) {
            self.running.remove(root);
        }
        self.running.get(root).map(|r| r.what)
    }
}

/// The text above the separator line, without trailing whitespace and
/// surrounding blank lines. `Err` explains why nothing may be committed.
pub fn commit_message(text: &str) -> Result<String, &'static str> {
    let mut message = Vec::new();
    let mut separated = false;
    for line in text.lines() {
        if line.trim_end() == COMMIT_SEPARATOR {
            separated = true;
            break;
        }
        message.push(line.trim_end());
    }
    if !separated {
        return Err("The scissors line was removed; restore it so the context is not committed");
    }
    let message = message.join("\n").trim_matches('\n').to_owned();
    if message.trim().is_empty() {
        return Err("Commit message is empty; write it above the scissors line");
    }
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_is_the_text_above_the_scissors_line() {
        let context = format!("{COMMIT_SEPARATOR}\n# context\ndiff --git a/x b/x\n");
        assert_eq!(
            commit_message(&format!("\nfix: x  \n\nbody #1\n\n{context}")).unwrap(),
            "fix: x\n\nbody #1"
        );
        assert_eq!(
            commit_message(&format!("  \n\n{context}")).unwrap_err(),
            "Commit message is empty; write it above the scissors line"
        );
        assert!(commit_message("fix: x\n# context\n")
            .unwrap_err()
            .contains("scissors line was removed"));
    }

    #[tokio::test]
    async fn finished_operations_do_not_block() {
        let mut state = State::default();
        let root = PathBuf::from("/r");
        let task = tokio::spawn(async {});
        state.running.insert(
            root.clone(),
            Running {
                what: "commit",
                task: task.abort_handle(),
            },
        );
        task.await.unwrap();
        assert_eq!(state.running(&root), None);
        let pending = tokio::spawn(std::future::pending::<()>());
        state.running.insert(
            root.clone(),
            Running {
                what: "push",
                task: pending.abort_handle(),
            },
        );
        assert_eq!(state.running(&root), Some("push"));
        pending.abort();
    }
}
