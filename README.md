# Zed

[![Zed](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/zed-industries/zed/main/assets/badge/v0.json)](https://zed.dev)
[![CI](https://github.com/zed-industries/zed/actions/workflows/run_tests.yml/badge.svg)](https://github.com/zed-industries/zed/actions/workflows/run_tests.yml)

Welcome to Zed, a high-performance, multiplayer code editor from the creators of [Atom](https://github.com/atom/atom) and [Tree-sitter](https://github.com/tree-sitter/tree-sitter).

---

## About this fork

This is [NicolaPanero/zed](https://github.com/NicolaPanero/zed), a fork of Zed's stable `v1.22.x` branch that adds multi-account support and hand-off between external agents (Claude Code, Codex, Grok, Cursor, OpenCode). The `v1.22.x` branch here is Zed's stable branch plus these features. A daily workflow (`.github/workflows/sync_fork.yml` on `main`) merges Zed's own `main` and `v1.22.x` into this fork and emails the owner if a merge fails.

### Agent accounts

- **Several logins per agent.** An account is the home directory the agent CLI starts with: `CLAUDE_CONFIG_DIR` (Claude Code), `CODEX_HOME` (Codex), `GROK_HOME` (Grok), or a stand-in `HOME` with Cursor's file credential store. OpenCode, the official registry agent, has a single account (its `~/.local/share/opencode` data directory), since it signs in to several model providers itself. Each account runs in its own agent process; the CLI's own home stays the default.
- **Found automatically.** Profiles such as `~/.claude-work`, `~/.codex-2`, `~/.grok-2`, `~/.cursor-work` or `~/.config/claude-*` are discovered by reading identity files only, never credentials.
- **Where to pick them:**
  - an account picker next to the mode and model selectors;
  - one entry per account in the agent panel's "+" menu;
  - terminals that start with an account's environment.
- **Adding one.** "+" → "Add Account" (or "Add Account…" in the account picker) creates a profile directory and opens a thread where the agent's own sign-in runs inside it.
- **Default account.** Mark one as the default for new threads from the "Agent Accounts & Usage" page ("Make default") or in settings.
- **Managing them.** The "Agent Accounts & Usage" page (agent panel "…" → "Accounts & Usage", or `agent: open agent accounts`) lists every account; each card's "…" menu renames it or removes it (its directory goes to the Trash).

### Continue with…

- The account picker under the message editor ("Continue with agent") or the agent panel's "…" → "Continue with…" moves the current conversation to another agent or account.
- The conversation is converted with the `txcript` CLI (bundled in release builds; otherwise it must be on your `PATH`) into the target agent's own session store, then reopened there, so the agent really has the history.
- Native transfer works for Claude Code, Codex, Grok and OpenCode. For Cursor, or if `txcript` fails, the transcript is sent as the first message of a new thread instead.
- **Known limitation: Cursor.** Conversations to or from Cursor are not moved natively. txcript writes the session where Cursor's CLI keeps it (`~/.cursor/chats`), and `cursor-agent --resume <id>` in a terminal does pick it up. Zed, however, runs Cursor's ACP server (`cursor-agent acp`), which only reads its own `acp-sessions` store. A session copied there replays in the thread, but the model never receives it and answers without the history; Cursor most likely keeps the conversation state on its servers. So Cursor gets the transcript as the first message of a new thread: the model reads the whole conversation, but it is not a native session.
- The conversation stays in the same thread: the panel and the sidebar entry switch to the new agent, and the sidebar shows the agents it went through ("from Claude Code → Codex"). The old agent's session is deleted from its store once the new one is written, since it would only be a stale copy.
- With the transcript fallback the transcript is sent right away, and the old session is kept: it is the only native copy of the conversation.

### Quota and usage

- **Quota.** The account picker and menus show each account's quota (session and weekly windows for Claude and Codex, weekly for Grok, and for OpenCode those of the Anthropic and OpenAI subscriptions it is signed in to), refreshed at most every 5 minutes.
- **Out of quota.** When an agent reports that an account is out of quota or credits, the thread offers to continue with another account or agent.
- **Agent Accounts & Usage page** (agent panel "…" → "Accounts & Usage", or the `agent: open agent usage` command):
  - every account with its quota windows, plan and reset times;
  - token usage over 7, 30 or 90 days, estimated at API rates from the agents' local session logs (OpenCode's from its database, with the cost it recorded), plus Cursor's own usage events.

### Settings

```json
"agent_accounts": {
  "discover": true,
  "accounts": [
    { "agent": "claude-acp", "home": "~/.claude-work", "name": "Work", "default": true }
  ],
  "auto_switch": { "enabled": false, "threshold_percent": 95 }
}
```

- `accounts` adds or renames profiles. `agent` is `claude-acp`, `codex-acp`, `cursor`, or the id of your Grok agent server.
- `default` makes an account the one new threads use.
- With `auto_switch` enabled, new threads avoid accounts whose quota is above the threshold.

Accounts are local only: they are hidden in remote projects.

### Grok Build

Zed's agent registry installs Grok Build (`grok-build`) through npm and starts its `bin/grok` with Node, but the package replaces that file with Grok's native executable, so the agent fails to launch with `SyntaxError: Invalid or unexpected token`. Add Grok as a custom agent instead, pointing at the Grok CLI you already have installed:

```json
"agent_servers": {
  "Grok Build": {
    "type": "custom",
    "command": "/Users/<you>/.grok/bin/grok",
    "args": ["agent", "stdio"],
    "default_config_options": { "model": "grok-4.7" }
  }
}
```

`default_config_options.model` is only needed when Grok runs on an API key for a model configured in `~/.grok/config.toml` (here `grok-4.7`): sessions continued from another agent otherwise reopen on Grok's default model, which needs a `grok login`. Any agent id containing "grok" gets Grok's accounts, quota and "Continue with…".

### Threads

Besides archiving, a thread's right-click menu in the threads sidebar has "Delete Permanently…": after confirming, the thread and the agent's saved session (in the account it ran with) are deleted. Archived threads can also be deleted from the archive view, as in Zed.

### Looks like VS Code

Release builds default to VS Code's look: the bundled "VS Code Dark Modern" and "VS Code Light Modern" themes following the system appearance, the VS Code keymap, Menlo at 12 px with 1.5 line height, the system UI font at 13 px, the minimap, and a Menlo 12 px terminal (Zed's terminal otherwise follows the editor's font size). These are defaults only (the `preview` block of `assets/settings/default.json`), so anything in your own settings wins; the themes can also be picked in any build from the theme selector.

### Installing and updating

On an Apple Silicon Mac, install or update the latest build from this fork's releases with:

```sh
curl -fsSL https://raw.githubusercontent.com/NicolaPanero/zed/main/script/install-fork.sh | sh
```

- The app is called **Zed Fork** and runs as Zed's "preview" channel, so it can sit next to an official Zed. Settings are shared and kept across updates (`~/.config/zed`).
- It isn't notarized by Apple. Installed with the command above it opens normally; downloaded with a browser, open it once via System Settings → Privacy & Security → "Open Anyway".
- When a newer build is out, the app offers "Update and Restart", which runs the same script. Zed's own updater is off in these builds.
- A release workflow (`.github/workflows/fork_release.yml` on `main`) builds and publishes a new release whenever this branch changes.

### Building this fork

Follow Zed's build guide below. On macOS:

```sh
cargo build --release
```

Then run `target/release/zed`. Shader compilation needs Xcode's Metal toolchain (`xcodebuild -downloadComponent MetalToolchain`).

---

### Installation

On macOS, Linux, and Windows you can [download Zed directly](https://zed.dev/download) or install Zed via your local package manager ([macOS](https://zed.dev/docs/installation#macos)/[Linux](https://zed.dev/docs/linux#installing-via-a-package-manager)/[Windows](https://zed.dev/docs/windows#package-managers)).

Other platforms are not yet available:

- Web ([tracking discussion](https://github.com/zed-industries/zed/discussions/26195))

### Developing Zed

- [Building Zed for macOS](./docs/src/development/macos.md)
- [Building Zed for Linux](./docs/src/development/linux.md)
- [Building Zed for Windows](./docs/src/development/windows.md)

### Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) for ways you can contribute to Zed.

Also... we're hiring! Check out our [jobs](https://zed.dev/jobs) page for open roles.

### Licensing

Zed source code is licensed primarily under GPL-3.0-or-later, with Apache-2.0 components where marked.

License information for third party dependencies must be correctly provided for CI to pass.

We use [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) to automatically comply with open source licenses. If CI is failing, check the following:

- Is it showing a `no license specified` error for a crate you've created? If so, add `publish = false` under `[package]` in your crate's Cargo.toml.
- Is the error `failed to satisfy license requirements` for a dependency? If so, first determine what license the project has and whether this system is sufficient to comply with this license's requirements. If you're unsure, ask a lawyer. Once you've verified that this system is acceptable add the license's SPDX identifier to the `accepted` array in `script/licenses/zed-licenses.toml`.
- Is `cargo-about` unable to find the license for a dependency? If so, add a clarification field at the end of `script/licenses/zed-licenses.toml`, as specified in the [cargo-about book](https://embarkstudios.github.io/cargo-about/cli/generate/config.html#crate-configuration).

## Sponsorship

Zed is developed by **Zed Industries, Inc.**, a for-profit company.

If you’d like to financially support the project, you can do so via GitHub Sponsors.
Sponsorships go directly to Zed Industries and are used as general company revenue.
There are no perks or entitlements associated with sponsorship.
