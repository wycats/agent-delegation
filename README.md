# agent-delegation

Choose the host that owns the conversation and the account you want to use. The model ID is a separate choice within that host.

| Channel | Where the work runs | Following up |
| --- | --- | --- |
| `ahp` | An existing VS Code Agent Host chat, using its registered provider and model catalog | Read status, queue or steer a message, or run and recover a full assignment |
| `claude` | A local Claude Code subprocess with first-party Claude sign-in | Steer the running process or resume its exact session |
| `google` (experimental) | A local Antigravity CLI subprocess using your configured personal Google sign-in | Read its result or send another turn to the exact conversation |

The Google adapter has fixture coverage and a real signed-out preflight check. A signed-in Gemini turn and follow-up are still unverified. The AHP client runs natively. The Claude and Google runners are embedded in the executable and use Node.js 22+ plus the respective vendor CLI on your PATH. Each channel keeps its own account, tool permissions and conversation identity. [Claude Code usage](docs/claude.md) · [Google usage and evidence](docs/google.md)

## Name your targets

Save your usual destinations in `~/.config/agent-delegation/targets.toml` on macOS or Linux, or `%APPDATA%\agent-delegation\targets.toml` on Windows. On Unix, setting `XDG_CONFIG_HOME` replaces `~/.config`. You can also select a file with `AGENT_DELEGATION_CONFIG` or the global `--config PATH` option, which takes precedence. Direct channel commands work without a config file.

```toml
[targets.work-neon]
backend = "ahp"
code = "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code"
user_data_dir = "/absolute/path/to/the/owning/profile"
session = "REPLACE_WITH_SESSION_URI"
chat = "REPLACE_WITH_CHAT_URI"
model = "REPLACE_WITH_EXACT_HOST_MODEL_ID"

[targets.personal-neon]
backend = "google"
model = "REPLACE_WITH_SLUG_FROM_AGY_MODELS"

[targets.fable]
backend = "claude"
model = "claude-fable-5-2"
```

```sh
agent-delegation targets
agent-delegation --target work-neon status
agent-delegation --target work-neon send task.txt
agent-delegation --target personal-neon run --cwd "$PWD" --task task.txt --run-dir "$PWD/run-1"
```

An AHP target binds an existing chat to its model and owning installation. Omitting `code` selects `code` on PATH. Omitting `user_data_dir` selects that installation's default profile. A named target takes precedence over the shell's editor and profile settings. Use literal paths in TOML. The AHP client obtains fresh endpoint credentials from the editor for each command.

A Claude or Google target binds the host and model. The vendor CLI supplies its signed-in account. A fresh `run` creates a conversation and records it in the specified run directory. To continue one, supply `--resume-from` and a new `--run-dir`. Several names can use the same model. The run directory identifies the particular conversation. Use `--target NAME --help` for the commands available on that host.

## On a Mac

1. Download the archive for your Mac from the [latest release](https://github.com/wycats/agent-delegation/releases/latest). In Terminal, `uname -m` prints `arm64` on Apple Silicon and `x86_64` on Intel when running natively. From the directory containing the downloaded archive, run the matching command.

   | Mac | Extract |
   | --- | --- |
   | Apple Silicon | `tar -xzf agent-delegation-macos-arm64.tar.gz` |
   | Intel | `tar -xzf agent-delegation-macos-x64.tar.gz` |

   Run `./agent-delegation --help` and `code agent --help` under the same account as VS Code. Its CLI must list the `ps` and `endpoints` subcommands. If the terminal cannot find `code`, run **Shell Command: Install 'code' command in PATH** in the VS Code Command Palette and reopen the terminal.

2. Find the intended session in the Mac's inventory. If the target is absent, open or resume an agent-host-backed chat in VS Code and check again. Copy its full `resource` URI. List its chats and select one by title and resource.

   ```sh
   code agent ps --all --json
   session='REPLACE_WITH_SESSION_URI'
   ./agent-delegation ahp chats "$session"
   chat='REPLACE_WITH_CHAT_URI'
   ./agent-delegation ahp status "$session" "$chat"
   ```

   A successful `status` prints the requested chat URI and its turn state. This is the read-only setup check. Treat the output as conversation data. The inventory and chat catalog can identify projects and chat titles, and `status` includes excerpts from the current and previous turns.

3. To try a round trip, use an idle chat and send a small task. Use `lastModelId` from `status` to reuse the model from its active or most recent turn. If that field is null, use a model ID supported by the session's provider. Alternatively, list the model IDs this session provider advertises with `./agent-delegation ahp models "$session"`. Copy the exact ID, including any provider prefix.

   ```sh
   model='REPLACE_WITH_HOST_MODEL_ID'
   prompt=$(mktemp)
   printf '%s\n' 'Reply with AHP hello.' > "$prompt"
   ./agent-delegation ahp send "$session" "$chat" "$model" "$prompt"
   ./agent-delegation ahp status "$session" "$chat"
   ```

   `accepted: true` means the host accepted the message. Check `status` again for the reply. For feedback during an active turn, use `steer` with the same arguments and a new prompt file. Both commands preserve the contents of the UTF-8 file, including newlines.

For a non-default VS Code installation or user-data directory, set both values in the shell used for the chat. Keep separate values for each profile.

```sh
export AGENT_DELEGATION_CODE="$CODE"
export AGENT_DELEGATION_USER_DATA_DIR="$PROFILE"
"$CODE" agent ps --all --json --user-data-dir "$PROFILE"
./agent-delegation ahp status "$session" "$chat"
```

The AHP client passes the same user-data directory to session and endpoint discovery. A model ID is opaque to routing. An AHP chat using a Google model remains on that editor host even when Antigravity is installed. Selecting the Google backend invokes Antigravity. The low-level `send` returns the host delivery receipt. For a full assignment, `ahp submit` keeps the native Rust client connected, serves configured skills and returns the recorded turn’s result. [Full assignment setup and recovery](docs/ahp-assignments.md)

A custom AHP model provider can advertise its own IDs. The client forwards the chosen ID unchanged. A VS Code distribution with the same discovery commands can be selected with `AGENT_DELEGATION_CODE`, for example `code-insiders`. Run the session inventory with the same CLI. On Windows use a command name on PATH or an absolute path to an executable. If discovery fails, check that the `code` on your PATH belongs to the VS Code installation that owns the session and that its `agent --help` lists both discovery commands. If the endpoint is missing or ambiguous, refresh the session inventory and select the intended chat again. To report a problem, share the error string and `code --version` with identifying details removed.

On Windows x64, extract `agent-delegation-windows-x64.zip` and use `.\agent-delegation.exe` with PowerShell variables. On Linux x64, extract `agent-delegation-linux-x64.tar.gz` and run `./agent-delegation`. Windows uses a named pipe. macOS and Linux use a filesystem Unix socket advertised by the local VS Code CLI.

The original Windows client exchanged tasks and replies with a live editor session. The macOS CI test runs the CLI against a fixture over a real Unix socket and verifies the selected chat's status. Steps 2 and 3 above check the installed VS Code build and signed-in session on your Mac. [Build and test runs](https://github.com/wycats/agent-delegation/actions/workflows/build.yml)

## Builds and releases

Every pull request targeting `main` and every push to `main` tests, builds and runs the release executable on all four platforms. Download a specific commit's archive from its completed Actions run. For public downloads, use the versioned GitHub Release. Each release includes `SHA256SUMS` for checking the downloaded archives.

To build from a source checkout with stable Rust, run `cargo test --locked` followed by `cargo build --release --locked`. The executable is under `target/release/`.

To publish a version, merge the desired Cargo package version into `main`, then push a matching `vX.Y.Z` tag on that commit. After the builds pass, the workflow publishes the four archives and their checksums together.

## How AHP delivery works

1. Find the editor host PID that owns the exact session in `code agent ps --all --json`. Read the matching editor socket and connection token from `code agent endpoints` **in process**. That command's raw output contains credentials. Keep it out of logs, issues, and handoff messages.
2. Connect to the local pipe or Unix socket. Upgrade that stream to a WebSocket using the token, initialize AHP 1.0.0, and subscribe to the session URI. Read the chat catalog from the session snapshot and check membership before subscribing to the chosen chat.
3. Subscribe for action echoes before dispatching `chat/pendingMessageSet`. Use `queued` for a task or `steering` for feedback. Match the echo on chat, action type, message UUID and `origin.clientSeq`, then check for host rejection.
4. Save the intent message ID printed on stderr. A timeout after dispatch leaves delivery uncertain. Inspect the same chat's queue, turn and reply before resending. A second invocation generates a new message ID.

The destination machine uses its own VS Code sign-in, local endpoint and existing chat. Only the source and these instructions need to travel. [AHP client guide](https://microsoft.github.io/agent-host-protocol/guide/clients) · [Chat channel specification](https://microsoft.github.io/agent-host-protocol/specification/chat-channel.html)
