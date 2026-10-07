use anyhow::Result;
use std::path::Path;
use std::time::SystemTime;

/// Format a SystemTime as a human-readable "time ago" string with " ago" suffix
/// This is a TUI-specific wrapper that adds " ago" to the compact format from the parent
pub fn format_time_ago_verbose(time: SystemTime) -> String {
    let compact = crate::format_time_ago(time);
    if compact == "just now" {
        compact
    } else {
        format!("{} ago", compact)
    }
}

/// Format bytes as human-readable size
pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Calculate total size of a repository on disk: the bytes the repo itself
/// holds, excluding its `.git` directory and never following symlinks out of
/// the worktree.
pub fn get_repo_size(repo_path: &Path) -> Result<u64> {
    use std::fs;

    let mut total_size = 0u64;

    fn visit_dirs(dir: &Path, total: &mut u64) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;

            // Skip .git directory for more accurate size
            if entry.file_name() == ".git" {
                continue;
            }

            // Symlinks are counted as the links they are, never followed.
            // What a link in a worktree points at is decided by its target,
            // which may be a directory outside the repo (whose bytes the repo
            // doesn't hold) or the repo itself: descending through one of
            // those walks the same files over and over, and only stops once
            // the path it keeps extending grows too long for the OS to stat.
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                visit_dirs(&entry.path(), total)?;
            } else if let Ok(metadata) = entry.metadata() {
                *total += metadata.len();
            }
        }
        Ok(())
    }

    if !repo_path.is_dir() {
        return Ok(0);
    }
    visit_dirs(repo_path, &mut total_size)?;
    Ok(total_size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[cfg(unix)]
    #[test]
    fn repo_size_does_not_follow_symlinks() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("tracked.bin"), vec![b'x'; 4096]).unwrap();

        // Bytes that live outside the repo, reachable only through a link
        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("big.bin"), vec![b'y'; 1024 * 1024]).unwrap();
        std::os::unix::fs::symlink(&outside, repo.join("linked-dir")).unwrap();
        std::os::unix::fs::symlink(outside.join("big.bin"), repo.join("linked-file")).unwrap();

        // A link back at the repo: walking through it never reaches a bottom
        std::os::unix::fs::symlink(&repo, repo.join("loop")).unwrap();

        let size = get_repo_size(&repo).unwrap();
        assert!(
            (4096..8192).contains(&size),
            "size should be the repo's own bytes, got {}",
            size
        );
    }
}
