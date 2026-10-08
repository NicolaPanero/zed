//! The account picker shown next to the mode and model selectors.

use acp_thread::AcpThread;
use gpui::{AnyElement, App, Entity, WeakEntity};
use project::Project;
use ui::{
    Button, Callout, CalloutBorderPosition, ContextMenu, ContextMenuEntry, PopoverMenu, Severity,
    Tooltip, prelude::*,
};

use crate::account_registry::{
    AccountRegistry, AgentAccountsSettings, QuotaRegistry, account_label_with_quota,
};
use crate::add_account_modal::AddAgentAccount;
use crate::agent_panel::thread_handoff::{ContinueThreadWith, NewThreadWithAccount};
use crate::conversation_view::ThreadError;
use crate::thread_accounts;
use crate::thread_metadata_store::ThreadId;
use crate::{Agent, ConversationView};
use settings::Settings as _;

/// Renders the picker when the thread's agent has more than one account.
///
/// Picking another account starts an empty thread over with it, or, once the
/// conversation has messages, continues it with that account.
pub(crate) fn render_account_selector(
    agent: &Agent,
    has_messages: bool,
    is_local_project: bool,
    other_agents: Vec<(Agent, SharedString)>,
    cx: &App,
) -> Option<AnyElement> {
    // Account homes are local paths; remote agents can't use them.
    if !is_local_project {
        return None;
    }
    let agent_id = agent.id();
    let accounts = AccountRegistry::accounts_for_agent(agent_id.as_ref(), cx);
    if accounts.is_empty() && other_agents.is_empty() {
        return None;
    }
    let current = agent.account().cloned();
    let current_label = if accounts.is_empty() {
        "Continue with…".to_string()
    } else {
        AccountRegistry::label(agent_id.as_ref(), current.as_ref(), cx)
    };

    let trigger = Button::new("account-selector-trigger", current_label)
        .label_size(LabelSize::Small)
        .color(Color::Muted)
        .end_icon(
            Icon::new(IconName::ChevronDown)
                .size(IconSize::XSmall)
                .color(Color::Muted),
        );
    let tooltip = if has_messages {
        "Continue this conversation with another account or agent"
    } else {
        "Choose the account for this thread"
    };

    Some(
        PopoverMenu::new("account-selector")
            .trigger_with_tooltip(trigger, Tooltip::text(tooltip))
            .anchor(gpui::Anchor::BottomRight)
            .offset(gpui::Point {
                x: px(0.0),
                y: px(-2.0),
            })
            .menu(move |window, cx| {
                QuotaRegistry::refresh_if_stale(&accounts, cx);
                let accounts = accounts.clone();
                let current = current.clone();
                let agent_id = agent_id.clone();
                let other_agents = other_agents.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, cx| {
                    if !accounts.is_empty() {
                        menu = menu.header(if has_messages {
                            "Continue with account"
                        } else {
                            "Account"
                        });
                    }
                    for account in &accounts {
                        let account_id = Some(account.selection());
                        let is_current = account.id() == current;
                        let label = account_label_with_quota(account, cx);
                        let action: Box<dyn gpui::Action> = if has_messages {
                            Box::new(ContinueThreadWith {
                                agent: agent_id.clone(),
                                account: account_id.clone(),
                            })
                        } else {
                            Box::new(NewThreadWithAccount {
                                agent: agent_id.clone(),
                                account: account_id.clone(),
                            })
                        };
                        menu.push_item(
                            ContextMenuEntry::new(label)
                                .toggleable(IconPosition::End, is_current)
                                .disabled(is_current)
                                .handler(move |window, cx| {
                                    window.dispatch_action(action.boxed_clone(), cx)
                                }),
                        );
                    }
                    if !other_agents.is_empty() {
                        menu = menu.separator().header("Continue with agent");
                        for (target, label) in &other_agents {
                            menu = menu.action(
                                label.clone(),
                                Box::new(ContinueThreadWith {
                                    agent: target.id(),
                                    account: Some(
                                        target
                                            .account()
                                            .cloned()
                                            .unwrap_or_else(agent_accounts::AccountId::system),
                                    ),
                                }),
                            );
                        }
                    }
                    let multiple_accounts =
                        agent_accounts::AccountProvider::for_agent(agent_id.as_ref())
                            .is_some_and(|provider| provider.supports_multiple_accounts());
                    if accounts.is_empty() || !multiple_accounts {
                        return menu;
                    }
                    let add_account = AddAgentAccount { agent: agent_id };
                    menu.separator().item(
                        ContextMenuEntry::new("Add Account…")
                            .icon(IconName::Plus)
                            .icon_color(Color::Muted)
                            .handler(move |window, cx| {
                                window.dispatch_action(Box::new(add_account.clone()), cx)
                            }),
                    )
                }))
            })
            .into_any_element(),
    )
}

/// Tells the user that a thread continues a conversation started elsewhere.
pub(crate) fn render_handoff_notice(thread_id: ThreadId, cx: &App) -> Option<AnyElement> {
    let source = thread_accounts::read(thread_id, cx)?.handoff_from?;
    let source_agent = Agent::with_account(source.agent_id, source.account);
    let label = crate::agent_panel::thread_handoff::target_label(&source_agent, cx);
    Some(
        Callout::new()
            .border_position(CalloutBorderPosition::Bottom)
            .severity(Severity::Info)
            .icon(IconName::ArrowRight)
            .title(format!("Continued from {label}"))
            .description("This agent received the whole conversation so far.")
            .into_any_element(),
    )
}

/// With auto-switch on, warns when the thread's account is almost out of
/// quota and offers the account with the most quota left.
pub(crate) fn render_quota_notice(
    agent: &Agent,
    has_messages: bool,
    is_local_project: bool,
    cx: &mut App,
) -> Option<AnyElement> {
    if !is_local_project || !AgentAccountsSettings::get_global(cx).auto_switch {
        return None;
    }
    let agent_id = agent.id();
    let accounts = AccountRegistry::accounts_for_agent(agent_id.as_ref(), cx);
    if accounts.len() < 2 {
        return None;
    }
    // Readings are cached for five minutes, so this rarely fetches.
    let to_refresh = accounts.clone();
    cx.defer(move |cx| QuotaRegistry::refresh_if_stale(&to_refresh, cx));

    let current = accounts
        .iter()
        .find(|account| account.id().as_ref() == agent.account())?;
    if !QuotaRegistry::is_exhausted(current, cx) {
        return None;
    }
    let alternative = QuotaRegistry::best_alternative(agent_id.as_ref(), agent.account(), cx)?;
    let used = QuotaRegistry::quota(current, cx)
        .and_then(|quota| quota.max_used_percent())
        .unwrap_or(100);
    let action: Box<dyn gpui::Action> = if has_messages {
        Box::new(ContinueThreadWith {
            agent: agent_id,
            account: Some(alternative.selection()),
        })
    } else {
        Box::new(NewThreadWithAccount {
            agent: agent_id,
            account: Some(alternative.selection()),
        })
    };
    let button_label = format!("Continue with {}", alternative.label());
    Some(
        Callout::new()
            .border_position(CalloutBorderPosition::Bottom)
            .severity(Severity::Warning)
            .icon(IconName::Warning)
            .title(format!("{} has used {used}% of its quota", current.label()))
            .description(format!(
                "{} has the most quota left. The conversation moves there with its history.",
                account_label_with_quota(&alternative, cx)
            ))
            .actions_slot(
                Button::new("quota-switch-account", button_label)
                    .label_size(LabelSize::Small)
                    .on_click(move |_, window, cx| {
                        window.dispatch_action(action.boxed_clone(), cx)
                    }),
            )
            .into_any_element(),
    )
}

/// After the agent reported the thread's account out of quota or credits,
/// offers to continue the conversation with another account or agent.
pub(crate) fn render_usage_limit_notice(
    agent: &Agent,
    project: &Entity<Project>,
    cx: &App,
) -> AnyElement {
    let agent_id = agent.id();
    let current_label = AccountRegistry::label(agent_id.as_ref(), agent.account(), cx);
    let alternative = project
        .read(cx)
        .is_local()
        .then(|| QuotaRegistry::any_alternative(agent_id.as_ref(), agent.account(), cx))
        .flatten();
    let targets = crate::agent_panel::thread_handoff::handoff_targets(agent, project, cx);

    let mut actions = h_flex().gap_1();
    if let Some(alternative) = &alternative {
        let action = ContinueThreadWith {
            agent: agent_id,
            account: Some(alternative.selection()),
        };
        actions = actions.child(
            Button::new(
                "usage-limit-switch-account",
                format!("Continue with {}", alternative.label()),
            )
            .label_size(LabelSize::Small)
            .on_click(move |_, window, cx| window.dispatch_action(Box::new(action.clone()), cx)),
        );
    }
    if !targets.is_empty() {
        actions = actions.child(
            PopoverMenu::new("usage-limit-continue-with")
                .trigger(
                    Button::new("usage-limit-continue-with-trigger", "Continue with…")
                        .label_size(LabelSize::Small)
                        .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall)),
                )
                .anchor(gpui::Anchor::BottomRight)
                .menu(move |window, cx| {
                    let targets = targets.clone();
                    Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                        for (target, label) in &targets {
                            menu = menu.action(
                                label.clone(),
                                Box::new(ContinueThreadWith {
                                    agent: target.id(),
                                    account: Some(
                                        target
                                            .account()
                                            .cloned()
                                            .unwrap_or_else(agent_accounts::AccountId::system),
                                    ),
                                }),
                            );
                        }
                        menu
                    }))
                }),
        );
    }

    Callout::new()
        .border_position(CalloutBorderPosition::Bottom)
        .severity(Severity::Warning)
        .icon(IconName::Warning)
        .title(format!("{current_label} is out of quota"))
        .description(
            "Continue this conversation with another account or agent; it moves there \
             with its history and this thread stays as it is.",
        )
        .actions_slot(actions)
        .into_any_element()
}

/// Whether the agent says its account ran out of quota or credits.
pub(crate) fn is_usage_limit(error: &ThreadError) -> bool {
    match error {
        ThreadError::Other { message, .. } | ThreadError::ProviderRejection { message } => {
            agent_accounts::is_usage_limit_error(message)
        }
        _ => false,
    }
}

/// Marks the thread's account exhausted when its agent reports it out of
/// quota, so new threads avoid it until its quota is read again.
pub(crate) fn note_thread_error(
    error: &ThreadError,
    server_view: &WeakEntity<ConversationView>,
    cx: &mut App,
) {
    if !is_usage_limit(error) {
        return;
    }
    let Some(view) = server_view.upgrade() else {
        return;
    };
    let agent = view.read(cx).connection_key().clone();
    QuotaRegistry::mark_exhausted(agent.id().as_ref(), agent.account(), cx);
}

/// The account picker for a thread's message editor.
pub(crate) fn thread_account_selector(
    server_view: &WeakEntity<ConversationView>,
    thread: &Entity<AcpThread>,
    project: &WeakEntity<Project>,
    cx: &App,
) -> Option<AnyElement> {
    let view = server_view.upgrade()?;
    let agent = view.read(cx).connection_key();
    let has_messages = !thread.read(cx).entries().is_empty();
    let project = project.upgrade()?;
    // Other agents only: this agent's accounts are listed above them.
    let other_agents = if has_messages {
        crate::agent_panel::thread_handoff::handoff_targets(agent, &project, cx)
            .into_iter()
            .filter(|(target, _)| target.id() != agent.id())
            .collect()
    } else {
        Vec::new()
    };
    render_account_selector(
        agent,
        has_messages,
        project.read(cx).is_local(),
        other_agents,
        cx,
    )
}

/// The notices shown above a thread: where it was continued from, and quota
/// warnings for its account.
pub(crate) fn thread_notices(
    server_view: &WeakEntity<ConversationView>,
    thread: &Entity<AcpThread>,
    project: &WeakEntity<Project>,
    thread_error: Option<&ThreadError>,
    cx: &mut App,
) -> Vec<AnyElement> {
    let Some(view) = server_view.upgrade() else {
        return Vec::new();
    };
    let (agent, thread_id) = {
        let view = view.read(cx);
        (view.connection_key().clone(), view.thread_id)
    };
    let mut notices: Vec<AnyElement> = render_handoff_notice(thread_id, cx).into_iter().collect();
    if thread_error.is_some_and(is_usage_limit) {
        if let Some(project) = project.upgrade() {
            notices.push(render_usage_limit_notice(&agent, &project, cx));
        }
    } else {
        let has_messages = !thread.read(cx).entries().is_empty();
        let is_local = project
            .upgrade()
            .is_some_and(|project| project.read(cx).is_local());
        notices.extend(render_quota_notice(&agent, has_messages, is_local, cx));
    }
    notices
}
