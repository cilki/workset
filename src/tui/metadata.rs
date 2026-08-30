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

/// Calculate total size of a repository on disk
pub fn get_repo_size(repo_path: &Path) -> Result<u64> {
    use std::fs;

    let mut total_size = 0u64;

    fn visit_dirs(dir: &Path, total: &mut u64) -> Result<()> {
        if dir.is_dir() {
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();

                // Skip .git directory for more accurate size
                if path.file_name().and_then(|n| n.to_str()) == Some(".git") {
                    continue;
                }

                if path.is_dir() {
                    visit_dirs(&path, total)?;
                } else if let Ok(metadata) = fs::metadata(&path) {
                    *total += metadata.len();
                }
            }
        }
        Ok(())
    }

    visit_dirs(repo_path, &mut total_size)?;
    Ok(total_size)
}
