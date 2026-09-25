//! Child processes for background features: a program with a fixed argument
//! list (never a shell), no terminal and no prompts, bounded output and time.
//!
//! On Unix, children run in their own session without a controlling terminal,
//! so a credential, passphrase or pinentry prompt that tries to open the
//! terminal fails instead of drawing over the editor. A timeout or a dropped
//! (cancelled) run terminates the whole process group, including hooks and ssh,
//! with SIGTERM first so that Git can remove its lock files.
use anyhow::{bail, ensure, Context as _};
use helix_view::review::safe_text;
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

/// Output limit per stream.
pub(crate) const LIMIT: u64 = 16 * 1024 * 1024;

pub(crate) struct Output {
    pub success: bool,
    pub stdout: Vec<u8>,
    /// Terminal-safe.
    pub stderr: String,
}

impl Output {
    /// Both streams as terminal-safe text, stderr first, for failure reports.
    pub fn message(&self) -> String {
        let stdout = safe_text(&String::from_utf8_lossy(&self.stdout));
        [self.stderr.trim(), stdout.trim()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A child's process group, terminated when dropped before the child was
/// waited for. Never signalled after waiting, when its id may be reused.
struct Group(Option<u32>);

impl Group {
    #[cfg(unix)]
    fn signal(&self, signal: i32) {
        if let Some(pid) = self.0.and_then(|pid| i32::try_from(pid).ok()) {
            // The child leads its own session and process group.
            unsafe { libc::kill(-pid, signal) };
        }
    }
    fn terminate(&self) {
        #[cfg(unix)]
        self.signal(libc::SIGTERM);
    }
    fn kill(&self) {
        #[cfg(unix)]
        self.signal(libc::SIGKILL);
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// Run `program` in `root` and wait for it. `Err` for spawn failures, timeouts
/// and oversized output; a failed exit status is reported in `Output`.
pub(crate) async fn output(
    root: &Path,
    program: &str,
    args: &[&str],
    input: Option<Vec<u8>>,
    timeout: Duration,
) -> anyhow::Result<Output> {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_EDITOR", "false")
        .env("GIT_SEQUENCE_EDITOR", "false")
        .env("GIT_PAGER", "cat")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_PAGER", "cat")
        .env_remove("GH_REPO")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GPG_TTY")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Tests never read the developer's Git configuration, hooks or keys.
    #[cfg(test)]
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com");
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    #[cfg(not(unix))]
    command.kill_on_drop(true);
    let mut child = command
        .spawn()
        .with_context(|| format!("Cannot run {program}"))?;
    let mut group = Group(child.id());
    let mut stdout = child.stdout.take().unwrap().take(LIMIT + 1);
    let mut stderr = child.stderr.take().unwrap().take(LIMIT + 1);
    let mut stdin = child.stdin.take();
    let result = tokio::time::timeout(timeout, async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        tokio::try_join!(
            async {
                if let (Some(mut stdin), Some(input)) = (stdin.take(), input) {
                    // A child may exit without reading its input, e.g. `git commit
                    // -F -` after a failing hook; its exit status tells the story.
                    let written = match stdin.write_all(&input).await {
                        Ok(()) => stdin.shutdown().await,
                        Err(err) => Err(err),
                    };
                    match written {
                        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => {}
                        written => written?,
                    }
                }
                Ok::<_, std::io::Error>(())
            },
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err)
        )?;
        ensure!(
            out.len() as u64 <= LIMIT && err.len() as u64 <= LIMIT,
            "{program} output exceeded the size limit"
        );
        let status = child.wait().await?;
        Ok((status, out, err))
    })
    .await;
    let Ok(result) = result else {
        group.terminate();
        if tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .is_err()
        {
            group.kill();
        }
        group.0 = None;
        bail!("{program} timed out after {}s", timeout.as_secs());
    };
    // Errors before waiting drop `group`, which terminates the child.
    let (status, stdout, stderr) = result?;
    group.0 = None;
    Ok(Output {
        success: status.success(),
        stdout,
        stderr: safe_text(&String::from_utf8_lossy(&stderr)),
    })
}

/// Run `program` and return its UTF-8 standard output; a failed exit status
/// becomes an error with the start of its standard error.
pub(crate) async fn run(
    root: &Path,
    program: &str,
    args: &[&str],
    input: Option<Vec<u8>>,
    timeout: Duration,
) -> anyhow::Result<String> {
    let out = output(root, program, args, input, timeout).await?;
    ensure!(
        out.success,
        "{program}: {}",
        out.stderr.chars().take(500).collect::<String>()
    );
    Ok(String::from_utf8(out.stdout)?)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn arguments_are_not_shell_code_and_failures_keep_output() {
        let dir = tempfile::tempdir().unwrap();
        let payload = "$(touch injected); `touch injected`\n--evil";
        let second = Duration::from_secs(5);
        let result = run(dir.path(), "printf", &["%s", payload], None, second)
            .await
            .unwrap();
        assert_eq!(result, payload);
        assert!(!dir.path().join("injected").exists());
        let out = output(
            dir.path(),
            "sh",
            &["-c", "echo out; echo 'err\x1b[31m' >&2; exit 3"],
            None,
            second,
        )
        .await
        .unwrap();
        assert!(!out.success);
        assert_eq!(out.message(), "err[31m\nout");
        let err = run(
            dir.path(),
            "sh",
            &["-c", "echo nope >&2; exit 1"],
            None,
            second,
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "sh: nope\n");
    }

    /// A child that exits without reading its input still reports its output.
    #[tokio::test]
    async fn unread_input_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let out = output(
            dir.path(),
            "sh",
            &["-c", "echo refused >&2; exit 1"],
            Some(vec![b'x'; 1 << 20]),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(!out.success);
        assert_eq!(out.stderr, "refused\n");
    }

    /// No controlling terminal: a prompt that opens /dev/tty fails at once.
    #[tokio::test]
    async fn children_have_no_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let out = output(
            dir.path(),
            "sh",
            &["-c", "exec 3</dev/tty && echo tty"],
            None,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(!out.success);
        assert!(!String::from_utf8_lossy(&out.stdout).contains("tty"));
    }

    /// A timeout terminates the child and everything it started.
    #[tokio::test]
    async fn timeouts_terminate_the_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survived");
        let script = format!("(sleep 2; touch '{}') & wait", marker.display());
        let err = output(
            dir.path(),
            "sh",
            &["-c", &script],
            None,
            Duration::from_millis(200),
        )
        .await
        .err()
        .unwrap();
        assert!(err.to_string().contains("timed out"), "{err}");
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(!marker.exists());
    }
}
