# Delegate through Claude Code

Install Node.js 22+ and the official Claude Code CLI. Sign in using your Claude account. The runner checks `claude auth status` for a first-party Claude.ai session before sending the assignment. Choose an exact Claude Code model ID. Each active run owns one streaming CLI process and has its own directory for messages, receipts and results.

From the directory containing the executable, substitute absolute paths for the workspace, task file and a fresh run directory. The task file contains your instructions. A relative `--expect` path is resolved against the workspace.

```sh
./agent-delegation claude run --model claude-fable-5-2 \
  --cwd /path/to/workspace --task /path/to/assignment.txt \
  --run-dir /path/to/run-1 --expect review.md
```

The owner stays running until the delegate exits. From a second terminal you can inspect the run, send a correction, or wait for it.

```sh
./agent-delegation claude status --run-dir /path/to/run-1
./agent-delegation claude steer --run-dir /path/to/run-1 --task /path/to/feedback.txt
./agent-delegation claude wait --run-dir /path/to/run-1
```

The runner verifies the model and session in the event stream, message replay, successful result, clean exit, permission outcomes and declared output files. Read `final.txt`, inspect the files, and record your own assessment. An existing file passing the presence check still needs review.

```sh
./agent-delegation claude review --run-dir /path/to/run-1 --note "Read review.md and checked the findings."
./agent-delegation claude run --resume-from /path/to/run-1 \
  --task /path/to/follow-up.txt --run-dir /path/to/run-2
```

Resume inherits the exact session, workspace, model and tool configuration. The default tool set is Read and Write. Use `agent-delegation claude --help` for tool, time, turn and budget controls. If the owner dies, inspect `status` and the workspace before using the runner's `reconcile` command. Run directories contain task text, responses and events. Keep them in a location private to your account. [Claude Code CLI](https://code.claude.com/docs/en/cli-reference)
