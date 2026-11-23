# Full assignments on an existing AHP chat

Use these commands when an assignment needs a particular editor chat, model and reasoning level, or when the editor needs the client to serve skill files during the turn. The Rust executable owns the AHP 1.1 connection through completion. The only child process it runs is the configured VS Code CLI for local host discovery.

```sh
agent-delegation ahp check  "$config" "$receipt"
agent-delegation ahp submit "$config" "$receipt" "$prompt_file"
```

`check` uses a fresh observer identity and prints a summary without chat text. `submit` opens the existing session and chat recorded in the receipt, verifies the model and reasoning level against that session provider's catalog, and confirms that the configured plugin skills have loaded before starting the turn. The UTF-8 prompt file is passed through with its whitespace intact. Keep the process running if it reports `waiting`. It continues serving read-only plugin resources and observing the recorded turn.

## Local configuration

An existing operator's config and receipt can be used in place. For a new configuration, identify the exact CLI, profile, editor instance, workspace, provider and model using local VS Code discovery. Set `model.thinkingLevel` to a value advertised for the model. An example with a skill plugin follows.

```json
{
  "cli": "/absolute/path/to/code",
  "profile": "/absolute/path/to/editor-user-data",
  "instanceId": "EDITOR_INSTANCE_ID",
  "workspace": "/absolute/path/to/workspace",
  "provider": "SESSION_PROVIDER_ID",
  "model": {"id": "EXACT_MODEL_ID", "thinkingLevel": "high"},
  "pluginRoot": "/absolute/path/to/plugin",
  "pluginRevision": "installed-revision",
  "requiredSkills": ["review"]
}
```

The receipt is a private JSON object. Its `target` contains the same `cli`, `profile`, `instanceId`, `workspace`, `provider`, `model` and `pluginRoot` values. At the top level, set `session` and `chat` to the selected resource URIs and `clientId` to the operator's ID. A new owner gets a fresh UUID. Existing owners reuse their receipt, including the client and server sequences and any recorded attempt. Use one receipt path for the owner. Keep this file and its adjacent output files in a private directory.

For assignments that use the editor's own guidance, omit `pluginRoot`, `pluginRevision`, and `requiredSkills`. A configured `pluginName` overrides the name in the plugin manifest. To require a specific context window, put `maxContextWindow` in both the config and the receipt target.

## Completion and recovery

Stdout contains one JSON record per line. A terminal record includes `outcome` (`complete`, `cancelled`, or `error`), `turnId`, and `transcript`. The transcript is the private local chat snapshot. Read the entry in `turns` with that exact ID to get its response. All three are terminal outcomes, so use the record to determine whether the assignment succeeded. While the process is running, `<receipt>.status.json` contains progress metadata.

The client records its turn ID and prompt hash before dispatch. A disconnect can leave delivery uncertain. After the original process exits, inspect the saved state or reconnect to that same attempt:

```sh
agent-delegation ahp inspect "$config" "$receipt"
agent-delegation ahp watch   "$config" "$receipt"
```

`watch` retrieves an already completed turn or resumes observation and skill serving for the recorded active turn. It never resends a prompt. An operational failure exits with code 2 and a diagnostic on stderr. If a process crashed and left a receipt lock, establish that the original operator has stopped before removing its lock. Repeating `submit` starts a new assignment.
