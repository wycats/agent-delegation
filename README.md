# opus-channel

Send a task to an existing VS Code agent chat, read its status, or steer the current turn from a terminal. The client speaks Microsoft's Agent Host Protocol (AHP) to the same local editor host that owns the chat. An existing authorized session is enough. Each assignment can reuse that session without opening another window.

## On a Mac

1. Download the archive for your Mac from the [latest release](https://github.com/wycats/opus-channel/releases/latest). In Terminal, `uname -m` prints `arm64` on Apple Silicon and `x86_64` on Intel when running natively. From the directory containing the downloaded archive, run the matching command.

   | Mac | Extract |
   | --- | --- |
   | Apple Silicon | `tar -xzf opus-channel-macos-arm64.tar.gz` |
   | Intel | `tar -xzf opus-channel-macos-x64.tar.gz` |

   Run `./opus-channel --help` and `code agent --help` under the same account as VS Code. Its CLI must list the `ps` and `endpoints` subcommands. If the terminal cannot find `code`, run **Shell Command: Install 'code' command in PATH** in the VS Code Command Palette and reopen the terminal.

2. Find the intended session in the Mac's inventory. If the target is absent, open or resume an agent-host-backed chat in VS Code and check again. Copy its full `resource` URI. List its chats and select one by title and resource.

   ```sh
   code agent ps --all --json
   session='REPLACE_WITH_SESSION_URI'
   ./opus-channel chats "$session"
   chat='REPLACE_WITH_CHAT_URI'
   ./opus-channel status "$session" "$chat"
   ```

   A successful `status` prints the requested chat URI and its turn state. This is the read-only setup check. Treat the output as conversation data. The inventory and chat catalog can identify projects and chat titles, and `status` includes excerpts from the current and previous turns.

3. To try a round trip, use an idle chat and send a small task. Use `lastModelId` from `status` to reuse the model from its active or most recent turn. If that field is null, use a model ID supported by the session's provider. The provider ID can differ from the display name.

   ```sh
   model='REPLACE_WITH_HOST_MODEL_ID'
   prompt=$(mktemp)
   printf '%s\n' 'Reply with AHP hello.' > "$prompt"
   ./opus-channel send "$session" "$chat" "$model" "$prompt"
   ./opus-channel status "$session" "$chat"
   ```

   `accepted: true` means the host accepted the message. Check `status` again for the reply. For feedback during an active turn, use `steer` with the same arguments and a new prompt file. Both commands preserve the contents of the UTF-8 file, including newlines.

If discovery fails, check that the `code` on your PATH belongs to the VS Code installation that owns the session and that its `agent --help` lists both discovery commands. If the endpoint is missing or ambiguous, refresh the session inventory and select the intended chat again. To report a problem, share the error string and `code --version` with identifying details removed.

On Windows x64, extract `opus-channel-windows-x64.zip` and use `.\opus-channel.exe` with PowerShell variables. On Linux x64, extract `opus-channel-linux-x64.tar.gz` and run `./opus-channel`. Windows uses a named pipe. macOS and Linux use a filesystem Unix socket advertised by the local VS Code CLI.

The original Windows client exchanged tasks and replies with a live editor session. The macOS CI test runs the CLI against a fixture over a real Unix socket and verifies the selected chat's status. Steps 2 and 3 above check the installed VS Code build and signed-in session on your Mac. [Build and test runs](https://github.com/wycats/opus-channel/actions/workflows/build.yml)

## Builds and releases

Every pull request targeting `main` and every push to `main` tests, builds and runs the release executable on all four platforms. Download a specific commit's archive from its completed Actions run. For public downloads, use the versioned GitHub Release. Each release includes `SHA256SUMS` for checking the downloaded archives.

To build from a source checkout with stable Rust, run `cargo test --locked` followed by `cargo build --release --locked`. The executable is under `target/release/`.

To publish a version, merge the desired Cargo package version into `main`, then push a matching `vX.Y.Z` tag on that commit. After the builds pass, the workflow publishes the four archives and their checksums together.

## How another client can use the same approach

1. Find the editor host PID that owns the exact session in `code agent ps --all --json`. Read the matching editor socket and connection token from `code agent endpoints` **in process**. That command's raw output contains credentials. Keep it out of logs, issues, and handoff messages.
2. Connect to the local pipe or Unix socket. Upgrade that stream to a WebSocket using the token, initialize AHP 1.0.0, and subscribe to the session URI. Read the chat catalog from the session snapshot and check membership before subscribing to the chosen chat.
3. Subscribe for action echoes before dispatching `chat/pendingMessageSet`. Use `queued` for a task or `steering` for feedback. Match the echo on chat, action type, message UUID and `origin.clientSeq`, then check for host rejection.
4. Save the intent message ID printed on stderr. A timeout after dispatch leaves delivery uncertain. Inspect the same chat's queue, turn and reply before resending. A second invocation generates a new message ID.

The destination machine uses its own VS Code sign-in, local endpoint and existing chat. Only the source and these instructions need to travel. [AHP client guide](https://microsoft.github.io/agent-host-protocol/guide/clients) · [Chat channel specification](https://microsoft.github.io/agent-host-protocol/specification/chat-channel.html)
