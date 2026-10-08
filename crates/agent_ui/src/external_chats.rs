//! "Find Chat…": searches the chats of this project that were started outside
//! Zed (in Superset, a terminal, another editor), with any agent and account,
//! and opens the chosen one as a thread, without importing the others.

use std::path::PathBuf;
use std::sync::Arc;

use agent_accounts::{
    AccountProvider,
    external_sessions::{
        ExternalSession, folder_for, list_external_sessions, prepare_cursor_chat, project_folders,
    },
};
use agent_client_protocol::schema::v1 as acp;
use chrono::Utc;
use collections::{HashMap, HashSet};
use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    Render, SharedString, Task, TaskExt as _, WeakEntity, Window, actions,
};
use picker::{Picker, PickerDelegate};
use project::AgentId;
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, PathList, Workspace};

use crate::account_registry::AccountRegistry;
use crate::thread_metadata_store::{ThreadId, ThreadMetadata, ThreadMetadataStore, WorktreePaths};
use crate::{Agent, AgentPanel, AgentThreadSource, thread_accounts};

actions!(
    agent,
    [
        /// Finds a chat of this project started outside Zed (in Superset, a
        /// terminal or another editor) and continues it here.
        FindChat
    ]
);

pub(crate) fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &FindChat, window, cx| {
        if !workspace.project().read(cx).is_local() {
            return;
        }
        let weak_workspace = cx.entity().downgrade();
        workspace.toggle_modal(window, cx, |window, cx| {
            FindChatModal::new(weak_workspace, window, cx)
        });
    });
}

pub struct FindChatModal {
    picker: Entity<Picker<FindChatDelegate>>,
    _load: Task<()>,
}

impl EventEmitter<DismissEvent> for FindChatModal {}
impl ModalView for FindChatModal {}

impl Focusable for FindChatModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for FindChatModal {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("FindChatModal")
            .w(rems(40.))
            .child(self.picker.clone())
    }
}

/// A chat the picker offers, with the project folder it belongs to.
#[derive(Clone)]
struct Candidate {
    session: ExternalSession,
    /// The project root the folder belongs to.
    main_folder: PathBuf,
    /// The project root or the repository worktree the chat ran in.
    folder: PathBuf,
    title: SharedString,
    detail: SharedString,
}

enum LoadState {
    Loading,
    Failed(SharedString),
    Ready,
}

struct FindChatDelegate {
    modal: WeakEntity<FindChatModal>,
    workspace: WeakEntity<Workspace>,
    agents: HashMap<AccountProvider, AgentId>,
    candidates: Vec<Candidate>,
    matches: Vec<StringMatch>,
    selected_index: usize,
    state: LoadState,
}

impl FindChatModal {
    fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let Some(workspace_entity) = workspace.upgrade() else {
            return Self::empty(workspace, window, cx);
        };
        let project = workspace_entity.read(cx).project().clone();
        let roots: Vec<PathBuf> = project
            .read(cx)
            .visible_worktrees(cx)
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            .collect();
        let agents: HashMap<AccountProvider, AgentId> = project
            .read(cx)
            .agent_server_store()
            .read(cx)
            .external_agents()
            .filter_map(|agent_id| {
                Some((
                    AccountProvider::for_agent(agent_id.as_ref())?,
                    agent_id.clone(),
                ))
            })
            .collect();
        let accounts: Vec<_> = AccountProvider::ALL
            .iter()
            .flat_map(|provider| AccountRegistry::accounts_for_agent(provider.agent_id(), cx))
            .collect();
        let store = ThreadMetadataStore::global(cx);
        let known_sessions: HashSet<String> = store
            .read(cx)
            .entries()
            .chain(store.read(cx).archived_entries())
            .filter_map(|thread| Some(thread.session_id.as_ref()?.0.to_string()))
            .collect();
        let labels: HashMap<(AccountProvider, Option<agent_accounts::AccountId>), String> =
            accounts
                .iter()
                .map(|account| {
                    let agent = Agent::with_account(
                        AgentId::new(account.provider.agent_id()),
                        account.id(),
                    );
                    let label = crate::agent_panel::thread_handoff::target_label(&agent, cx);
                    ((account.provider, account.id()), label)
                })
                .collect();

        let delegate = FindChatDelegate {
            modal: cx.entity().downgrade(),
            workspace,
            agents: agents.clone(),
            candidates: Vec::new(),
            matches: Vec::new(),
            selected_index: 0,
            state: LoadState::Loading,
        };
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));

        let shell_env: collections::HashMap<String, String> = std::env::vars().collect();
        let found = cx.background_spawn(async move {
            let mut folders = Vec::new();
            for root in roots {
                for folder in project_folders(&root).await {
                    folders.push((root.clone(), folder));
                }
            }
            let paths: Vec<PathBuf> = folders.iter().map(|(_, folder)| folder.clone()).collect();
            let sessions = list_external_sessions(&accounts, &paths, &shell_env).await?;
            anyhow::Ok((sessions, folders))
        });
        let load = cx.spawn_in(window, {
            let picker = picker.downgrade();
            async move |_, cx| {
                let result = found.await;
                picker
                    .update_in(cx, |picker, window, cx| {
                        match result {
                            Ok((sessions, folders)) => {
                                picker.delegate.candidates = candidates(
                                    sessions,
                                    &folders,
                                    &known_sessions,
                                    &agents,
                                    &labels,
                                );
                                picker.delegate.state = LoadState::Ready;
                            }
                            Err(error) => {
                                log::warn!("finding chats failed: {error:#}");
                                picker.delegate.state =
                                    LoadState::Failed(format!("{error:#}").into());
                            }
                        }
                        picker.refresh(window, cx);
                    })
                    .log_err();
            }
        });
        Self {
            picker,
            _load: load,
        }
    }

    fn empty(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = FindChatDelegate {
            modal: cx.entity().downgrade(),
            workspace,
            agents: HashMap::default(),
            candidates: Vec::new(),
            matches: Vec::new(),
            selected_index: 0,
            state: LoadState::Ready,
        };
        Self {
            picker: cx.new(|cx| Picker::uniform_list(delegate, window, cx)),
            _load: Task::ready(()),
        }
    }
}

/// The sessions that belong to the project, ran with an agent this project
/// has, and are not already threads in Zed.
fn candidates(
    sessions: Vec<ExternalSession>,
    folders: &[(PathBuf, PathBuf)],
    known_sessions: &HashSet<String>,
    agents: &HashMap<AccountProvider, AgentId>,
    labels: &HashMap<(AccountProvider, Option<agent_accounts::AccountId>), String>,
) -> Vec<Candidate> {
    let folder_paths: Vec<PathBuf> = folders.iter().map(|(_, folder)| folder.clone()).collect();
    let now = Utc::now();
    sessions
        .into_iter()
        .filter(|session| {
            agents.contains_key(&session.provider) && !known_sessions.contains(&session.id)
        })
        .filter_map(|session| {
            let folder = folder_for(&session.cwd, &folder_paths)?.clone();
            let main_folder = folders
                .iter()
                .find(|(_, candidate)| *candidate == folder)
                .map(|(root, _)| root.clone())?;
            let agent = labels
                .get(&(session.provider, session.account.clone()))
                .cloned()
                .unwrap_or_else(|| session.provider.display_name().to_string());
            let mut detail = format!("{agent} · {}", relative_time(session.updated_at, now));
            if folder != main_folder
                && let Some(name) = folder.file_name()
            {
                detail.push_str(&format!(" · {}", name.to_string_lossy()));
            }
            Some(Candidate {
                title: session
                    .title
                    .clone()
                    .or_else(|| session.preview.clone())
                    .unwrap_or_else(|| "Untitled chat".to_string())
                    .into(),
                detail: detail.into(),
                session,
                main_folder,
                folder,
            })
        })
        .collect()
}

fn relative_time(timestamp: chrono::DateTime<Utc>, now: chrono::DateTime<Utc>) -> String {
    let minutes = (now - timestamp).num_minutes().max(0);
    match minutes {
        0 => "just now".to_string(),
        1..60 => format!("{minutes} min ago"),
        60..1440 => format!("{} h ago", minutes / 60),
        _ => format!("{} d ago", minutes / 1440),
    }
}

impl PickerDelegate for FindChatDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "find chat"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Find a chat of this project started outside Zed…".into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some(match &self.state {
            LoadState::Loading => "Looking for chats…".into(),
            LoadState::Failed(error) => error.clone(),
            LoadState::Ready => "No other chats for this project.".into(),
        })
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let candidates: Vec<StringMatchCandidate> = self
            .candidates
            .iter()
            .enumerate()
            .map(|(ix, candidate)| StringMatchCandidate::new(ix, &candidate.title))
            .collect();
        let background = cx.background_executor().clone();
        cx.spawn_in(window, async move |picker, cx| {
            let matches = if query.is_empty() {
                candidates
                    .into_iter()
                    .map(|candidate| StringMatch {
                        candidate_id: candidate.id,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.,
                    })
                    .collect()
            } else {
                match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    200,
                    &Default::default(),
                    background,
                )
                .await
            };
            picker
                .update(cx, |picker, cx| {
                    picker.delegate.matches = matches;
                    picker.delegate.selected_index = 0;
                    cx.notify();
                })
                .log_err();
        })
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(candidate) = self
            .matches
            .get(self.selected_index)
            .and_then(|found| self.candidates.get(found.candidate_id))
            .cloned()
        else {
            return;
        };
        let Some(agent_id) = self.agents.get(&candidate.session.provider).cloned() else {
            return;
        };
        let workspace = self.workspace.clone();
        self.dismissed(window, cx);
        cx.spawn_in(window, async move |_, cx| {
            if candidate.session.provider == AccountProvider::Cursor {
                let session = candidate.session.clone();
                cx.background_spawn(async move { prepare_cursor_chat(&session) })
                    .await?;
            }
            workspace.update_in(cx, |workspace, window, cx| {
                open_candidate(workspace, candidate, agent_id, window, cx)
            })
        })
        .detach_and_log_err(cx);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.modal.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let found = self.matches.get(ix)?;
        let candidate = self.candidates.get(found.candidate_id)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(
                    Icon::new(crate::usage_view::provider_icon(candidate.session.provider))
                        .color(Color::Muted),
                )
                .child(
                    v_flex()
                        .child(HighlightedLabel::new(
                            candidate.title.clone(),
                            found.positions.clone(),
                        ))
                        .child(
                            Label::new(candidate.detail.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                ),
        )
    }
}

/// Records the session as a thread of the project and opens it, so the agent
/// loads it with its history.
fn open_candidate(
    workspace: &mut Workspace,
    candidate: Candidate,
    agent_id: AgentId,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
        return;
    };
    let session = candidate.session;
    let thread_id = ThreadId::new();
    let folder = PathList::new(&[candidate.folder.as_path()]);
    let worktree_paths = WorktreePaths::from_path_lists(
        PathList::new(&[candidate.main_folder.as_path()]),
        folder.clone(),
    )
    .unwrap_or_else(|_| WorktreePaths::from_folder_paths(&folder));
    let title: SharedString = candidate.title;
    ThreadMetadataStore::global(cx).update(cx, |store, cx| {
        store.save(
            ThreadMetadata {
                thread_id,
                session_id: Some(acp::SessionId::new(session.id.clone())),
                agent_id: agent_id.clone(),
                title: Some(title.clone()),
                title_override: None,
                updated_at: session.updated_at,
                created_at: Some(session.started_at),
                interacted_at: Some(Utc::now()),
                worktree_paths,
                remote_connection: None,
                archived: false,
            },
            cx,
        )
    });
    thread_accounts::record_account(thread_id, session.account.clone(), cx);
    let agent = Agent::with_account(agent_id, session.account);
    panel.update(cx, |panel, cx| {
        panel.load_agent_thread(
            agent,
            thread_id,
            Some(PathList::new(&[session.cwd.as_path()])),
            Some(title),
            true,
            AgentThreadSource::AgentPanel,
            window,
            cx,
        )
    });
    workspace.focus_panel::<AgentPanel>(window, cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_project_chats_that_are_not_threads_yet() {
        let now = Utc::now();
        let session = |id: &str, provider, cwd: &str| ExternalSession {
            provider,
            account: None,
            id: id.into(),
            title: None,
            preview: Some(format!("chat {id}")),
            cwd: PathBuf::from(cwd),
            started_at: now,
            updated_at: now,
        };
        let folders = vec![
            (PathBuf::from("/repo"), PathBuf::from("/repo")),
            (PathBuf::from("/repo"), PathBuf::from("/wt/feature")),
        ];
        let agents: HashMap<_, _> = [
            (AccountProvider::Claude, AgentId::new("claude-acp")),
            (AccountProvider::Codex, AgentId::new("codex-acp")),
        ]
        .into_iter()
        .collect();
        let known: HashSet<String> = ["known".to_string()].into_iter().collect();
        let found = candidates(
            vec![
                session("in-root", AccountProvider::Claude, "/repo/sub"),
                session("in-worktree", AccountProvider::Codex, "/wt/feature"),
                session("known", AccountProvider::Claude, "/repo"),
                session("elsewhere", AccountProvider::Claude, "/other"),
                session("no-agent", AccountProvider::Grok, "/repo"),
            ],
            &folders,
            &known,
            &agents,
            &HashMap::default(),
        );
        let found: Vec<_> = found
            .iter()
            .map(|candidate| {
                (
                    candidate.session.id.as_str(),
                    candidate.folder.clone(),
                    candidate.detail.to_string(),
                )
            })
            .collect();
        assert_eq!(
            found,
            vec![
                (
                    "in-root",
                    PathBuf::from("/repo"),
                    "Claude Code · just now".to_string()
                ),
                (
                    "in-worktree",
                    PathBuf::from("/wt/feature"),
                    "Codex · just now · feature".to_string()
                ),
            ]
        );
    }
}
