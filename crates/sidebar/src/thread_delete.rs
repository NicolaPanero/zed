//! "Delete Permanently…" for threads, from this fork: archives the thread so
//! its views close as usual, then deletes it for good.

use agent_client_protocol::schema::v1 as acp;
use agent_ui::AgentPanel;
use agent_ui::thread_metadata_store::ThreadMetadataStore;
use gpui::{Context, PromptLevel, TaskExt as _, Window};

use crate::Sidebar;

impl Sidebar {
    pub(crate) fn delete_thread_permanently(
        &mut self,
        session_id: &acp::SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(metadata) = ThreadMetadataStore::global(cx)
            .read(cx)
            .entry_by_session(session_id)
            .cloned()
        else {
            return;
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Delete \"{}\"?", metadata.display_title()),
            Some("The thread and the agent's saved session are deleted for good."),
            &["Delete", "Cancel"],
            cx,
        );
        let session_id = session_id.clone();
        cx.spawn_in(window, async move |this, cx| {
            if answer.await.ok() != Some(0) {
                return anyhow::Ok(());
            }
            this.update_in(cx, |this, window, cx| {
                this.archive_thread(&session_id, window, cx);
                let connection_store = this
                    .active_workspace(cx)
                    .and_then(|workspace| workspace.read(cx).panel::<AgentPanel>(cx))
                    .map(|panel| panel.read(cx).connection_store().clone());
                if let Some(connection_store) = connection_store {
                    agent_ui::thread_accounts::delete_thread_permanently(
                        metadata.thread_id,
                        metadata.session_id.clone(),
                        metadata.agent_id.clone(),
                        &connection_store,
                        cx,
                    );
                }
            })
        })
        .detach_and_log_err(cx);
    }
}
