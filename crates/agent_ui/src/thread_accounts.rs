//! Per-thread account and hand-off records.
//!
//! A thread run with a non-default account must be reopened with the same
//! account, because its session lives in that account's store. A thread
//! created by "Continue with…" remembers where it came from. Both are kept
//! here, keyed by [`ThreadId`], instead of in the thread metadata table.

use agent_accounts::AccountId;
use anyhow::Context as _;
use collections::HashMap;
use db::kvp::KeyValueStore;
use gpui::{App, AppContext as _, Global, SharedString, Task, TaskExt as _};
use project::AgentId;
use serde::{Deserialize, Serialize};
use util::ResultExt as _;

use crate::Agent;
use crate::thread_metadata_store::ThreadId;

const NAMESPACE: &str = "agent_thread_accounts";

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadAccountInfo {
    /// The account the thread's agent runs with; `None` is the default one.
    #[serde(default)]
    pub account: Option<AccountId>,
    /// Set on threads created by continuing another thread.
    #[serde(default)]
    pub handoff_from: Option<HandoffSource>,
    /// The thread this one was last continued in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continued_in: Option<String>,
}

/// Hand-off chains are short; the bound only guards against a cycle in
/// corrupted records.
const MAX_CHAIN_LENGTH: usize = 32;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandoffSource {
    pub thread_id: String,
    pub agent_id: AgentId,
    #[serde(default)]
    pub account: Option<AccountId>,
}

impl HandoffSource {
    pub fn source_thread_id(&self) -> Option<ThreadId> {
        ThreadId::from_key_string(&self.thread_id)
    }
}

/// What was last written in this session, so that reads right after a write
/// don't race the background persistence.
#[derive(Default)]
struct RecentWrites(HashMap<ThreadId, ThreadAccountInfo>);

impl Global for RecentWrites {}

/// How a thread relates to the others of its conversation: "continued in
/// Grok" when a newer thread carries it on, otherwise "from Claude Code →
/// Codex", the threads it was continued from.
pub fn handoff_label(thread_id: ThreadId, cx: &App) -> Option<SharedString> {
    let info = read(thread_id, cx)?;
    if let Some(latest) = latest_continuation(thread_id, &info, cx) {
        return Some(format!("continued in {}", agent_label(latest, cx)).into());
    }
    let source = info.handoff_from?;
    let mut labels: Vec<String> = handoff_ancestors(thread_id, cx)
        .into_iter()
        .map(|ancestor| agent_label(ancestor, cx))
        .collect();
    if labels.is_empty() {
        // The source thread is gone; its record still names the agent.
        let agent = Agent::with_account(source.agent_id, source.account);
        labels.push(crate::agent_panel::thread_handoff::target_label(&agent, cx));
    }
    labels.reverse();
    labels.dedup();
    Some(format!("from {}", labels.join(" → ")).into())
}

fn agent_label(thread_id: ThreadId, cx: &App) -> String {
    live_thread_agent(thread_id, cx)
        .map(|agent| crate::agent_panel::thread_handoff::target_label(&agent, cx))
        .unwrap_or_default()
}

/// The agent of a thread that is still listed (not archived or deleted).
fn live_thread_agent(thread_id: ThreadId, cx: &App) -> Option<Agent> {
    let metadata = crate::thread_metadata_store::ThreadMetadataStore::try_global(cx)?
        .read(cx)
        .entry(thread_id)?;
    (!metadata.archived)
        .then(|| agent_for_thread(Agent::from(metadata.agent_id.clone()), thread_id, cx))
}

/// The newest listed thread that carries this one on, if any.
fn latest_continuation(
    thread_id: ThreadId,
    info: &ThreadAccountInfo,
    cx: &App,
) -> Option<ThreadId> {
    let mut latest = None;
    let mut next = info.continued_in.clone();
    for _ in 0..MAX_CHAIN_LENGTH {
        let Some(candidate) = next.as_deref().and_then(ThreadId::from_key_string) else {
            break;
        };
        if candidate == thread_id || live_thread_agent(candidate, cx).is_none() {
            break;
        }
        latest = Some(candidate);
        next = read(candidate, cx).and_then(|info| info.continued_in);
    }
    latest
}

/// The listed threads this one was continued from, nearest first.
fn handoff_ancestors(thread_id: ThreadId, cx: &App) -> Vec<ThreadId> {
    let mut ancestors = Vec::new();
    let mut visited = vec![thread_id];
    let mut current = thread_id;
    for _ in 0..MAX_CHAIN_LENGTH {
        let Some(parent) = read(current, cx)
            .and_then(|info| info.handoff_from)
            .and_then(|source| source.source_thread_id())
        else {
            break;
        };
        if visited.contains(&parent) {
            break;
        }
        visited.push(parent);
        if live_thread_agent(parent, cx).is_some() {
            ancestors.push(parent);
        }
        current = parent;
    }
    ancestors
}

/// Threads of the conversation `source` belongs to that already run with
/// `target`: continuing with `target` again makes them stale copies.
pub(crate) fn threads_superseded_by(source: ThreadId, target: &Agent, cx: &App) -> Vec<ThreadId> {
    handoff_ancestors(source, cx)
        .into_iter()
        .filter(|ancestor| live_thread_agent(*ancestor, cx).as_ref() == Some(target))
        .collect()
}

/// Records that `source` was continued in `continuation`.
pub(crate) fn record_continuation(source: ThreadId, continuation: ThreadId, cx: &mut App) {
    let info = ThreadAccountInfo {
        continued_in: Some(continuation.to_key_string()),
        ..read(source, cx).unwrap_or_default()
    };
    write(source, &info, cx).detach_and_log_err(cx);
}

pub fn read(thread_id: ThreadId, cx: &App) -> Option<ThreadAccountInfo> {
    if let Some(info) = cx
        .try_global::<RecentWrites>()
        .and_then(|recent| recent.0.get(&thread_id))
    {
        return Some(info.clone());
    }
    let raw = KeyValueStore::global(cx)
        .scoped(NAMESPACE)
        .read(&thread_id.to_key_string())
        .log_err()
        .flatten()?;
    serde_json::from_str(&raw).log_err()
}

pub fn write(
    thread_id: ThreadId,
    info: &ThreadAccountInfo,
    cx: &mut App,
) -> Task<anyhow::Result<()>> {
    cx.default_global::<RecentWrites>()
        .0
        .insert(thread_id, info.clone());
    let kvp = KeyValueStore::global(cx);
    let key = thread_id.to_key_string();
    let payload = match serde_json::to_string(info).context("serializing thread account") {
        Ok(payload) => payload,
        Err(err) => return Task::ready(Err(err)),
    };
    cx.background_spawn(async move { kvp.scoped(NAMESPACE).write(key, payload).await })
}

/// Records the account a thread runs with, keeping any hand-off record.
pub fn record_account(thread_id: ThreadId, account: Option<AccountId>, cx: &mut App) {
    let existing = read(thread_id, cx);
    if existing.as_ref().map(|info| &info.account) == Some(&account)
        || (existing.is_none() && account.is_none())
    {
        return;
    }
    let info = ThreadAccountInfo {
        account,
        ..existing.unwrap_or_default()
    };
    write(thread_id, &info, cx).detach_and_log_err(cx);
}

/// The agent to reopen a persisted thread with: the stored account wins over
/// an agent that does not name one.
pub fn agent_for_thread(agent: Agent, thread_id: ThreadId, cx: &App) -> Agent {
    match agent {
        Agent::Custom { id } => {
            let account = read(thread_id, cx).and_then(|info| info.account);
            Agent::with_account(id, account)
        }
        agent => agent,
    }
}

/// The agent a thread being opened runs with: a persisted thread reopens with
/// the account whose store holds its session.
pub(crate) fn agent_for_resume(agent: Agent, thread_id: Option<ThreadId>, cx: &App) -> Agent {
    match thread_id {
        Some(thread_id) => agent_for_thread(agent, thread_id, cx),
        None => agent,
    }
}

impl Agent {
    /// An external agent run with the given account; `None` (or the CLI's
    /// own home picked explicitly) is its default account.
    pub fn with_account(id: AgentId, account: Option<AccountId>) -> Self {
        match (Self::from(id), account) {
            (Self::Custom { id }, Some(account)) if !account.is_system() => {
                Self::CustomAccount { id, account }
            }
            (agent, _) => agent,
        }
    }

    /// The non-default account the agent runs with.
    pub fn account(&self) -> Option<&AccountId> {
        match self {
            Self::CustomAccount { account, .. } => Some(account),
            _ => None,
        }
    }
}

/// Deletes a thread for good: its metadata, its archived worktrees and,
/// when the agent supports it, the session in the store of the account the
/// thread ran with.
pub fn delete_thread_permanently(
    thread_id: ThreadId,
    session_id: Option<agent_client_protocol::schema::v1::SessionId>,
    agent_id: AgentId,
    connection_store: &gpui::Entity<crate::agent_connection_store::AgentConnectionStore>,
    cx: &mut App,
) {
    let agent = agent_for_thread(Agent::from(agent_id), thread_id, cx);
    crate::thread_metadata_store::ThreadMetadataStore::global(cx)
        .update(cx, |store, cx| store.delete(thread_id, cx));
    let fs = <dyn fs::Fs>::global(cx);
    let connection = connection_store.update(cx, |store, cx| {
        store
            .request_connection(
                agent.clone(),
                agent.server(fs, agent::ThreadStore::global(cx)),
                cx,
            )
            .read(cx)
            .wait_for_connection()
    });
    cx.spawn(async move |cx| {
        crate::thread_worktree_archive::cleanup_thread_archived_worktrees(thread_id, cx).await;
        let state = connection.await?;
        let deletion = cx.update(|cx| match &session_id {
            Some(session_id) => match state
                .connection
                .session_list(cx)
                .filter(|list| list.supports_delete())
            {
                Some(list) => list.delete_session(session_id, cx),
                None => Task::ready(Ok(())),
            },
            None => Task::ready(Ok(())),
        });
        deletion.await
    })
    .detach_and_log_err(cx);
}

const TERMINAL_NAMESPACE: &str = "agent_terminal_accounts";

/// The account a terminal thread was opened with: its shell gets the
/// account's home variable, so the agent CLIs started in it use that account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminalAccount {
    pub agent_id: AgentId,
    pub account: AccountId,
}

impl TerminalAccount {
    pub fn env(&self) -> HashMap<String, String> {
        agent_accounts::account_env(self.agent_id.as_ref(), Some(&self.account))
            .into_iter()
            .collect()
    }
}

#[derive(Default)]
struct RecentTerminalWrites(HashMap<String, TerminalAccount>);

impl Global for RecentTerminalWrites {}

pub fn terminal_account(terminal_key: &str, cx: &App) -> Option<TerminalAccount> {
    if let Some(account) = cx
        .try_global::<RecentTerminalWrites>()
        .and_then(|recent| recent.0.get(terminal_key))
    {
        return Some(account.clone());
    }
    let raw = KeyValueStore::global(cx)
        .scoped(TERMINAL_NAMESPACE)
        .read(terminal_key)
        .log_err()
        .flatten()?;
    serde_json::from_str(&raw).log_err()
}

pub fn write_terminal_account(terminal_key: String, account: TerminalAccount, cx: &mut App) {
    let payload = match serde_json::to_string(&account).context("serializing terminal account") {
        Ok(payload) => payload,
        Err(err) => {
            log::error!("{err:#}");
            return;
        }
    };
    cx.default_global::<RecentTerminalWrites>()
        .0
        .insert(terminal_key.clone(), account);
    let kvp = KeyValueStore::global(cx);
    cx.background_spawn(async move {
        kvp.scoped(TERMINAL_NAMESPACE)
            .write(terminal_key, payload)
            .await
    })
    .detach_and_log_err(cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    fn init(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
    }

    /// Drops the in-memory copies so reads must come from the database.
    fn forget_recent_writes(cx: &mut TestAppContext) {
        cx.run_until_parked();
        cx.update(|cx| {
            if cx.has_global::<RecentWrites>() {
                cx.remove_global::<RecentWrites>();
            }
            if cx.has_global::<RecentTerminalWrites>() {
                cx.remove_global::<RecentTerminalWrites>();
            }
        });
    }

    #[gpui::test]
    async fn persisted_thread_reopens_with_its_account(cx: &mut TestAppContext) {
        init(cx);
        let thread_id = ThreadId::new();
        let account = AccountId::from("/Users/me/.claude-work");
        cx.update(|cx| record_account(thread_id, Some(account.clone()), cx));
        forget_recent_writes(cx);

        cx.update(|cx| {
            assert_eq!(
                read(thread_id, cx).and_then(|info| info.account),
                Some(account.clone())
            );
            assert_eq!(
                agent_for_thread(Agent::from(AgentId::new("claude-acp")), thread_id, cx),
                Agent::CustomAccount {
                    id: AgentId::new("claude-acp"),
                    account: account.clone(),
                }
            );
            // An explicitly chosen account is kept.
            let other = AccountId::from("/Users/me/.claude-home");
            assert_eq!(
                agent_for_thread(
                    Agent::with_account(AgentId::new("claude-acp"), Some(other.clone())),
                    thread_id,
                    cx
                ),
                Agent::with_account(AgentId::new("claude-acp"), Some(other)),
            );
            // Threads without a record keep the default account.
            assert_eq!(
                agent_for_thread(Agent::from(AgentId::new("codex-acp")), ThreadId::new(), cx),
                Agent::from(AgentId::new("codex-acp")),
            );
        });
    }

    #[gpui::test]
    async fn continuing_back_to_an_agent_replaces_its_old_thread(cx: &mut TestAppContext) {
        use crate::thread_metadata_store::{ThreadMetadata, ThreadMetadataStore};

        init(cx);
        cx.update(|cx| {
            settings::init(cx);
            ThreadMetadataStore::init_global(cx);
        });
        // Let the store finish loading before threads are added to it.
        cx.run_until_parked();
        let claude = Agent::from(AgentId::new("claude-acp"));
        let codex = Agent::from(AgentId::new("codex-acp"));
        let save = |agent: &Agent, cx: &mut TestAppContext| {
            let thread_id = ThreadId::new();
            let now = chrono::Utc::now();
            let metadata = ThreadMetadata {
                thread_id,
                session_id: None,
                agent_id: agent.id(),
                title: None,
                title_override: None,
                updated_at: now,
                created_at: Some(now),
                interacted_at: Some(now),
                worktree_paths: Default::default(),
                remote_connection: None,
                archived: false,
            };
            cx.update(|cx| {
                ThreadMetadataStore::global(cx).update(cx, |store, cx| store.save(metadata, cx))
            });
            thread_id
        };
        let continue_with =
            |source: ThreadId, target: ThreadId, agent: &Agent, cx: &mut TestAppContext| {
                cx.update(|cx| {
                    write(
                        target,
                        &ThreadAccountInfo {
                            account: None,
                            handoff_from: Some(HandoffSource {
                                thread_id: source.to_key_string(),
                                agent_id: agent.id(),
                                account: None,
                            }),
                            continued_in: None,
                        },
                        cx,
                    )
                    .detach();
                    record_continuation(source, target, cx);
                });
            };

        let first_claude = save(&claude, cx);
        let first_codex = save(&codex, cx);
        continue_with(first_claude, first_codex, &claude, cx);
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(
                handoff_label(first_claude, cx).as_deref(),
                Some("continued in Codex")
            );
            assert_eq!(
                handoff_label(first_codex, cx).as_deref(),
                Some("from Claude Code")
            );
            assert!(threads_superseded_by(first_codex, &codex, cx).is_empty());
            assert_eq!(
                threads_superseded_by(first_codex, &claude, cx),
                vec![first_claude]
            );
        });

        let second_claude = save(&claude, cx);
        continue_with(first_codex, second_claude, &codex, cx);
        cx.update(|cx| {
            ThreadMetadataStore::global(cx)
                .update(cx, |store, cx| store.archive(first_claude, None, cx));
        });
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(
                handoff_label(first_codex, cx).as_deref(),
                Some("continued in Claude Code")
            );
            assert_eq!(
                handoff_label(second_claude, cx).as_deref(),
                Some("from Codex")
            );
        });
    }

    #[gpui::test]
    async fn recording_the_account_keeps_the_handoff_source(cx: &mut TestAppContext) {
        init(cx);
        let thread_id = ThreadId::new();
        let source = HandoffSource {
            thread_id: ThreadId::new().to_key_string(),
            agent_id: AgentId::new("claude-acp"),
            account: None,
        };
        let account = AccountId::from("/Users/me/.codex-2");
        cx.update(|cx| {
            write(
                thread_id,
                &ThreadAccountInfo {
                    account: Some(account.clone()),
                    handoff_from: Some(source.clone()),
                    continued_in: None,
                },
                cx,
            )
            .detach();
            // Opening the thread records the same account again: a no-op.
            record_account(thread_id, Some(account.clone()), cx);
        });
        forget_recent_writes(cx);

        cx.update(|cx| {
            assert_eq!(
                read(thread_id, cx),
                Some(ThreadAccountInfo {
                    account: Some(account),
                    handoff_from: Some(source),
                    continued_in: None,
                })
            );
        });
    }

    #[gpui::test]
    async fn terminal_account_sets_the_home_variable(cx: &mut TestAppContext) {
        init(cx);
        cx.update(|cx| {
            write_terminal_account(
                "terminal-1".into(),
                TerminalAccount {
                    agent_id: AgentId::new("codex-acp"),
                    account: AccountId::from("/Users/me/.codex-2"),
                },
                cx,
            )
        });
        forget_recent_writes(cx);

        cx.update(|cx| {
            let account = terminal_account("terminal-1", cx).expect("persisted");
            assert_eq!(
                account.env(),
                HashMap::from_iter([("CODEX_HOME".to_string(), "/Users/me/.codex-2".to_string())])
            );
            assert_eq!(terminal_account("terminal-2", cx), None);
        });
    }

    #[test]
    fn agent_account_serialization() {
        let agent = Agent::with_account(
            AgentId::from("claude-acp"),
            Some(AccountId::from("/Users/me/.claude-work")),
        );
        let json = serde_json::to_string(&agent).unwrap();
        assert_eq!(
            json,
            r#"{"custom_account":{"name":"claude-acp","account":"/Users/me/.claude-work"}}"#
        );
        assert_eq!(serde_json::from_str::<Agent>(&json).unwrap(), agent);
        // The default account keeps Zed's own format.
        assert_eq!(
            serde_json::to_string(&Agent::from(AgentId::from("claude-acp"))).unwrap(),
            r#"{"custom":{"name":"claude-acp"}}"#
        );
        assert_eq!(
            Agent::with_account(AgentId::from("claude-acp"), Some(AccountId::system())),
            Agent::from(AgentId::from("claude-acp"))
        );
        assert_eq!(
            Agent::with_account(agent::ZED_AGENT_ID.clone(), Some(AccountId::from("/x"))),
            Agent::NativeAgent
        );
    }
}
