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
    /// The agent the conversation was last moved from.
    #[serde(default)]
    pub handoff_from: Option<HandoffSource>,
    /// Every agent the conversation was moved from, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub earlier_agents: Vec<HandoffSource>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandoffSource {
    /// The thread the conversation was in; the thread itself once a switch
    /// keeps the conversation in place.
    pub thread_id: String,
    pub agent_id: AgentId,
    #[serde(default)]
    pub account: Option<AccountId>,
}

impl HandoffSource {
    pub fn agent(&self) -> Agent {
        Agent::with_account(self.agent_id.clone(), self.account.clone())
    }
}

/// What was last written in this session, so that reads right after a write
/// don't race the background persistence.
#[derive(Default)]
struct RecentWrites(HashMap<ThreadId, ThreadAccountInfo>);

impl Global for RecentWrites {}

/// The agents a thread's conversation went through before its current one,
/// such as "from Claude Code → Codex".
pub fn handoff_label(thread_id: ThreadId, cx: &App) -> Option<SharedString> {
    let info = read(thread_id, cx)?;
    let mut sources = info.earlier_agents;
    if sources.is_empty() {
        sources.extend(info.handoff_from);
    }
    let mut labels: Vec<String> = sources
        .iter()
        .map(|source| crate::agent_panel::thread_handoff::target_label(&source.agent(), cx))
        .collect();
    labels.dedup();
    (!labels.is_empty()).then(|| format!("from {}", labels.join(" → ")).into())
}

/// Records that a thread's conversation moved from `source` to an agent run
/// with `account`, in the same thread.
pub(crate) fn record_switch(
    thread_id: ThreadId,
    source: HandoffSource,
    account: Option<AccountId>,
    cx: &mut App,
) -> Task<anyhow::Result<()>> {
    let mut info = read(thread_id, cx).unwrap_or_default();
    if info.earlier_agents.is_empty() {
        // Threads continued before switches stayed in place name only
        // their last source.
        info.earlier_agents.extend(info.handoff_from.take());
    }
    info.earlier_agents.push(source.clone());
    info.handoff_from = Some(source);
    info.account = account;
    write(thread_id, &info, cx)
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
    let deletion =
        session_id.map(|session_id| delete_agent_session(agent, session_id, connection_store, cx));
    cx.spawn(async move |cx| {
        crate::thread_worktree_archive::cleanup_thread_archived_worktrees(thread_id, cx).await;
        match deletion {
            Some(deletion) => deletion.await,
            None => Ok(()),
        }
    })
    .detach_and_log_err(cx);
}

/// Deletes a session from the store of the agent (and account) it belongs
/// to, when the agent supports deleting sessions.
pub(crate) fn delete_agent_session(
    agent: Agent,
    session_id: agent_client_protocol::schema::v1::SessionId,
    connection_store: &gpui::Entity<crate::agent_connection_store::AgentConnectionStore>,
    cx: &mut App,
) -> Task<anyhow::Result<()>> {
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
        let state = connection.await?;
        let deletion = cx.update(|cx| {
            match state
                .connection
                .session_list(cx)
                .filter(|list| list.supports_delete())
            {
                Some(list) => list.delete_session(&session_id, cx),
                None => Task::ready(Ok(())),
            }
        });
        deletion.await
    })
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
    async fn switches_keep_the_agents_a_thread_went_through(cx: &mut TestAppContext) {
        init(cx);
        let thread_id = ThreadId::new();
        let source = |agent_id: &str| HandoffSource {
            thread_id: thread_id.to_key_string(),
            agent_id: AgentId::new(agent_id),
            account: None,
        };
        cx.update(|cx| record_switch(thread_id, source("claude-acp"), None, cx).detach());
        cx.run_until_parked();
        cx.update(|cx| record_switch(thread_id, source("codex-acp"), None, cx).detach());
        forget_recent_writes(cx);
        cx.update(|cx| {
            assert_eq!(
                handoff_label(thread_id, cx).as_deref(),
                Some("from Claude Code → Codex")
            );
            let info = read(thread_id, cx).unwrap();
            assert_eq!(info.handoff_from, Some(source("codex-acp")));
            assert_eq!(info.earlier_agents.len(), 2);
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
                    earlier_agents: Vec::new(),
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
                    earlier_agents: Vec::new(),
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
