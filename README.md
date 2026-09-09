# Pika

**One home for your coding agents. A way for their work to connect.**

Bring your **Codex, Claude Code, and OpenCode** conversations into one terminal
board. See what needs you, return to work by name, and let agents consult relevant
experience from your other projects.

https://github.com/user-attachments/assets/91c07810-e304-4ab5-a533-e2b05351a73e

*Pika in 60 seconds · Illustrative demo*

- **Know where you're needed.** Distinguish requests for your input from work in
  progress and completed results.
- **Pick up where you left off.** Open a conversation by name or from the board,
  in its original agent interface.
- **Connect relevant work.** Let your agent find a project expert and consult its
  existing context while the original conversation keeps working.

## Get started

Pika runs on **macOS and Linux**. You need **tmux** and at least one agent CLI
installed and signed in: Codex, Claude Code, or OpenCode (1.18.21+).
One machine is enough.

Install the current alpha, **v0.5.0a3**:

```bash
curl -fsSL https://github.com/ayushjainr/pikamux/releases/download/v0.5.0a3/install.sh | bash -s -- --version v0.5.0a3
```

The [installer](https://github.com/ayushjainr/pikamux/releases/download/v0.5.0a3/install.sh)
handles Python and offers setup. It asks before changing shell or agent settings
and backs up existing configuration. See the [installation guide](docs/installing.md)
for prerequisites and troubleshooting.

1. **Choose your conversations.** Review setup and select an existing conversation.
   Run `pika setup` if you skipped onboarding. In Codex, review and trust the
   integration through `/hooks`.
2. **Open the board.** Run `pika`, select a conversation, and press **Enter**.
   Use **Ctrl-b**, then **d**, to detach and return to Pika while the agent keeps
   running.

## Everyday use

```bash
pika          # Open the board
pika NAME     # Open or create a named conversation
pika setup    # Review hooks and agent integrations
pika update   # Review and install an available update
```

Replace `NAME` with your conversation's name. Pika opens it if it exists, asks
you to choose when the match is ambiguous, or creates a conversation for a new
name. If an agent is already running elsewhere, Pika explains the safe handover
steps instead of opening a duplicate.

| On the board | Action |
| --- | --- |
| **↑↓** or **j/k**, then **Enter** | Select and open a conversation |
| **/** | Filter conversations |
| **p** | Peek at a result |
| **x** | Stop watching; keep the agent running and its history |
| **q** | Leave the board |

Installer-managed copies show update notices on the board. Press **U** to review
an update; installation requires your approval and leaves running agents alone.
See [installation and updates](docs/installing.md) for other install methods.

## Let your agents consult prior work

The included **agent-convo** skill gives your agents access to relevant project
experience. Expert cards describe each conversation's scope, topics, and artifacts
so an agent can find the right context without searching every transcript.

For example, a dashboard agent deciding how to display missing data can consult
the pipeline project about why it preserved gaps instead of filling them. The
dashboard agent checks the referenced test, then applies the decision to its chart.
The exchange happens in a separate consultation; its questions and answers do not
enter the original conversation.

When you're ready, follow the [first-consultation guide](docs/first-consultation.md)
to prepare one expert card. Then give your agent a task and tell it:

> Use agent-convo when relevant prior work would help. Check the evidence and
> continue the task.

Card searches and ordinary board views make no model calls. Card interviews and
consultations use your provider quota.

**Compatibility:** v0.5.0a3 cannot consult some newer Codex conversations.
Claude consultations require CLI 2.1.228+. Check the
[consultation compatibility notes](docs/first-consultation.md#release-compatibility)
before your first consultation.

## Across your machines

Add trusted SSH or Tailscale machines to monitor and consult their conversations
from the same board. Pika must be installed on each machine; you choose which
machines to connect. Start locally and add others when you need them.

See the [machine setup guide](docs/guide.md#multiple-machines).
Native Windows agent hosting is not supported; the Windows client bridge is
experimental.

## Privacy

Pika stores operational metadata and expert cards locally. Consultations are
separate from the original conversation, but your model provider still processes
them. Side-session storage and cleanup vary by provider and version. Returning to
a conversation requires its original host and provider history to remain available.
See [security and privacy](SECURITY.md) for the boundaries.

## Documentation

- [Operating guide](docs/guide.md)
- [First consultation](docs/first-consultation.md)
- [Installation and updates](docs/installing.md)
- [Architecture](DESIGN.md) · [Contributing](CONTRIBUTING.md)

Pika is an independent open-source project, not affiliated with OpenAI, Anthropic,
or OpenCode.

Maintained by [Ayush Jain](https://ayushjainr.com). [MIT license](LICENSE).
