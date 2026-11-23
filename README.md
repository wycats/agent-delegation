# opus-channel

Send a task to an existing VS Code agent chat, read its status, or steer the current turn from a terminal. The client speaks Microsoft's Agent Host Protocol (AHP) to the same local editor host that owns the chat. An existing authorized session is enough. Each assignment can reuse that session without opening another window.

## On a Mac

1. Run these commands on the Mac that owns the VS Code session, under the same user account. Use a VS Code build with `code agent ps` and `code agent endpoints`, and a recent stable Rust toolchain. If the terminal cannot find `code`, run **Shell Command: Install 'code' command in PATH** in the VS Code Command Palette and reopen the terminal.

   ```sh
   code agent --help
   git clone https://github.com/wycats/opus-channel.git
   cd opus-channel
   cargo test --locked
   cargo build --locked
   ```

2. Find the intended session in the Mac's inventory. If the target is absent, open or resume an agent-host-backed chat in VS Code and check again. Copy its full `resource` URI. List its chats and select one by title and resource.

   ```sh
   code agent ps --all --json
   session='REPLACE_WITH_SESSION_URI'
   ./target/debug/opus-channel chats "$session"
   chat='REPLACE_WITH_CHAT_URI'
   ./target/debug/opus-channel status "$session" "$chat"
   ```

   A successful `status` prints the requested chat URI and its turn state. This is the read-only setup check. Treat the output as conversation data. The inventory and chat catalog can identify projects and chat titles, and `status` includes excerpts from the current and previous turns.

3. To try a round trip, use an idle chat and send a small task. Use `lastModelId` from `status` to reuse the model from its active or most recent turn. If that field is null, use a model ID supported by the session's provider. The provider ID can differ from the display name.

   ```sh
   model='REPLACE_WITH_HOST_MODEL_ID'
   prompt=$(mktemp)
   printf '%s\n' 'Reply with AHP hello.' > "$prompt"
   ./target/debug/opus-channel send "$session" "$chat" "$model" "$prompt"
   ./target/debug/opus-channel status "$session" "$chat"
   ```

   `accepted: true` means the host accepted the message. Check `status` again for the reply. For feedback during an active turn, use `steer` with the same arguments and a new prompt file. Both commands preserve the contents of the UTF-8 file, including newlines.

If discovery fails, check that the `code` on your PATH belongs to the VS Code installation that owns the session and that its `agent --help` lists both discovery commands. If the endpoint is missing or ambiguous, refresh the session inventory and select the intended chat again. To report a problem, share the error string and `code --version` with identifying details removed.

On Windows, run the same Cargo commands and use `.\target\debug\opus-channel.exe` with PowerShell variables. Windows uses a named pipe. macOS and Linux use a filesystem Unix socket advertised by the local VS Code CLI.

## How another client can use the same approach

1. Find the editor host PID that owns the exact session in `code agent ps --all --json`. Read the matching editor socket and connection token from `code agent endpoints` **in process**. That command's raw output contains credentials. Keep it out of logs, issues, and handoff messages.
2. Connect to the local pipe or Unix socket. Upgrade that stream to a WebSocket using the token, initialize AHP 1.0.0, and subscribe to the session URI. Read the chat catalog from the session snapshot and check membership before subscribing to the chosen chat.
3. Subscribe for action echoes before dispatching `chat/pendingMessageSet`. Use `queued` for a task or `steering` for feedback. Match the echo on chat, action type, message UUID and `origin.clientSeq`, then check for host rejection.
4. Save the intent message ID printed on stderr. A timeout after dispatch leaves delivery uncertain. Inspect the same chat's queue, turn and reply before resending. A second invocation generates a new message ID.

The destination machine uses its own VS Code sign-in, local endpoint and existing chat. Only the source and these instructions need to travel. [AHP client guide](https://microsoft.github.io/agent-host-protocol/guide/clients) · [Chat channel specification](https://microsoft.github.io/agent-host-protocol/specification/chat-channel.html)
