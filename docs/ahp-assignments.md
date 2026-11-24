# Full assignments on an existing AHP chat

Use these commands when an assignment needs a particular editor chat, model and reasoning level, or when the editor needs the client to serve skill files during the turn. The Rust executable owns the AHP 1.1 connection through completion. The only child process it runs is the configured VS Code CLI for local host discovery.

```sh
agent-delegation ahp check  "$config" "$receipt"
agent-delegation ahp submit "$config" "$receipt" "$prompt_file"
```

`check` uses a fresh observer identity and prints a summary without chat text. `submit` opens the existing session and chat recorded in the receipt, verifies the model and reasoning level against that session provider's catalog, and confirms that the configured plugin skills have loaded before starting the turn. The UTF-8 prompt file is passed through with its whitespace intact. Keep the process running if it reports `waiting`. It continues serving read-only plugin resources and observing the recorded turn.

## Run by target name

Give the saved pair a name in the local target registry. The paths can be absolute or relative to the directory containing `targets.toml`.

```toml
[targets.work-neon]
backend = "ahp"
assignment = { config = "work/neon-config.json", receipt = "work/neon-receipt.json" }
```

```sh
agent-delegation --target work-neon check
agent-delegation --target work-neon submit "$prompt_file"
```

After an interrupted assignment, `--target work-neon inspect` and `--target work-neon watch` use the turn ID in the same receipt. Direct and named invocations share its lock and transcript. The prompt path is resolved from the caller's working directory.

## Local configuration

An existing operator's config and receipt can be used in place. For a new configuration, identify the exact CLI, profile, editor instance, workspace, provider and model using local VS Code discovery. To require a reasoning level, set `model.thinkingLevel` to a value advertised for the model. The client checks it before starting the turn. Omit the setting in both files to use the editor's default, including for models without a reasoning control.

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

Stdout contains one JSON record per line. When `submit` or `watch` confirms a terminal turn, its record includes `outcome` (`complete`, `cancelled`, or `error`), `turnId`, `hasWrittenReply`, and `transcript`. A completed turn can contain only tool calls. `hasWrittenReply` tells you whether that exact turn contains any nonblank Markdown. To review the answer, open the returned private transcript and read the `turns` entry with the matching ID. A later follow-up has its own ID. While the process is running, `<receipt>.status.json` contains progress metadata.

The client records the turn ID, prompt hash, and client sequence in the receipt before dispatch. If the editor explicitly rejects that start, the client records `rejected` and prints the turn ID. This outcome has no transcript because the turn never started. A subsequent `watch` reports the saved rejection, and a new `submit` can start another assignment.

A disconnect before confirmation leaves delivery uncertain and blocks another submission. After the original process exits, inspect the saved state or reconnect to the recorded attempt.

```sh
agent-delegation ahp inspect "$config" "$receipt"
agent-delegation ahp watch   "$config" "$receipt"
```

`inspect` returns the editor's current snapshot in `<receipt>.inspect.json`. It can settle an unresolved receipt when the exact turn is present. The last confirmed `submit` or `watch` snapshot stays in `<receipt>.chat.json`, and an inspection preserves the recorded terminal outcome even if the editor's history changes. Each path holds the latest snapshot of its kind.

`watch` retrieves an already completed turn, recognizes a replayed rejection of the exact start dispatch, or resumes observation and skill serving for the recorded active turn. It never resends a prompt. An operational failure exits with code 2 and a diagnostic on stderr. If a process crashed and left a receipt lock, establish that the original operator has stopped before removing its lock.
