//! Agent sessions started outside Zed (in Superset, a terminal, another
//! editor), found with `txcript list --json` across every account, so one of
//! them can be opened and continued in Zed.

/// How many recent sessions per account are listed. Previews read whole
/// transcripts, and old ones can be large.
const SESSIONS_PER_ACCOUNT: usize = 60;

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow};
use chrono::{DateTime, Utc};
use collections::{HashMap, HashSet};
use serde::Deserialize;

use crate::handoff::{
    SessionEndpoint, cursor_root, cursor_store, find_txcript, run_step, step_env,
};
use crate::{AccountId, AccountProvider, AgentAccount};

#[derive(Debug, Clone, PartialEq)]
pub struct ExternalSession {
    pub provider: AccountProvider,
    /// `None` is the default account.
    pub account: Option<AccountId>,
    pub id: String,
    pub title: Option<String>,
    /// The first prompt, when the session has no title.
    pub preview: Option<String>,
    pub cwd: PathBuf,
    pub started_at: DateTime<Utc>,
    /// When the session last changed.
    pub updated_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct ListedSession {
    harness: String,
    id: String,
    timestamp: DateTime<Utc>,
    #[serde(default)]
    updated_at: Option<DateTime<Utc>>,
    title: Option<String>,
    #[serde(default)]
    preview: Option<String>,
    cwd: Option<String>,
}

/// Lists the recent sessions in `folders` of every account, most recently
/// active first. The default accounts are read in one call; each other
/// account with its home variable set.
pub async fn list_external_sessions(
    accounts: &[AgentAccount],
    folders: &[PathBuf],
    shell_env: &HashMap<String, String>,
) -> Result<Vec<ExternalSession>> {
    let txcript = find_txcript(shell_env).context("txcript was not found")?;
    let cwd = util::paths::home_dir();
    let mut sessions = Vec::new();
    let mut seen = HashSet::default();

    let limit = SESSIONS_PER_ACCOUNT.to_string();
    let mut common: Vec<&std::ffi::OsStr> = vec![
        "list".as_ref(),
        "--json".as_ref(),
        "--preview".as_ref(),
        "-n".as_ref(),
        limit.as_ref(),
    ];
    for folder in folders {
        common.push("--under".as_ref());
        common.push(folder.as_os_str());
    }
    let mut calls: Vec<(Option<&AgentAccount>, Vec<&std::ffi::OsStr>)> =
        vec![(None, common.clone())];
    for account in accounts.iter().filter(|account| !account.is_default) {
        let mut args = common.clone();
        args.push(std::ffi::OsStr::new("--from"));
        args.push(std::ffi::OsStr::new(account.provider.harness()));
        calls.push((Some(account), args));
    }
    for (account, args) in calls {
        let env = match account {
            Some(account) => step_env(
                shell_env,
                &SessionEndpoint {
                    provider: account.provider,
                    account: account.id(),
                },
            ),
            None => shell_env.clone(),
        };
        let output = run_step(&txcript, &args, cwd, &env)
            .await
            .map_err(|error| {
                if error.to_string().contains("unexpected argument") {
                    anyhow!(
                        "this txcript can't list sessions for Zed; use the txcript bundled with \
                     this fork's builds ({error})"
                    )
                } else {
                    error
                }
            })?;
        let listed: Vec<ListedSession> =
            serde_json::from_str(output.trim()).context("reading txcript's session list")?;
        for session in listed {
            let Some(provider) = AccountProvider::for_harness(&session.harness) else {
                continue;
            };
            let Some(cwd) = session.cwd.filter(|cwd| !cwd.is_empty()) else {
                continue;
            };
            if !seen.insert((provider, session.id.clone())) {
                continue;
            }
            sessions.push(ExternalSession {
                provider,
                account: account.and_then(AgentAccount::id),
                id: session.id,
                title: session.title.filter(|title| !title.trim().is_empty()),
                preview: session.preview.filter(|preview| !preview.is_empty()),
                cwd: PathBuf::from(cwd),
                started_at: session.timestamp,
                updated_at: session.updated_at.unwrap_or(session.timestamp),
            });
        }
    }
    sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
    Ok(sessions)
}

/// The project's own folder and, when it is a git repository, every worktree
/// of that repository: Superset runs its agents in worktrees elsewhere.
pub async fn project_folders(root: &Path) -> Vec<PathBuf> {
    let mut folders = vec![root.to_path_buf()];
    let mut command = util::command::new_command("git");
    command
        .args(["worktree", "list", "--porcelain"])
        .current_dir(root);
    if let Ok(output) = command.output().await
        && output.status.success()
    {
        for path in String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
        {
            let path = PathBuf::from(path);
            if !folders.contains(&path) {
                folders.push(path);
            }
        }
    }
    folders
}

/// The folder of `folders` a session's cwd is in, if any.
pub fn folder_for<'a>(cwd: &Path, folders: &'a [PathBuf]) -> Option<&'a PathBuf> {
    folders
        .iter()
        .filter(|folder| cwd.starts_with(folder))
        .max_by_key(|folder| folder.components().count())
}

/// Cursor's ACP agent, which Zed's Cursor chats use, reads its own store; a
/// chat started with Cursor's CLI is copied there before it is opened.
pub fn prepare_cursor_chat(session: &ExternalSession) -> Result<()> {
    let root = cursor_root(&SessionEndpoint {
        provider: AccountProvider::Cursor,
        account: session.account.clone(),
    });
    if cursor_store::acp_dir(&root, &session.id)?
        .join("store.db")
        .is_file()
    {
        return Ok(());
    }
    cursor_store::copy_cli_session_to_acp(&root, &session.id, &session.cwd, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_belong_to_the_innermost_project_folder() {
        let folders = vec![
            PathBuf::from("/repo"),
            PathBuf::from("/home/.superset/worktrees/repo/feature"),
        ];
        assert_eq!(
            folder_for(Path::new("/repo/crates/app"), &folders),
            Some(&folders[0])
        );
        assert_eq!(
            folder_for(
                Path::new("/home/.superset/worktrees/repo/feature"),
                &folders
            ),
            Some(&folders[1])
        );
        assert_eq!(folder_for(Path::new("/other"), &folders), None);
        assert_eq!(folder_for(Path::new("/repository"), &folders), None);
    }

    #[test]
    fn reads_txcript_list_json() {
        let listed: Vec<ListedSession> = serde_json::from_str(
            r#"[{"harness":"codex","id":"01a1","timestamp":"2026-10-08T08:43:30+00:00","title":null,"cwd":"/repo","git_branch":null,"model":null}]"#,
        )
        .unwrap();
        assert_eq!(listed[0].harness, "codex");
        assert_eq!(
            AccountProvider::for_harness(&listed[0].harness),
            Some(AccountProvider::Codex)
        );
    }
}
