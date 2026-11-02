//! Keep cwd-relative mirrors out of Git without changing global ignore
//! settings or touching unrelated ancestors outside the repository.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const GIT_IGNORE_TIMEOUT: Duration = Duration::from_secs(2);

fn repository_root(cwd: &Path) -> Option<&Path> {
    // A normal checkout has a .git directory; worktrees and submodules
    // have a .git file. Both mark the boundary for our upward search.
    cwd.ancestors().find(|dir| dir.join(".git").exists())
}

pub(super) fn ignore_local_mirror(cwd: &Path) -> Result<(), String> {
    let Some(root) = repository_root(cwd) else {
        return Ok(());
    };
    // Git reads every ancestor's ignore file, not only our append target.
    // Reject FIFOs and other nonregular entries before launching it.
    let path = checked_ignore_path(cwd, root)?;
    if is_ignored(cwd, GIT_IGNORE_TIMEOUT)? {
        return Ok(());
    }
    append_ignore(&path)
}

fn is_ignored(cwd: &Path, timeout: Duration) -> Result<bool, String> {
    // Respect existing patterns, including broader ignores, nested
    // negations, and .git/info/exclude. --no-index also handles mirrors
    // that were already tracked (ignore rules never untrack files).
    // Query the directory itself: a trailing slash can match a contents-
    // only pattern even when a negated subtree exposes mirror files.
    let mut command = Command::new("git");
    command
        .args(["check-ignore", "--no-index", "--quiet", "--", ".figmog"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|_| "could not check Git ignore rules; ensure Git is available".to_string())?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => {
                // Git may be a launcher that spawns the actual binary.
                // SAFETY: process_group(0) gave this child a new group
                // whose ID is its PID. Only that group is terminated.
                #[cfg(unix)]
                unsafe {
                    libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                return Err(if result.is_err() {
                    "could not check Git ignore rules".into()
                } else {
                    "Git ignore check timed out".into()
                });
            }
        }
    };
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err("could not check Git ignore rules; ensure Git is available".into()),
    }
}

fn append_ignore(path: &Path) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    // Read and append through the same descriptor. The no-follow open
    // closes the symlink-swap gap after checked_ignore_path; nonblocking
    // mode lets us reject a swapped FIFO before attempting to read it.
    let mut file = options
        .open(path)
        .map_err(|error| format!("opening {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("checking {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)
        .map_err(|error| format!("reading {}: {error}", path.display()))?;
    let newline = if contents.windows(2).any(|pair| pair == b"\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut addition = String::new();
    if !contents.is_empty() && !contents.ends_with(b"\n") {
        addition.push_str(newline);
    }
    addition.push_str(".figmog/");
    addition.push_str(newline);
    file.write_all(addition.as_bytes())
        .map_err(|error| format!("writing {}: {error}", path.display()))
}

fn checked_ignore_path(cwd: &Path, root: &Path) -> Result<PathBuf, String> {
    let mut closest = None;
    for dir in cwd.ancestors() {
        let path = dir.join(".gitignore");
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(format!("{} is not a regular file", path.display()));
                }
                closest.get_or_insert(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("checking {}: {error}", path.display())),
        }
        if dir == root {
            return Ok(closest.unwrap_or_else(|| root.join(".gitignore")));
        }
    }
    unreachable!("repository root is an ancestor of cwd")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_symlink_swap_after_the_precheck_cannot_redirect_the_append() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(".gitignore");
        fs::write(&path, "existing\n").unwrap();
        let checked = checked_ignore_path(root.path(), root.path()).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("ignore");
        fs::write(&target, "untouched\n").unwrap();
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();

        assert!(append_ignore(&checked).is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "untouched\n");
    }
}
