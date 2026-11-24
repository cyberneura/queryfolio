# Agent chat (Claude Code / Codex)

With an `agent:` section in the config, the **Chat** pane and the **AI** field
above the editor talk to a coding agent — [Claude Code](https://claude.com/claude-code)
or [Codex](https://developers.openai.com/codex) — instead of the built-in
assistant. Queryfolio starts the `claude` / `codex` command installed on your
machine as a child process and uses the login that command already has (your
subscription). No API key is configured in Queryfolio. See
[settings.md](settings.md#coding-agent-agent) for the keys.

## What the agent gets

- **A working directory.** By default the query file folder of the selected
  connection (it holds your `.sql` files and `_queryfolio.md`, which describes
  the connection without secrets). `agent.directory`, or
  `agent.directories.<connection>`, points it somewhere else — typically a
  clone of the application's repository, so that the agent can read the models
  and migrations while it writes queries. The query file folder then stays
  reachable (`--add-dir` for Claude Code, a writable root for Codex). The
  directory can also be changed for one chat in the pane's header.
- **The editor context, with every message**: the connection name and engine,
  the active database, the absolute path of the query file that is open, and
  the statement at the cursor. The first message of a chat also carries the
  table and column names (up to 8,000 characters). The open file is saved
  before the message is sent, so the agent reads what the editor shows.
- It does **not** get host names, users, passwords or query results, and it has
  no database connection: it edits files, and you run the SQL in Queryfolio.
  (An agent that is allowed to run commands could of course start a database
  client itself; that goes through the approval described below.)

When the agent edits the open query file, the change appears in the editor: the
external-change watcher reloads the file when a turn ends (unsaved edits made in
the meantime are merged, or flagged as a conflict).

## Approvals

Approvals are never bypassed (`--dangerously-skip-permissions` and the like are
not used, and a test keeps them out of the command lines).

- **Claude Code** routes its permission prompts to the Chat pane
  (`--permission-prompt-tool stdio`), where each request is shown with
  **Allow** / **Deny**. With `agent.auto_accept_edits` (the default) it runs in
  `acceptEdits` mode: file edits inside the working directory and the query
  file folder are applied without asking. Everything else — commands, network
  access — still asks.
- **Codex** keeps the sandbox and approval policy of your `~/.codex/config.toml`
  and asks through the same pane.
- The **AI field above the editor** runs the agent once, without a
  conversation, and nobody is there to answer a prompt. Claude Code is limited
  to `Read` / `Edit` / `Write` / `Glob` / `Grep` (no commands); Codex runs with
  `--sandbox workspace-write`. The agent's final message is shown as a
  notification, and **Stop** ends the run (edits already made stay).

## Conversations

One conversation per connection. Switching connections shows that connection's
conversation; the agents keep running until **New chat**, a config reload, or
quitting Queryfolio (they are sent SIGTERM first so that they can stop their own
child processes, then killed). Messages sent while the agent is answering are
queued and sent in order; **Stop** holds the queue until **Resume**.

## Where the code comes from

The protocol engines in `src/lib/agentChat/beckwell/` (Claude Code stream-json,
Codex app-server JSON-RPC, the common chat items, questions, sanitized Markdown,
code copying) are vendored from Cyberneura's Beckwell (MIT) by way of
[Astragal](https://github.com/cyberneura/astragal) (revision
`fd0411e0b5a4738b7009b3151fbee7acf0fbb748`, `src/chat/beckwell/`), together with
`queue.ts` and `scroll.ts`. `src-tauri/src/agent_process.rs` adapts Astragal's
process supervision.

Local changes: comments translated to English (this repository's rule), Codex
identifies the client as Queryfolio, and the process layer resolves the working
directory and the command line in the backend from the connection (the webview
does not pass a command line), goes through the user's login shell on every
launch, and adds the one-shot run. Astragal's DOM renderer (`pane.ts`) is
replaced by `src/lib/agentChat/conversations.svelte.ts` (state) and
`src/lib/components/AgentChatPane.svelte`. History selection and model
switching are not exposed.

When updating the vendored files, keep these changes, run `pnpm test` and
`pnpm check`, and exercise both providers in the app.
