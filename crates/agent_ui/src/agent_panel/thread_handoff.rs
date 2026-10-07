//! "Continue with…": carries the active thread over to another agent or
//! another account of the same agent.
//!
//! The conversation is converted with txcript into a new native session in
//! the target agent's (and account's) store, then opened like any persisted
//! thread, so the agent loads it through ACP `session/load` and really has
//! the history. When that is not possible, the transcript is offered as the
//! first message of a fresh thread instead.

use std::path::{Path, PathBuf};

use agent_accounts::{
    AccountId, AccountProvider,
    handoff::{SessionEndpoint, TransferRequest, transfer},
};
use agent_client_protocol::schema::v1 as acp;
use chrono::Utc;
use futures::FutureExt as _;
use gpui::{Action, App, Context, Entity, SharedString, TaskExt as _, WeakEntity, Window};
use project::{AgentId, Project};
use schemars::JsonSchema;
use serde::Deserialize;
use ui::{Color, ContextMenu, ContextMenuEntry, FluentBuilder as _, IconName};
use workspace::{PathList, Toast, Workspace, notifications::NotificationId};

use super::AgentPanel;
use crate::account_registry::{
    AccountRegistry, AgentAccountsSettings, QuotaRegistry, account_label_with_quota,
};
use crate::{
    Agent, AgentInitialContent, AgentThreadSource,
    thread_accounts::{self, HandoffSource, ThreadAccountInfo},
    thread_metadata_store::{ThreadId, ThreadMetadata, ThreadMetadataStore, WorktreePaths},
};
use settings::Settings as _;

struct ThreadHandoffToast;

/// Covers both txcript steps; long conversations take a few seconds.
const HANDOFF_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(240);

/// What the new thread needs from the source thread.
struct HandoffSourceThread {
    thread_id: ThreadId,
    agent: Agent,
    session_id: acp::SessionId,
    title: Option<SharedString>,
    work_dirs: Option<PathList>,
    markdown: String,
}

impl AgentPanel {
    pub(crate) fn continue_thread_with(
        &mut self,
        target_agent_id: AgentId,
        target_account: Option<AccountId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target_account = AccountRegistry::resolve(target_agent_id.as_ref(), target_account, cx);
        if self.pending_handoff.is_some() {
            self.show_handoff_message("A conversation is already being moved.", cx);
            return;
        }
        if self.active_thread_is_generating(cx) {
            self.show_handoff_message(
                "The agent is still responding. Wait for it to finish, or stop it, then continue.",
                cx,
            );
            return;
        }
        let Some(source) = self.handoff_source_thread(cx) else {
            self.show_handoff_message(
                "Send a message first: there is no conversation to continue yet.",
                cx,
            );
            return;
        };
        let target = Agent::with_account(target_agent_id.clone(), target_account.clone());
        if target == source.agent {
            return;
        }

        let native = |provider: Option<AccountProvider>| {
            provider.filter(|provider| provider.supports_native_handoff())
        };
        let source_provider = native(AccountProvider::for_agent(source.agent.id().as_ref()));
        let target_provider = native(AccountProvider::for_agent(target_agent_id.as_ref()));
        let (Some(source_provider), Some(target_provider)) = (source_provider, target_provider)
        else {
            // No native conversion for this pair: hand the transcript over instead.
            self.continue_with_transcript(source, target, window, cx);
            return;
        };

        let Some(cwd) = handoff_cwd(&source, self, cx) else {
            self.continue_with_transcript(source, target, window, cx);
            return;
        };
        let shell_env = self.project.update(cx, |project, cx| {
            project.environment().update(cx, |environment, cx| {
                environment.local_directory_environment(
                    &task::Shell::System,
                    cwd.as_path().into(),
                    cx,
                )
            })
        });
        let source_endpoint = SessionEndpoint {
            provider: source_provider,
            account: source.agent.account().cloned(),
        };
        let target_endpoint = SessionEndpoint {
            provider: target_provider,
            account: target_account,
        };
        let source_session_id = source.session_id.0.to_string();

        self.show_handoff_progress(
            format!("Moving the conversation to {}…", target_label(&target, cx)),
            cx,
        );
        self.pending_handoff = Some(cx.spawn_in(window, async move |this, cx| {
            let shell_env = shell_env.await.unwrap_or_default();
            let transfer = transfer(TransferRequest {
                source: source_endpoint,
                source_session_id,
                target: target_endpoint,
                cwd,
                shell_env: shell_env.into_iter().collect(),
            });
            let timeout = cx.background_executor().timer(HANDOFF_TIMEOUT);
            let result = futures::select_biased! {
                result = transfer.fuse() => result,
                _ = timeout.fuse() => Err(anyhow::anyhow!("txcript timed out")),
            };
            this.update_in(cx, |this, window, cx| {
                this.pending_handoff = None;
                this.dismiss_handoff_message(cx);
                match result {
                    Ok(new_session_id) => {
                        this.open_handed_off_thread(source, target, new_session_id, window, cx)
                    }
                    Err(error) => {
                        log::error!("continuing the thread failed: {error:#}");
                        this.show_handoff_message(
                            format!(
                                "Couldn't move the conversation natively ({error}). \
                             Started a new thread with the transcript instead."
                            ),
                            cx,
                        );
                        this.continue_with_transcript(source, target, window, cx);
                    }
                }
            })
            .ok();
        }));
    }

    /// The agents and accounts the active thread can be continued with,
    /// with their menu labels.
    pub(crate) fn handoff_targets(&self, cx: &App) -> Vec<(Agent, SharedString)> {
        let Some(current) = self
            .active_conversation_view()
            .map(|view| view.read(cx).connection_key().clone())
        else {
            return Vec::new();
        };
        handoff_targets(&current, &self.project, cx)
    }

    /// Opens a terminal thread whose shell runs agent CLIs with an account.
    pub(crate) fn new_terminal_with_account(
        &mut self,
        workspace: Option<&workspace::Workspace>,
        agent_id: AgentId,
        account: AccountId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.supports_terminal(cx) {
            return;
        }
        let terminal_id = super::TerminalId::new();
        let title = SharedString::from(format!(
            "Terminal · {}",
            target_label(
                &Agent::with_account(agent_id.clone(), Some(account.clone())),
                cx
            )
        ));
        thread_accounts::write_terminal_account(
            terminal_id.to_key_string(),
            thread_accounts::TerminalAccount { agent_id, account },
            cx,
        );
        let working_directory = self.terminal_working_directory(workspace, cx);
        self.spawn_terminal(
            terminal_id,
            working_directory,
            Some(title),
            None,
            None,
            true,
            true,
            true,
            AgentThreadSource::AgentPanel,
            window,
            cx,
        );
    }

    /// With auto-switch on, replaces an account that is out of quota with the
    /// one that has the most left, and says so.
    pub(crate) fn agent_with_quota_left(&self, agent: Agent, cx: &mut Context<Self>) -> Agent {
        if !AgentAccountsSettings::get_global(cx).auto_switch || !self.project.read(cx).is_local() {
            return agent;
        }
        let agent_id = agent.id();
        let Some(current) = AccountRegistry::accounts_for_agent(agent_id.as_ref(), cx)
            .into_iter()
            .find(|account| account.id().as_ref() == agent.account())
        else {
            return agent;
        };
        if !QuotaRegistry::is_exhausted(&current, cx) {
            return agent;
        }
        let Some(alternative) =
            QuotaRegistry::best_alternative(agent_id.as_ref(), agent.account(), cx)
        else {
            return agent;
        };
        self.show_handoff_message(
            format!(
                "{} is almost out of quota; using {} instead.",
                current.label(),
                alternative.label()
            ),
            cx,
        );
        Agent::with_account(agent_id, alternative.id())
    }

    fn handoff_source_thread(&self, cx: &Context<Self>) -> Option<HandoffSourceThread> {
        let view = self.active_conversation_view()?.read(cx);
        let thread = view.root_thread(cx)?;
        let thread = thread.read(cx);
        if thread.is_draft_thread() || thread.entries().is_empty() {
            return None;
        }
        Some(HandoffSourceThread {
            thread_id: view.thread_id,
            agent: view.connection_key().clone(),
            session_id: thread.session_id().clone(),
            title: thread.title(),
            work_dirs: thread.work_dirs().cloned(),
            markdown: thread.to_markdown(cx),
        })
    }

    fn open_handed_off_thread(
        &mut self,
        source: HandoffSourceThread,
        target: Agent,
        new_session_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let thread_id = ThreadId::new();
        let now = Utc::now();
        let work_dirs = source.work_dirs.clone();
        let metadata = ThreadMetadata {
            thread_id,
            session_id: Some(acp::SessionId::new(new_session_id)),
            agent_id: target.id(),
            title: source.title.clone(),
            title_override: None,
            updated_at: now,
            created_at: Some(now),
            interacted_at: Some(now),
            worktree_paths: work_dirs
                .as_ref()
                .map(WorktreePaths::from_folder_paths)
                .unwrap_or_default(),
            remote_connection: None,
            archived: false,
        };
        ThreadMetadataStore::global(cx).update(cx, |store, cx| store.save(metadata, cx));
        // Written before the thread opens, so its account is already known.
        thread_accounts::write(
            thread_id,
            &ThreadAccountInfo {
                account: target.account().cloned(),
                handoff_from: Some(HandoffSource {
                    thread_id: source.thread_id.to_key_string(),
                    agent_id: source.agent.id(),
                    account: source.agent.account().cloned(),
                }),
                continued_in: None,
            },
            cx,
        )
        .detach_and_log_err(cx);
        thread_accounts::record_continuation(source.thread_id, thread_id, cx);
        // Going back to an agent the conversation already ran with: its old
        // thread misses everything since, so the new one replaces it.
        let superseded = thread_accounts::threads_superseded_by(source.thread_id, &target, cx);
        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
            for thread_id in superseded {
                store.archive(thread_id, None, cx);
            }
        });

        self.load_agent_thread(
            target,
            thread_id,
            work_dirs,
            source.title,
            true,
            AgentThreadSource::AgentPanel,
            window,
            cx,
        );
    }

    fn continue_with_transcript(
        &mut self,
        source: HandoffSourceThread,
        target: Agent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let blocks = vec![
            acp::ContentBlock::Text(acp::TextContent::new(
                "This conversation was started with another agent. Its transcript is \
                 attached; read it and continue from where it left off.\n\n"
                    .to_string(),
            )),
            acp::ContentBlock::Resource(acp::EmbeddedResource::new(
                acp::EmbeddedResourceResource::TextResourceContents(
                    acp::TextResourceContents::new(
                        source.markdown,
                        format!("zed:///agent/thread/{}", source.thread_id.to_key_string()),
                    ),
                ),
            )),
        ];
        self.external_thread(
            Some(target),
            None,
            source.work_dirs,
            source.title,
            Some(AgentInitialContent::ContentBlock {
                blocks,
                auto_submit: false,
            }),
            true,
            AgentThreadSource::AgentPanel,
            window,
            cx,
        );
    }

    fn active_thread_is_generating(&self, cx: &App) -> bool {
        self.active_conversation_view()
            .and_then(|view| view.read(cx).root_thread(cx))
            .is_some_and(|thread| thread.read(cx).status() == acp_thread::ThreadStatus::Generating)
    }

    /// Stops a running transfer. txcript is killed with its task; the source
    /// thread was never modified, and a half-written target session is
    /// never opened.
    pub(crate) fn cancel_handoff(&mut self, cx: &mut Context<Self>) {
        if self.pending_handoff.take().is_some() {
            self.show_handoff_message("Cancelled moving the conversation.", cx);
        }
    }

    fn show_handoff_progress(&self, message: String, cx: &mut Context<Self>) {
        let panel = cx.weak_entity();
        self.update_workspace_later(cx, move |workspace, cx| {
            workspace.show_toast(
                Toast::new(NotificationId::unique::<ThreadHandoffToast>(), message).on_click(
                    "Cancel",
                    move |_, cx| {
                        panel.update(cx, |panel, cx| panel.cancel_handoff(cx)).ok();
                    },
                ),
                cx,
            )
        });
    }

    fn dismiss_handoff_message(&self, cx: &mut Context<Self>) {
        self.update_workspace_later(cx, |workspace, cx| {
            workspace.dismiss_toast(&NotificationId::unique::<ThreadHandoffToast>(), cx)
        });
    }

    fn show_handoff_message(&self, message: impl Into<String>, cx: &mut Context<Self>) {
        let message = message.into();
        self.update_workspace_later(cx, move |workspace, cx| {
            workspace.show_toast(
                Toast::new(NotificationId::unique::<ThreadHandoffToast>(), message).autohide(),
                cx,
            )
        });
    }

    /// These run from workspace actions and menus, while the workspace is
    /// already being updated, so its toasts are shown once that finishes.
    fn update_workspace_later(
        &self,
        cx: &mut Context<Self>,
        update: impl FnOnce(&mut workspace::Workspace, &mut Context<workspace::Workspace>) + 'static,
    ) {
        let workspace = self.workspace.clone();
        cx.defer(move |cx| {
            workspace.update(cx, update).ok();
        });
    }
}

fn handoff_cwd(
    source: &HandoffSourceThread,
    panel: &AgentPanel,
    cx: &Context<AgentPanel>,
) -> Option<PathBuf> {
    source
        .work_dirs
        .as_ref()
        .and_then(|dirs| dirs.paths().first().cloned())
        .or_else(|| {
            panel
                .project
                .read(cx)
                .visible_worktrees(cx)
                .next()
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        })
        .filter(|path: &PathBuf| Path::is_dir(path))
}

/// "Codex", or "Claude Code · me@work.dev" when an account is chosen.
pub(crate) fn target_label(agent: &Agent, cx: &gpui::App) -> String {
    let agent_id = agent.id();
    let name = AccountProvider::for_agent(agent_id.as_ref())
        .map(|provider| provider.display_name().to_string())
        .unwrap_or_else(|| agent.label().to_string());
    match agent.account() {
        Some(account) => format!(
            "{name} · {}",
            crate::account_registry::AccountRegistry::label(agent_id.as_ref(), Some(account), cx)
        ),
        None => name,
    }
}

/// Every agent and account a thread with `current` can be continued with.
pub(crate) fn handoff_targets(
    current: &Agent,
    project: &Entity<Project>,
    cx: &App,
) -> Vec<(Agent, SharedString)> {
    let is_local_project = project.read(cx).is_local();
    let agent_server_store = project.read(cx).agent_server_store().read(cx);
    let registry_store = project::AgentRegistryStore::try_global(cx);
    let mut targets = Vec::new();
    for agent_id in agent_server_store.external_agents() {
        let display_name = agent_server_store
            .agent_display_name(agent_id)
            .or_else(|| {
                registry_store
                    .as_ref()
                    .and_then(|store| store.read(cx).agent(agent_id))
                    .map(|agent| agent.name().clone())
            })
            .unwrap_or_else(|| agent_id.0.clone());
        let accounts = if is_local_project {
            AccountRegistry::accounts_for_agent(agent_id.as_ref(), cx)
        } else {
            Vec::new()
        };
        if accounts.len() > 1 {
            for account in accounts {
                targets.push((
                    Agent::with_account(agent_id.clone(), Some(account.selection())),
                    SharedString::from(format!(
                        "{display_name} · {}",
                        account_label_with_quota(&account, cx)
                    )),
                ));
            }
        } else {
            targets.push((Agent::with_account(agent_id.clone(), None), display_name));
        }
    }
    targets.retain(|(agent, _)| {
        agent.id() != current.id()
            || agent.account().filter(|account| !account.is_system()) != current.account()
    });
    targets.sort_by_key(|(_, label)| label.to_lowercase());
    targets
}

// ---------------------------------------------------------------------------
// Panel integration. `agent_panel.rs` stays as upstream wrote it apart from
// one-line calls into what follows, so syncing with Zed rarely conflicts.

/// Continues the active thread in a new thread with another agent or another
/// account, carrying the conversation over.
#[derive(Clone, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = agent)]
#[serde(deny_unknown_fields)]
pub struct ContinueThreadWith {
    /// The agent id to continue with.
    pub agent: AgentId,
    /// The provider account to continue with; omitted for the default account.
    #[serde(default)]
    pub account: Option<AccountId>,
}

/// Creates a new thread with an external agent and one of its accounts.
#[derive(Clone, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = agent)]
#[serde(deny_unknown_fields)]
pub struct NewThreadWithAccount {
    /// The agent id to use for the conversation.
    pub agent: AgentId,
    /// The provider account, as the path of its home directory; omitted for
    /// the agent's default account.
    #[serde(default)]
    pub account: Option<AccountId>,
}

pub(crate) fn register(workspace: &mut Workspace) {
    crate::add_account_modal::register(workspace);
    workspace
        .register_action(|workspace, action: &ContinueThreadWith, window, cx| {
            if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                panel.update(cx, |panel, cx| {
                    panel.continue_thread_with(
                        action.agent.clone(),
                        action.account.clone(),
                        window,
                        cx,
                    )
                });
                workspace.focus_panel::<AgentPanel>(window, cx);
            }
        })
        .register_action(|workspace, action: &NewThreadWithAccount, window, cx| {
            if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                workspace.focus_panel::<AgentPanel>(window, cx);
                panel.update(cx, |panel, cx| {
                    panel.new_thread_with_account(action, window, cx)
                });
            }
        });
}

impl AgentPanel {
    fn new_thread_with_account(
        &mut self,
        action: &NewThreadWithAccount,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.has_open_project(cx) {
            return;
        }
        let account = AccountRegistry::resolve(action.agent.as_ref(), action.account.clone(), cx);
        let agent = Agent::with_account(action.agent.clone(), account);
        self.selected_agent = self.agent_with_quota_left(agent, cx);
        self.activate_new_thread(true, AgentThreadSource::AgentPanel, window, cx);
    }

    /// The agent a thread started without picking an account runs with: the
    /// configured default account, or another one if that is out of quota.
    pub(super) fn agent_for_new_thread(&self, agent: Agent, cx: &mut Context<Self>) -> Agent {
        let agent = match agent {
            Agent::Custom { id } => {
                let account = AccountRegistry::resolve(id.as_ref(), None, cx);
                Agent::with_account(id, account)
            }
            agent => agent,
        };
        self.agent_with_quota_left(agent, cx)
    }

    /// The account environment for the terminal being spawned.
    pub(super) fn pending_terminal_account_env(
        &self,
        cx: &App,
    ) -> collections::HashMap<String, String> {
        self.pending_terminal_spawn
            .as_ref()
            .and_then(|terminal_id| {
                thread_accounts::terminal_account(&terminal_id.to_key_string(), cx)
            })
            .map(|account| account.env())
            .unwrap_or_default()
    }

    pub(super) fn handoff_targets_for_menu(
        &self,
        has_thread_messages: bool,
        cx: &App,
    ) -> Vec<(Agent, SharedString)> {
        if has_thread_messages {
            self.handoff_targets(cx)
        } else {
            Vec::new()
        }
    }
}

/// The options menu's "Continue with…" submenu.
pub(super) fn continue_with_submenu(
    menu: ContextMenu,
    targets: &[(Agent, SharedString)],
) -> ContextMenu {
    if targets.is_empty() {
        return menu;
    }
    let targets = targets.to_vec();
    menu.submenu("Continue with…", move |mut menu, _, _| {
        for (target, label) in &targets {
            menu = menu.action(
                label.clone(),
                Box::new(ContinueThreadWith {
                    agent: target.id(),
                    // Explicit, so the CLI's own home stays pickable while
                    // another account is the default.
                    account: Some(target.account().cloned().unwrap_or_else(AccountId::system)),
                }),
            );
        }
        menu
    })
}

/// The new-thread button's label, with the selected agent's account.
pub(super) fn label_with_account(label: SharedString, agent: &Agent, cx: &App) -> SharedString {
    match agent.account() {
        Some(account) => format!(
            "{label} · {}",
            AccountRegistry::label(agent.id().as_ref(), Some(account), cx)
        )
        .into(),
        None => label,
    }
}

/// The new-thread menu's accounts section: a thread entry per account of
/// agents with several, terminals per non-default account, and "Add Account".
pub(super) fn account_menu_entries(
    mut menu: ContextMenu,
    workspace: &WeakEntity<Workspace>,
    agent_server_store: &Entity<project::AgentServerStore>,
    is_via_collab: bool,
    cx: &mut App,
) -> ContextMenu {
    let is_local_project = workspace
        .upgrade()
        .is_some_and(|workspace| workspace.read(cx).project().read(cx).is_local());
    if !is_local_project {
        return menu;
    }
    let store = agent_server_store.read(cx);
    let registry_store = project::AgentRegistryStore::try_global(cx);
    let mut agents: Vec<(AgentId, SharedString, Option<SharedString>)> = store
        .external_agents()
        .filter(|agent_id| AccountProvider::for_agent(agent_id.as_ref()).is_some())
        .map(|agent_id| {
            let registry_agent = registry_store
                .as_ref()
                .and_then(|registry| registry.read(cx).agent(agent_id));
            let name = store
                .agent_display_name(agent_id)
                .or_else(|| registry_agent.map(|agent| agent.name().clone()))
                .unwrap_or_else(|| agent_id.0.clone());
            let icon = store
                .agent_icon(agent_id)
                .or_else(|| registry_agent.and_then(|agent| agent.icon_path().cloned()));
            (agent_id.clone(), name, icon)
        })
        .collect();
    agents.sort_by_key(|(_, name, _)| name.to_lowercase());

    let mut has_header = false;
    for (agent_id, name, icon) in &agents {
        let accounts = AccountRegistry::accounts_for_agent(agent_id.as_ref(), cx);
        if accounts.is_empty() {
            continue;
        }
        if !has_header {
            menu = menu.separator().header("Accounts");
            has_header = true;
        }
        for account in accounts {
            let label = format!("{name} · {}", account_label_with_quota(&account, cx));
            let action = NewThreadWithAccount {
                agent: agent_id.clone(),
                account: Some(account.selection()),
            };
            let entry = ContextMenuEntry::new(label)
                .when_some(icon.clone(), |entry, icon| entry.custom_icon_svg(icon))
                .when(icon.is_none(), |entry| entry.icon(IconName::Sparkle))
                .icon_color(Color::Muted)
                .disabled(is_via_collab)
                .handler(move |window, cx| window.dispatch_action(Box::new(action.clone()), cx));
            menu = menu.item(entry);
        }
    }

    for provider in AccountProvider::ALL {
        let agent_id = provider.agent_id();
        for account in AccountRegistry::accounts_for_agent(agent_id, cx) {
            let Some(account_id) = account.id() else {
                continue;
            };
            if !has_header {
                menu = menu.separator().header("Accounts");
                has_header = true;
            }
            let workspace = workspace.clone();
            menu = menu.item(
                ContextMenuEntry::new(format!(
                    "Terminal · {} · {}",
                    provider.display_name(),
                    account.label()
                ))
                .icon(IconName::Terminal)
                .icon_color(Color::Muted)
                .disabled(is_via_collab)
                .handler(move |window, cx| {
                    let Some(workspace) = workspace.upgrade() else {
                        return;
                    };
                    let account_id = account_id.clone();
                    workspace.update(cx, |workspace, cx| {
                        if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                            panel.update(cx, |panel, cx| {
                                panel.new_terminal_with_account(
                                    Some(workspace),
                                    agent_id.into(),
                                    account_id,
                                    window,
                                    cx,
                                );
                            });
                        }
                    });
                }),
            );
        }
    }

    if agents.is_empty() {
        return menu;
    }
    let targets: Vec<(AgentId, SharedString)> = agents
        .into_iter()
        .map(|(agent_id, name, _)| (agent_id, name))
        .collect();
    menu.separator()
        .submenu("Add Account", move |mut menu, _, _| {
            for (agent_id, name) in &targets {
                menu = menu.action(
                    name.clone(),
                    Box::new(crate::add_account_modal::AddAgentAccount {
                        agent: agent_id.clone(),
                    }),
                );
            }
            menu
        })
}
