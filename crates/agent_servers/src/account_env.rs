//! Account environment for an agent's processes, from this fork's agent
//! accounts.

use collections::HashMap;
use task::SpawnInTerminal;

/// Applies a connection's environment overrides to one of its login tasks.
/// The task id includes them, so that logins for different accounts of the
/// same agent run in separate terminals.
pub(crate) fn with_env_overrides(
    mut task: SpawnInTerminal,
    env_overrides: &HashMap<String, String>,
) -> SpawnInTerminal {
    if env_overrides.is_empty() {
        return task;
    }
    let mut overrides = env_overrides.iter().collect::<Vec<_>>();
    overrides.sort();
    for (key, value) in &overrides {
        task.id.0.push_str(&format!("-{key}={value}"));
    }
    task.env.extend(
        env_overrides
            .iter()
            .map(|(key, value)| (key.clone(), value.clone())),
    );
    task
}

#[cfg(test)]
mod tests {
    use super::*;
    use task::TaskId;

    #[test]
    fn login_tasks_get_the_account_environment() {
        let plain = SpawnInTerminal {
            id: TaskId("external-agent-claude-acp-login".into()),
            env: HashMap::from_iter([("CLAUDE_CONFIG_DIR".into(), "/from/settings".into())]),
            ..SpawnInTerminal::default()
        };
        let overrides =
            HashMap::from_iter([("CLAUDE_CONFIG_DIR".into(), "/Users/me/.claude-work".into())]);
        let task = with_env_overrides(plain.clone(), &overrides);
        assert_eq!(
            task.env.get("CLAUDE_CONFIG_DIR").map(String::as_str),
            Some("/Users/me/.claude-work")
        );
        // Logins for different accounts don't share a terminal.
        assert_ne!(task.id, plain.id);
        assert_eq!(
            with_env_overrides(plain.clone(), &HashMap::default()),
            plain
        );
    }
}
