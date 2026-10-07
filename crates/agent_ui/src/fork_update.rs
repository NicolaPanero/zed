//! Tells users of this fork's release builds when a newer build is out, and
//! installs it with the fork's install script, which replaces the app and
//! reopens it once this process has quit.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use db::kvp::KeyValueStore;
use futures::AsyncReadExt as _;
use gpui::{App, AppContext as _, DismissEvent, TaskExt as _};
use http_client::{AsyncBody, HttpClient, Method, Request};
use ui::IconName;
use util::ResultExt as _;
use workspace::notifications::{
    NotificationId, show_app_notification, simple_message_notification::MessageNotification,
};

const REPOSITORY: &str = "NicolaPanero/zed";
const INSTALL_SCRIPT_URL: &str =
    "https://raw.githubusercontent.com/NicolaPanero/zed/main/script/install-fork.sh";
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(30);
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const NAMESPACE: &str = "fork_update";
const SKIPPED_RELEASE_KEY: &str = "skipped_release";

/// The release this build was published as, set by the fork's release
/// workflow. Local builds have none and never check.
const CURRENT_RELEASE: Option<&str> = option_env!("ZED_FORK_RELEASE");

struct ForkUpdateNotification;

pub fn init(cx: &mut App) {
    let Some(current_release) = CURRENT_RELEASE else {
        return;
    };
    let http = cx.http_client();
    cx.spawn(async move |cx| {
        cx.background_executor().timer(FIRST_CHECK_DELAY).await;
        loop {
            match latest_release(&http).await {
                Ok(latest_release) => {
                    cx.update(|cx| offer_update(current_release, latest_release, cx));
                }
                Err(error) => log::info!("checking for a newer fork build failed: {error:#}"),
            }
            cx.background_executor().timer(CHECK_INTERVAL).await;
        }
    })
    .detach();
}

async fn latest_release(http: &Arc<dyn HttpClient>) -> Result<String> {
    let request = Request::builder()
        .method(Method::GET)
        .uri(format!(
            "https://api.github.com/repos/{REPOSITORY}/releases/latest"
        ))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "zed-fork-update-check")
        .body(AsyncBody::default())?;
    let mut response = http.send(request).await?;
    let mut body = String::new();
    response.body_mut().read_to_string(&mut body).await?;
    anyhow::ensure!(
        response.status().is_success(),
        "GitHub answered {}",
        response.status()
    );
    release_tag(&body)
}

fn release_tag(body: &str) -> Result<String> {
    let release: serde_json::Value = serde_json::from_str(body).context("parsing the release")?;
    release
        .get("tag_name")
        .and_then(|tag| tag.as_str())
        .map(str::to_string)
        .context("the release has no tag")
}

fn offer_update(current_release: &str, latest_release: String, cx: &mut App) {
    if latest_release == current_release {
        return;
    }
    let skipped = KeyValueStore::global(cx)
        .scoped(NAMESPACE)
        .read(SKIPPED_RELEASE_KEY)
        .log_err()
        .flatten();
    if skipped.as_deref() == Some(latest_release.as_str()) {
        return;
    }
    show_app_notification(
        NotificationId::unique::<ForkUpdateNotification>(),
        cx,
        move |cx| {
            let latest_release = latest_release.clone();
            cx.new(|cx| {
                MessageNotification::new(
                    format!("A newer build of this Zed fork is available: {latest_release}."),
                    cx,
                )
                .primary_message("Update and Restart")
                .primary_icon(IconName::Download)
                .primary_on_click(|_, cx| {
                    install_update(cx);
                    cx.emit(DismissEvent);
                })
                .secondary_message("Skip This Version")
                .secondary_on_click(move |_, cx| {
                    let kvp = KeyValueStore::global(cx);
                    let latest_release = latest_release.clone();
                    cx.background_spawn(async move {
                        kvp.scoped(NAMESPACE)
                            .write(SKIPPED_RELEASE_KEY.to_string(), latest_release)
                            .await
                    })
                    .detach_and_log_err(cx);
                    cx.emit(DismissEvent);
                })
            })
        },
    );
}

/// Starts the install script, which waits for this process to exit, then
/// replaces the app and reopens it.
fn install_update(cx: &mut App) {
    let script = update_script(std::process::id());
    match util::command::new_command("/bin/sh")
        .arg("-c")
        .arg(&script)
        .spawn()
    {
        Ok(_) => cx.quit(),
        Err(error) => log::error!("starting the fork update failed: {error:#}"),
    }
}

fn update_script(pid: u32) -> String {
    format!(
        "mkdir -p \"$HOME/Library/Logs/Zed\" && \
         curl -fsSL {INSTALL_SCRIPT_URL} | sh -s -- --wait-pid {pid} --relaunch \
         >\"$HOME/Library/Logs/Zed/fork-update.log\" 2>&1"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_release_tag() {
        assert_eq!(
            release_tag(r#"{"tag_name":"v1.22.0-fork.9b169e3","name":"x"}"#).unwrap(),
            "v1.22.0-fork.9b169e3"
        );
        assert!(release_tag(r#"{"message":"Not Found"}"#).is_err());
    }

    #[test]
    fn update_waits_for_this_process_and_relaunches() {
        let script = update_script(4242);
        assert!(script.contains("--wait-pid 4242 --relaunch"));
        assert!(script.starts_with("mkdir -p"));
        assert!(script.contains(INSTALL_SCRIPT_URL));
    }
}
