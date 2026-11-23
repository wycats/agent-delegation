# Delegate through Google Antigravity (experimental)

For personal Google sign-in, the host is Antigravity CLI (`agy`). Google moved Free, Pro and Ultra individual accounts from the older Gemini CLI to Antigravity. A Gemini model registered in an AHP chat is also available through the AHP channel using that chat's provider account. Choose the channel according to which conversation, permissions and account should own the task. [Google's transition announcement](https://github.com/google-gemini/gemini-cli/discussions/28017) · [Antigravity authentication](https://antigravity.google/docs/install?tab=cli)

Install Node.js 22+ and Google's native `agy` executable. Open `agy` and select your personal Google account. Run `agy -p /usage` to check that account, then `agy models` to find the exact Gemini slug. The adapter checks that the CLI can answer its local usage command before sending your task and strips API-key, custom endpoint, ADC and Vertex environment overrides from its child processes. The usage check does not independently identify the billing tier. Your account's own quota and credit settings apply.

```sh
./agent-delegation google run --model YOUR_GEMINI_SLUG \
  --cwd /path/to/workspace --task /path/to/task.txt --run-dir /path/to/new-run
./agent-delegation google run --resume-from /path/to/new-run \
  --task /path/to/follow-up.txt --run-dir /path/to/next-run
```

Each invocation owns one bounded turn. `--timeout-seconds` defaults to 600. Add `--expect report.md` for each required file. A follow-up inherits the conversation UUID, workspace and selected model from a terminal receipt. Supply a fresh run directory each time. The initial run directory holds the shared ownership record for follow-ups, so keep it in place.

The adapter requests sandbox restrictions and the host's review permission mode. Your installed host determines effective permissions. It checks the selected slug at initialization, a completed user-input step, matching conversation IDs, a successful terminal result and process exit. The host's stream identifies the configured model. It does not independently attest the model on every response or echo a caller-generated message UUID. Use sequential follow-ups after completion. Live steering remains specific to AHP and Claude Code in this release. [Headless protocol and permissions](https://antigravity.google/docs/cli/headless/)

Inspect `completion.json`, `final.txt`, `stderr.txt` and any requested files. A tool error, stderr diagnostic or missing expected file requires review even if the host returned an answer. A file-presence check alone cannot tell whether an existing file changed. A valid terminal conversation ID remains usable for follow-up when a tool or deliverable needs attention. A disconnect or timeout can leave work in progress. Inspect the conversation in Antigravity before retrying. After confirming the owner and its work have stopped, `google reconcile --run-dir DIR --note TEXT` records that inspection and releases the retained lock. It never repeats a task. Use `google review --run-dir DIR --note TEXT` to save your assessment separately from the host receipt. Keep run directories private, particularly on Windows where access follows the enclosing directory ACL.

The event contract and exact-ID follow-up are covered by local fixtures. The native Windows CLI was checked at version 1.5.15. A signed-in end-to-end test remains outstanding. To check your installation, ask for a unique word, then resume the run and ask for the word without including it again. Confirm both receipts have the same conversation UUID.
