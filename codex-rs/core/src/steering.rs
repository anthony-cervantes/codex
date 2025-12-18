//! Steering file discovery and loading.
//!
//! Steering files provide additional persistent guidance to the model. They are
//! discovered from two fixed locations:
//!
//! - Global: `$CODEX_HOME/steering/*.md`
//! - Project: `<repo_root>/.codex/steering/*.md`
//!
//! Both directories are scanned non-recursively. Files are loaded in a stable
//! order so that later files can override earlier ones:
//!
//! - Global steering first (lexicographic by filename)
//! - Project steering second (lexicographic by filename)
//!
//! The repository root detection matches the logic used for project-level
//! `AGENTS.md` discovery: walk upwards from the current working directory until
//! a `.git` directory or file is found; otherwise treat the current working
//! directory as the root.

use crate::config::Config;
use dunce::canonicalize as normalize_path;
use std::path::Path;
use std::path::PathBuf;
use tokio::io::AsyncReadExt;

pub const PROJECT_STEERING_DIR: &str = ".codex/steering";
pub const GLOBAL_STEERING_DIR: &str = "steering";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteeringScope {
    Global,
    Project,
}

impl SteeringScope {
    pub fn as_str(self) -> &'static str {
        match self {
            SteeringScope::Global => "global",
            SteeringScope::Project => "project",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteeringDiscovery {
    pub codex_home: PathBuf,
    pub repo_root: PathBuf,
    pub global_dir: PathBuf,
    pub project_dir: PathBuf,
    /// Discovered steering files in deterministic load order.
    pub files: Vec<SteeringFile>,
    pub global_dir_state: DirState,
    pub project_dir_state: DirState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirState {
    Missing,
    Present,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteeringFile {
    pub scope: SteeringScope,
    /// Absolute path to the file on disk.
    pub path: PathBuf,
    /// Display path used in injected prompt headers and CLI output.
    pub display_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteeringLoadResult {
    pub enabled: bool,
    pub max_bytes: usize,
    pub discovery: SteeringDiscovery,
    pub files: Vec<SteeringFileOutcome>,
    pub combined: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteeringFileOutcome {
    pub scope: SteeringScope,
    pub path: PathBuf,
    pub display_path: String,
    pub status: SteeringFileStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteeringFileStatus {
    Included { bytes: usize, truncated: bool },
    Omitted { reason: OmissionReason },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OmissionReason {
    Disabled,
    Empty,
    NonUtf8,
    OverBudget,
    Io(String),
}

impl OmissionReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            OmissionReason::Disabled => "disabled",
            OmissionReason::Empty => "empty",
            OmissionReason::NonUtf8 => "non-utf8",
            OmissionReason::OverBudget => "over-budget",
            OmissionReason::Io(_) => "io-error",
        }
    }
}

pub fn discover_steering_files(config: &Config) -> std::io::Result<SteeringDiscovery> {
    let repo_root = discover_repo_root(&config.cwd)?;
    let global_dir = config.codex_home.join(GLOBAL_STEERING_DIR);
    let project_dir = repo_root.join(PROJECT_STEERING_DIR);

    let (global_state, mut global_files) = list_md_files(&global_dir, SteeringScope::Global)?;
    let (project_state, mut project_files) = list_md_files(&project_dir, SteeringScope::Project)?;

    global_files.sort_by(|a, b| a.display_path.cmp(&b.display_path));
    project_files.sort_by(|a, b| a.display_path.cmp(&b.display_path));

    let mut files = Vec::with_capacity(global_files.len() + project_files.len());
    files.extend(global_files);
    files.extend(project_files);

    Ok(SteeringDiscovery {
        codex_home: config.codex_home.clone(),
        repo_root,
        global_dir,
        project_dir,
        files,
        global_dir_state: global_state,
        project_dir_state: project_state,
    })
}

pub async fn load_steering_docs(config: &Config) -> std::io::Result<SteeringLoadResult> {
    let discovery = discover_steering_files(config)?;
    let max_bytes = config.steering_doc_max_bytes;

    if !config.steering_enabled || max_bytes == 0 {
        let files = discovery
            .files
            .iter()
            .map(|f| SteeringFileOutcome {
                scope: f.scope,
                path: f.path.clone(),
                display_path: f.display_path.clone(),
                status: SteeringFileStatus::Omitted {
                    reason: OmissionReason::Disabled,
                },
            })
            .collect();
        return Ok(SteeringLoadResult {
            enabled: false,
            max_bytes,
            discovery,
            files,
            combined: None,
        });
    }

    let mut remaining: u64 = max_bytes as u64;
    let mut parts: Vec<String> = Vec::new();
    let mut outcomes: Vec<SteeringFileOutcome> = Vec::new();

    for file in &discovery.files {
        if remaining == 0 {
            outcomes.push(SteeringFileOutcome {
                scope: file.scope,
                path: file.path.clone(),
                display_path: file.display_path.clone(),
                status: SteeringFileStatus::Omitted {
                    reason: OmissionReason::OverBudget,
                },
            });
            continue;
        }

        let opened = tokio::fs::File::open(&file.path).await;
        let file_handle = match opened {
            Ok(f) => f,
            Err(err) => {
                outcomes.push(SteeringFileOutcome {
                    scope: file.scope,
                    path: file.path.clone(),
                    display_path: file.display_path.clone(),
                    status: SteeringFileStatus::Omitted {
                        reason: OmissionReason::Io(err.to_string()),
                    },
                });
                continue;
            }
        };

        let file_size = file_handle.metadata().await.map(|md| md.len()).unwrap_or(0);
        let mut reader = tokio::io::BufReader::new(file_handle).take(remaining);
        let mut data: Vec<u8> = Vec::new();
        reader.read_to_end(&mut data).await?;

        let truncated_by_budget = file_size > remaining;

        let text = match std::string::String::from_utf8(data.clone()) {
            Ok(s) => s,
            Err(err) => {
                // If we're truncating due to budget, allow dropping an incomplete
                // trailing UTF-8 sequence so we can still include a valid prefix.
                if truncated_by_budget {
                    let utf8_err = err.utf8_error();
                    if utf8_err.error_len().is_none() {
                        let valid = utf8_err.valid_up_to();
                        let prefix = &data[..valid];
                        match std::str::from_utf8(prefix) {
                            Ok(s) => s.to_string(),
                            Err(_) => {
                                outcomes.push(SteeringFileOutcome {
                                    scope: file.scope,
                                    path: file.path.clone(),
                                    display_path: file.display_path.clone(),
                                    status: SteeringFileStatus::Omitted {
                                        reason: OmissionReason::NonUtf8,
                                    },
                                });
                                continue;
                            }
                        }
                    } else {
                        outcomes.push(SteeringFileOutcome {
                            scope: file.scope,
                            path: file.path.clone(),
                            display_path: file.display_path.clone(),
                            status: SteeringFileStatus::Omitted {
                                reason: OmissionReason::NonUtf8,
                            },
                        });
                        continue;
                    }
                } else {
                    outcomes.push(SteeringFileOutcome {
                        scope: file.scope,
                        path: file.path.clone(),
                        display_path: file.display_path.clone(),
                        status: SteeringFileStatus::Omitted {
                            reason: OmissionReason::NonUtf8,
                        },
                    });
                    continue;
                }
            }
        };

        if text.trim().is_empty() {
            outcomes.push(SteeringFileOutcome {
                scope: file.scope,
                path: file.path.clone(),
                display_path: file.display_path.clone(),
                status: SteeringFileStatus::Omitted {
                    reason: OmissionReason::Empty,
                },
            });
            continue;
        }

        let included_bytes = text.len();
        let header = format!(
            "[Steering: scope={} file={}{}]",
            file.scope.as_str(),
            file.display_path,
            if truncated_by_budget {
                " truncated=true"
            } else {
                ""
            }
        );
        parts.push(format!("{header}\n{text}"));

        outcomes.push(SteeringFileOutcome {
            scope: file.scope,
            path: file.path.clone(),
            display_path: file.display_path.clone(),
            status: SteeringFileStatus::Included {
                bytes: included_bytes,
                truncated: truncated_by_budget,
            },
        });

        remaining = remaining.saturating_sub(included_bytes as u64);
    }

    let mut combined = if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    };

    let omitted_over_budget: Vec<(SteeringScope, String)> = outcomes
        .iter()
        .filter_map(|outcome| match &outcome.status {
            SteeringFileStatus::Omitted {
                reason: OmissionReason::OverBudget,
            } => Some((outcome.scope, outcome.display_path.clone())),
            _ => None,
        })
        .collect();

    if !omitted_over_budget.is_empty() {
        let note = format_omission_note(max_bytes, &omitted_over_budget);
        combined = Some(match combined {
            Some(s) => format!("{s}\n\n{note}"),
            None => note,
        });
    }

    Ok(SteeringLoadResult {
        enabled: true,
        max_bytes,
        discovery,
        files: outcomes,
        combined,
    })
}

fn format_omission_note(max_bytes: usize, omitted: &[(SteeringScope, String)]) -> String {
    let mut lines = Vec::with_capacity(2 + omitted.len());
    lines.push("[Steering: note]".to_string());
    lines.push(format!(
        "Omitted {} file(s) due to steering.doc_max_bytes={max_bytes}.",
        omitted.len()
    ));
    for (scope, display_path) in omitted {
        lines.push(format!(
            "- scope={} file={display_path} reason=over-budget",
            scope.as_str()
        ));
    }
    lines.join("\n")
}

fn list_md_files(
    dir: &Path,
    scope: SteeringScope,
) -> std::io::Result<(DirState, Vec<SteeringFile>)> {
    let read_dir = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok((DirState::Missing, Vec::new()));
        }
        Err(err) => return Ok((DirState::Error(err.to_string()), Vec::new())),
    };

    let mut out = Vec::new();
    for entry in read_dir {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!("Failed to read steering directory entry: {err}");
                continue;
            }
        };
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "md") {
            continue;
        }

        // Only include plain files; ignore symlinks to avoid path traversal.
        let md = match std::fs::symlink_metadata(&path) {
            Ok(md) => md,
            Err(err) => {
                tracing::warn!("Failed to stat steering file {}: {err}", path.display());
                continue;
            }
        };
        if !md.file_type().is_file() {
            continue;
        }

        let display_path = match scope {
            SteeringScope::Global => {
                let file_name = path
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.to_string_lossy().to_string());
                format!("$CODEX_HOME/{GLOBAL_STEERING_DIR}/{file_name}")
            }
            SteeringScope::Project => {
                let file_name = path
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.to_string_lossy().to_string());
                format!("{PROJECT_STEERING_DIR}/{file_name}")
            }
        };

        out.push(SteeringFile {
            scope,
            path,
            display_path,
        });
    }

    Ok((DirState::Present, out))
}

fn discover_repo_root(cwd: &Path) -> std::io::Result<PathBuf> {
    let mut dir = cwd.to_path_buf();
    if let Ok(canon) = normalize_path(&dir) {
        dir = canon;
    }

    let mut cursor = dir;
    while let Some(parent) = cursor.parent() {
        let git_marker = cursor.join(".git");
        let git_exists = match std::fs::metadata(&git_marker) {
            Ok(_) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(e),
        };

        if git_exists {
            return Ok(cursor);
        }

        cursor = parent.to_path_buf();
    }

    Ok(cwd.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigOverrides;
    use crate::config::ConfigToml;
    use pretty_assertions::assert_eq;
    use std::fs;
    use tempfile::TempDir;

    fn make_config(codex_home: &TempDir, cwd: PathBuf) -> Config {
        let mut config = Config::load_from_base_config_with_overrides(
            ConfigToml::default(),
            ConfigOverrides::default(),
            codex_home.path().to_path_buf(),
        )
        .expect("defaults for test should always succeed");
        config.cwd = cwd;
        config.steering_enabled = true;
        config.steering_doc_max_bytes = 4096;
        config
    }

    #[test]
    fn discovers_files_in_stable_order() {
        let codex_home = tempfile::tempdir().expect("codex home");
        let repo = tempfile::tempdir().expect("repo");
        fs::write(repo.path().join(".git"), "gitdir: /tmp/fake\n").unwrap();

        let global_dir = codex_home.path().join("steering");
        let project_dir = repo.path().join(".codex/steering");
        fs::create_dir_all(&global_dir).unwrap();
        fs::create_dir_all(&project_dir).unwrap();

        fs::write(global_dir.join("b.md"), "global b").unwrap();
        fs::write(global_dir.join("a.md"), "global a").unwrap();
        fs::write(project_dir.join("02.md"), "proj 02").unwrap();
        fs::write(project_dir.join("01.md"), "proj 01").unwrap();

        let cfg = make_config(&codex_home, repo.path().to_path_buf());
        let discovery = discover_steering_files(&cfg).expect("discover");
        let display: Vec<String> = discovery
            .files
            .iter()
            .map(|f| f.display_path.clone())
            .collect();
        assert_eq!(
            display,
            vec![
                "$CODEX_HOME/steering/a.md".to_string(),
                "$CODEX_HOME/steering/b.md".to_string(),
                ".codex/steering/01.md".to_string(),
                ".codex/steering/02.md".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn project_files_override_global_by_later_placement() {
        let codex_home = tempfile::tempdir().expect("codex home");
        let repo = tempfile::tempdir().expect("repo");
        fs::write(repo.path().join(".git"), "gitdir: /tmp/fake\n").unwrap();

        let global_dir = codex_home.path().join("steering");
        let project_dir = repo.path().join(".codex/steering");
        fs::create_dir_all(&global_dir).unwrap();
        fs::create_dir_all(&project_dir).unwrap();

        fs::write(global_dir.join("00.md"), "rule: global").unwrap();
        fs::write(project_dir.join("00.md"), "rule: project").unwrap();

        let cfg = make_config(&codex_home, repo.path().to_path_buf());
        let loaded = load_steering_docs(&cfg).await.expect("load");
        let combined = loaded.combined.expect("combined");
        let global_idx = combined.find("rule: global").expect("global present");
        let project_idx = combined.find("rule: project").expect("project present");
        assert!(
            global_idx < project_idx,
            "project steering should appear after global steering"
        );
    }

    #[tokio::test]
    async fn enforces_max_bytes_and_reports_omissions() {
        let codex_home = tempfile::tempdir().expect("codex home");
        let repo = tempfile::tempdir().expect("repo");
        fs::write(repo.path().join(".git"), "gitdir: /tmp/fake\n").unwrap();

        let project_dir = repo.path().join(".codex/steering");
        fs::create_dir_all(&project_dir).unwrap();

        fs::write(project_dir.join("01.md"), "A".repeat(10)).unwrap();
        fs::write(project_dir.join("02.md"), "B".repeat(10)).unwrap();
        fs::write(project_dir.join("03.md"), "C".repeat(10)).unwrap();

        let mut cfg = make_config(&codex_home, repo.path().to_path_buf());
        cfg.steering_doc_max_bytes = 15;

        let loaded = load_steering_docs(&cfg).await.expect("load");
        let combined = loaded.combined.expect("combined");
        assert!(combined.contains("[Steering: note]"));
        assert!(combined.contains(".codex/steering/03.md"));
        assert!(combined.contains("file=.codex/steering/02.md truncated=true"));
        assert_eq!(loaded.files.len(), 3);
        assert!(matches!(
            loaded.files[0].status,
            SteeringFileStatus::Included { .. }
        ));
        assert!(matches!(
            loaded.files[1].status,
            SteeringFileStatus::Included {
                truncated: true,
                ..
            }
        ));
        assert!(matches!(
            loaded.files[2].status,
            SteeringFileStatus::Omitted {
                reason: OmissionReason::OverBudget
            }
        ));
    }

    #[tokio::test]
    async fn opt_out_disables_loading() {
        let codex_home = tempfile::tempdir().expect("codex home");
        let repo = tempfile::tempdir().expect("repo");
        fs::write(repo.path().join(".git"), "gitdir: /tmp/fake\n").unwrap();
        let project_dir = repo.path().join(".codex/steering");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(project_dir.join("01.md"), "hello").unwrap();

        let mut cfg = make_config(&codex_home, repo.path().to_path_buf());
        cfg.steering_enabled = false;

        let loaded = load_steering_docs(&cfg).await.expect("load");
        assert!(!loaded.enabled);
        assert!(loaded.combined.is_none());
        assert!(matches!(
            loaded.files[0].status,
            SteeringFileStatus::Omitted {
                reason: OmissionReason::Disabled
            }
        ));
    }

    #[tokio::test]
    async fn ignores_empty_files() {
        let codex_home = tempfile::tempdir().expect("codex home");
        let repo = tempfile::tempdir().expect("repo");
        fs::write(repo.path().join(".git"), "gitdir: /tmp/fake\n").unwrap();
        let project_dir = repo.path().join(".codex/steering");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(project_dir.join("01.md"), "").unwrap();

        let cfg = make_config(&codex_home, repo.path().to_path_buf());
        let loaded = load_steering_docs(&cfg).await.expect("load");
        assert!(matches!(
            loaded.files[0].status,
            SteeringFileStatus::Omitted {
                reason: OmissionReason::Empty
            }
        ));
    }

    #[tokio::test]
    async fn ignores_non_utf8_files() {
        let codex_home = tempfile::tempdir().expect("codex home");
        let repo = tempfile::tempdir().expect("repo");
        fs::write(repo.path().join(".git"), "gitdir: /tmp/fake\n").unwrap();
        let project_dir = repo.path().join(".codex/steering");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(project_dir.join("01.md"), [0xff, 0xfe, 0xfd]).unwrap();

        let cfg = make_config(&codex_home, repo.path().to_path_buf());
        let loaded = load_steering_docs(&cfg).await.expect("load");
        assert!(matches!(
            loaded.files[0].status,
            SteeringFileStatus::Omitted {
                reason: OmissionReason::NonUtf8
            }
        ));
    }
}
