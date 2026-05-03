// Metadata gathering for the summary post-processor: timestamp,
// chisel commit (best-effort via `git rev-parse HEAD`), machine
// info (os/arch/hostname). Serialized into the metadata block of
// the JSON output and into the markdown header.
//
// All fields fall back to safe defaults on failure rather than
// aborting — the post-processor still produces a useful summary
// even when run in environments without git or hostname access
// (tarballed CI, containers without /etc/hostname, etc.).

use serde::Serialize;
use std::path::Path;
use std::process::Command;

#[derive(Clone, Debug, Serialize)]
pub struct Metadata {
    pub timestamp: String,
    pub chisel_commit: String,
    pub machine: MachineInfo,
    pub post_processor_version: &'static str,
    pub criterion_dir: String,
    pub aux_metrics_path: String,
    pub cell_count: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct MachineInfo {
    pub os: String,
    pub arch: String,
    pub hostname: String,
}

/// Build a Metadata struct from the current environment. Best-effort
/// for git commit (returns "unknown" if git is unavailable) and
/// hostname (returns "unknown" if the syscall fails).
pub fn gather_metadata(
    criterion_dir: &Path,
    aux_metrics_path: &Path,
    cell_count: usize,
) -> Result<Metadata, Box<dyn std::error::Error>> {
    let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let chisel_commit = git_rev_parse_head().unwrap_or_else(|| "unknown".to_string());

    let machine = MachineInfo {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        hostname: hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "unknown".to_string()),
    };

    Ok(Metadata {
        timestamp,
        chisel_commit,
        machine,
        post_processor_version: env!("CARGO_PKG_VERSION"),
        criterion_dir: criterion_dir.display().to_string(),
        aux_metrics_path: aux_metrics_path.display().to_string(),
        cell_count,
    })
}

/// Run `git rev-parse HEAD` from the chisel repo root (CARGO_MANIFEST_DIR
/// is bench/, so the repo root is one level up). Returns None on any
/// failure — git missing, not a git repo, command non-zero exit. The
/// post-processor surfaces this as "unknown" rather than aborting.
fn git_rev_parse_head() -> Option<String> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let repo_root = Path::new(manifest_dir).parent()?;

    let output = Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(repo_root)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let sha = String::from_utf8(output.stdout).ok()?;
    Some(sha.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gather_metadata_populates_all_fields() {
        let metadata = gather_metadata(
            Path::new("target/criterion"),
            Path::new("bench/results/aux_metrics.jsonl"),
            42,
        )
        .unwrap();

        assert!(!metadata.timestamp.is_empty());
        assert!(metadata.timestamp.ends_with('Z'));
        assert!(!metadata.chisel_commit.is_empty()); // either a sha or "unknown"
        assert!(!metadata.machine.os.is_empty());
        assert!(!metadata.machine.arch.is_empty());
        assert!(!metadata.machine.hostname.is_empty());
        assert_eq!(metadata.criterion_dir, "target/criterion");
        assert_eq!(metadata.aux_metrics_path, "bench/results/aux_metrics.jsonl");
        assert_eq!(metadata.cell_count, 42);
        assert!(!metadata.post_processor_version.is_empty());
    }
}
